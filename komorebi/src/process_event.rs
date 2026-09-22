use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use crossbeam_utils::atomic::AtomicConsume;
use parking_lot::Mutex;

use crate::core::OperationDirection;
use crate::core::Rect;
use crate::core::Sizing;
use crate::core::WindowContainerBehaviour;

use crate::CURRENT_VIRTUAL_DESKTOP;
use crate::DefaultLayout;
use crate::FLOATING_APPLICATIONS;
use crate::HIDDEN_HWNDS;
use crate::Layout;
use crate::NotificationEvent;
use crate::PINNED_FLOATING_APPLICATIONS;
use crate::REGEX_IDENTIFIERS;
use crate::TRAY_AND_MULTI_WINDOW_IDENTIFIERS;
use crate::VirtualDesktopNotification;
use crate::Window;
use crate::WorkspaceLayerFocusBehaviour;
use crate::apply_worker::ApplyWorker;
use crate::border_manager;
use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_WIDTH;
use crate::current_virtual_desktop;
use crate::has_subscribers;
use crate::notify_subscribers;
use crate::splash;
use crate::splash::mdm_enrollment;
use crate::stackbar_manager;
use crate::state::State;
use crate::transparency_manager;
use crate::window::RuleDebug;
use crate::window::should_act;
use crate::window_manager::WindowManager;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;
use crate::winevent::WinEvent;
use crate::workspace::WorkspaceLayer;

#[tracing::instrument]
pub fn listen_for_events(wm: Arc<Mutex<WindowManager>>) {
    let receiver = wm.lock().incoming_events.clone();

    std::thread::spawn(|| {
        loop {
            if let Ok((mdm, server)) = mdm_enrollment() {
                #[allow(clippy::collapsible_if)]
                if mdm && splash::should().map(|f| f.into()).unwrap_or(true) {
                    let mut args = vec!["splash".to_string()];
                    if let Some(server) = server {
                        if !server.trim().is_empty() {
                            args.push(server);
                        }
                    }

                    let _ = Command::new("komorebic").args(&args).spawn();
                }
            }

            std::thread::sleep(std::time::Duration::from_secs(14400));
        }
    });

    std::thread::spawn(move || {
        tracing::info!("listening");
        loop {
            if let Ok(event) = receiver.recv() {
                let mut guard = wm.lock();
                match guard.process_event(event) {
                    Ok(()) => {}
                    Err(error) => {
                        if cfg!(debug_assertions) {
                            tracing::error!("{:?}", error)
                        } else {
                            tracing::error!("{}", error)
                        }
                    }
                }
            }
        }
    });
}

/// Determines whether a `MoveResizeEnd` should be treated as a move of a
/// container rather than a resize or a plain click.
///
/// A "move" is a pure translation: the container keeps its width and height
/// (`Rect::right`/`Rect::bottom` are dimensions, not absolute coordinates).
///
/// A plain click on a window (e.g. on its border) fires a `MoveResizeEnd`
/// without any actual movement. This must not be treated as a move, otherwise
/// the container would get swapped with whichever container sits under the
/// cursor (typically its neighbour) even though the user only clicked on the
/// border.
fn is_container_translation_move(resize: Rect, moved_across_monitors: bool) -> bool {
    if moved_across_monitors {
        return true;
    }

    let nothing_changed =
        resize.left == 0 && resize.top == 0 && resize.right == 0 && resize.bottom == 0;

    !nothing_changed && resize.right == 0 && resize.bottom == 0
}

impl WindowManager {
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    #[tracing::instrument(skip(self, event), fields(event = event.title(), winevent = event.winevent(), hwnd = event.hwnd()))]
    pub fn process_event(&mut self, event: WindowManagerEvent) -> eyre::Result<()> {
        if self.is_paused {
            tracing::trace!("ignoring while paused");
            return Ok(());
        }

        let mut rule_debug = RuleDebug::default();

        let should_manage = event.window().should_manage(Some(event), &mut rule_debug)?;

        // All event handlers below this point should only be processed if the event is
        // related to a window that should be managed by the WindowManager.
        if !should_manage {
            let mut transparency_override = false;

            if transparency_manager::TRANSPARENCY_ENABLED.load_consume() {
                for m in self.monitors() {
                    for w in m.workspaces() {
                        let event_hwnd = event.window().hwnd;

                        let visible_hwnds = w
                            .visible_windows()
                            .iter()
                            .flatten()
                            .map(|w| w.hwnd)
                            .collect::<Vec<_>>();

                        let contains_managed_window = w.contains_managed_window(event_hwnd);

                        // this is for an old stackbar clicking fix
                        if contains_managed_window && !visible_hwnds.contains(&event_hwnd) {
                            transparency_override = true;
                        }

                        // but we always want to handle a minimize event when transparency overrides
                        // are applied
                        if !transparency_override
                            && contains_managed_window
                            && matches!(event, WindowManagerEvent::Minimize(_, _))
                        {
                            transparency_override = true;
                        }
                    }
                }
            }

            if !transparency_override {
                if rule_debug.matches_ignore_identifier.is_some() {
                    border_manager::send_notification(Option::from(event.hwnd()));
                }

                return Ok(());
            }
        }

        let mut last_known_virtual_desktop_id = CURRENT_VIRTUAL_DESKTOP.lock();

        if let Some(virtual_desktop_id) = &self.virtual_desktop_id {
            let latest_virtual_desktop_id = current_virtual_desktop();
            if let Some(id) = latest_virtual_desktop_id {
                // if we are on the vd associated with komorebi
                let should_retile = id == *virtual_desktop_id
                    // and we came from a vd not associated with komorebi
                    && (*last_known_virtual_desktop_id).clone().unwrap_or_default() != id;

                *last_known_virtual_desktop_id = Some(id.clone());
                if id != *virtual_desktop_id {
                    tracing::info!(
                        "ignoring events and commands while not on virtual desktop {:?}",
                        virtual_desktop_id
                    );

                    // TODO: when returning from another VD to the VD associated with komorebi
                    // if borders are enabled, they will not be drawn again until the user interacts
                    // with the workspace or forces a retile
                    border_manager::destroy_all_borders()?;

                    // to be consumed by integrating gui applications like bars to know
                    // when to hide visual components which don't make sense when not on
                    // komorebi's associated virtual desktop
                    tracing::debug!(
                        "notifying subscribers that we have left komorebi's associated virtual desktop"
                    );
                    notify_subscribers(
                        NotificationEvent::VirtualDesktop(
                            VirtualDesktopNotification::LeftAssociatedVirtualDesktop,
                        ),
                        true,
                        || self.as_ref().into(),
                    )?;

                    return Ok(());
                }

                if should_retile {
                    self.retile_all(true)?;

                    // to be consumed by integrating gui applications like bars to know
                    // when to show visual components associated with komorebi's virtual
                    // desktop
                    tracing::debug!(
                        "notifying subscribers that we are back on komorebi's associated virtual desktop"
                    );
                    notify_subscribers(
                        NotificationEvent::VirtualDesktop(
                            VirtualDesktopNotification::EnteredAssociatedVirtualDesktop,
                        ),
                        true,
                        || self.as_ref().into(),
                    )?;
                }
            }
        }

        #[allow(clippy::useless_asref)]
        // We don't have From implemented for &mut WindowManager
        let initial_state = State::from(self.as_ref());

        // Make sure we have the most recently focused monitor from any event
        match event {
            WindowManagerEvent::FocusChange(_, window)
            | WindowManagerEvent::Show(_, window)
            | WindowManagerEvent::MoveResizeEnd(_, window) => {
                if let Some(monitor_idx) = self.monitor_idx_from_window(window) {
                    // This is a hidden window apparently associated with COM support mechanisms (based
                    // on a post from http://www.databaseteam.org/1-ms-sql-server/a5bb344836fb889c.htm)
                    //
                    // The hidden window, OLEChannelWnd, associated with this class (spawned by
                    // explorer.exe), after some debugging, is observed to always be tied to the primary
                    // display monitor, or (usually) monitor 0 in the WindowManager state.
                    //
                    // Due to this, at least one user in the Discord has witnessed behaviour where, when
                    // a MonitorPoll event is triggered by OLEChannelWnd, the focused monitor index gets
                    // set repeatedly to 0, regardless of where the current foreground window is actually
                    // located.
                    //
                    // This check ensures that we only update the focused monitor when the window
                    // triggering monitor reconciliation is known to not be tied to a specific monitor.
                    if let Ok(class) = window.class()
                        && class != "OleMainThreadWndClass"
                        && self.focused_monitor_idx() != monitor_idx
                    {
                        self.focus_monitor(monitor_idx)?;
                    }
                }
            }
            _ => {}
        }

        self.enforce_workspace_rules()?;

        if matches!(event, WindowManagerEvent::MouseCapture(..)) {
            tracing::trace!(
                "only reaping orphans and enforcing workspace rules for mouse capture event"
            );
            return Ok(());
        }

        match event {
            WindowManagerEvent::Raise(window) => {
                window.focus(false)?;
                self.has_pending_raise_op = false;
            }
            WindowManagerEvent::DragDrop(_, window) => {
                // A completed drop is treated as a full focus transfer to the target window: it
                // was the surface the user just interacted with. Activating it makes the OS fire
                // a foreground WinEvent, which the normal FocusChange reconciliation then processes
                // (focusing the container/float and honouring workspace_layer_focus_behaviour), and
                // the transparency pass at the end of this event restores its opacity through the
                // ordinary focused-window rule. Only act on managed windows on the focused
                // workspace: dropping onto the desktop, taskbar or an unmanaged surface must not
                // steal focus.
                if let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
                    && should_manage
                    && self.focused_workspace()?.contains_window(window.hwnd)
                {
                    window.focus(false)?;
                }
            }
            WindowManagerEvent::Destroy(_, window) | WindowManagerEvent::Unmanage(window) => {
                // A destroyed/recycled hwnd must never be served the previous
                // window's cached exe/class from the metadata cache.
                WindowsApi::invalidate_window_metadata(window.hwnd);

                if self.focused_workspace()?.contains_window(window.hwnd) {
                    self.focused_workspace_mut()?.remove_window(window.hwnd)?;

                    let keep_monocle = self.keep_monocle_on_window_close
                        && self
                            .focused_workspace()?
                            .monocle_container
                            .as_ref()
                            .is_some_and(|m| !m.windows().is_empty());

                    if keep_monocle {
                        self.update_focused_workspace(true, true)?;
                    } else {
                        // Restore tiling containers when not keeping monocle
                        if let Some(monocle) = &self.focused_workspace()?.monocle_container {
                            if !monocle.windows().is_empty() {
                                for c in self.focused_workspace()?.containers() {
                                    c.restore();
                                }
                            }
                        }
                        self.update_focused_workspace(false, false)?;
                    }

                    let mut already_moved_window_handles = self.already_moved_window_handles.lock();

                    already_moved_window_handles.remove(&window.hwnd);
                }

                // A pinned window is not in any workspace's list, so it was not
                // removed above; prune it from the monitor's pinned set.
                for monitor in self.monitors_mut() {
                    monitor.unpin_floating_window(window.hwnd);
                }
            }
            WindowManagerEvent::Minimize(_, window) => {
                // During transient display connection changes (e.g. monitor
                // briefly disconnecting and reconnecting), Windows may fire
                // SystemMinimizeStart for windows on the affected monitor.
                // We must not treat these OS-initiated minimizes as user
                // actions, otherwise the window gets removed from the
                // workspace and the reconciliator cannot restore it.
                if crate::monitor_reconciliator::display_change_in_progress(
                    std::time::Duration::from_secs(10),
                ) {
                    tracing::debug!(
                        "ignoring minimize during display connection change for hwnd: {}",
                        window.hwnd
                    );
                } else {
                    let mut hide = false;

                    {
                        let programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
                        if !programmatically_hidden_hwnds.contains(&window.hwnd) {
                            hide = true;
                        }
                    }

                    if hide {
                        self.focused_workspace_mut()?.remove_window(window.hwnd)?;
                        self.update_focused_workspace(false, false)?;
                    }
                }
            }
            WindowManagerEvent::Hide(_, window) => {
                let mut hide = false;
                // Some major applications unfortunately send the HIDE signal when they are being
                // minimized or destroyed. Applications that close to the tray also do the same,
                // and will have is_window() return true, as the process is still running even if
                // the window is not visible.
                {
                    let tray_and_multi_window_identifiers =
                        TRAY_AND_MULTI_WINDOW_IDENTIFIERS.lock();
                    let regex_identifiers = REGEX_IDENTIFIERS.lock();

                    let title = &window.title()?;
                    let exe_name = &window.exe()?;
                    let class = &window.class()?;
                    let path = &window.path()?;

                    // We don't want to purge windows that have been deliberately hidden by us, eg. when
                    // they are not on the top of a container stack.
                    let programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
                    let should_act = should_act(
                        title,
                        exe_name,
                        class,
                        path,
                        &tray_and_multi_window_identifiers,
                        &regex_identifiers,
                    )
                    .is_some();

                    if !window.is_window()
                        || (should_act && !programmatically_hidden_hwnds.contains(&window.hwnd))
                    {
                        hide = true;
                    }
                }

                if hide {
                    self.focused_workspace_mut()?.remove_window(window.hwnd)?;
                    self.update_focused_workspace(false, false)?;
                }

                let mut already_moved_window_handles = self.already_moved_window_handles.lock();

                already_moved_window_handles.remove(&window.hwnd);
            }
            WindowManagerEvent::FocusChange(_, window) => {
                // Check if this focus change is for a window on the currently focused workspace.
                // FocusChange events can arrive asynchronously (e.g., generated by a previous
                // workspace switch while the ring has already moved), causing us to update
                // last_focused_hwnd on the wrong workspace.
                let focused_monitor_idx = self.focused_monitor_idx();
                let focused_workspace_idx = self.focused_workspace_idx()?;

                let window_owner = self.known_hwnds.get(&window.hwnd).copied();
                let on_current_workspace = window_owner
                    .map(|(m, w)| m == focused_monitor_idx && w == focused_workspace_idx)
                    .unwrap_or(false);

                // A window pinned across this monitor's workspaces is part of
                // the Floating overlay on every workspace, so focusing it is
                // treated as focusing a floating window on the current
                // workspace, regardless of which workspace it was homed on.
                let is_pinned = self
                    .focused_monitor()
                    .map(|monitor| monitor.is_pinned(window.hwnd))
                    .unwrap_or(false);

                if !on_current_workspace && !is_pinned {
                    if let Some((m_idx, w_idx)) = window_owner {
                        tracing::debug!(
                            hwnd = window.hwnd,
                            window_monitor = m_idx,
                            window_workspace = w_idx,
                            focused_monitor = focused_monitor_idx,
                            focused_workspace = focused_workspace_idx,
                            "FocusChange for window on non-focused workspace, \
                             updating last_focused_hwnd only"
                        );

                        if let Some(workspace) = self
                            .monitors_mut()
                            .get_mut(m_idx)
                            .and_then(|m| m.workspaces_mut().get_mut(w_idx))
                        {
                            let _ = workspace.focus_container_by_window(window.hwnd);
                        }
                    }
                    return Ok(());
                }

                // Ignore stale FocusChange events: if the window is no longer the foreground,
                // this event is outdated (e.g., generated by a hide-induced auto-promotion
                // during a workspace switch) and should not overwrite last_focused_hwnd.
                if !window.is_focused() {
                    return Ok(());
                }

                // don't want to trigger the full workspace updates when there are no managed
                // containers - this makes floating windows on empty workspaces go into very
                // annoying focus change loops which prevents users from interacting with them
                if !matches!(
                    self.focused_workspace()?.layout,
                    Layout::Default(DefaultLayout::Scrolling)
                ) && !self.focused_workspace()?.containers().is_empty()
                {
                    self.update_focused_workspace(self.mouse_follows_focus, false)?;
                }

                let previous_layer = self.focused_workspace()?.layer;

                // Whether the focused window is one of this workspace's own floating
                // windows; used to skip re-tapping the overlay over it.
                let focused_own_float;

                // Set when the focus-driven flip to Tiling keeps the floating
                // overlay intact (`WorkspaceLayerFocusBehaviour::SwitchLayerOverlay`):
                // instead of re-ordering the whole window stack, only the focused
                // tiling window is raised above the overlay.
                let mut keep_overlay_intact = false;

                // Copied out before the mutable workspace borrow (Copy enum).
                let focus_behaviour = self.workspace_layer_focus_behaviour;

                // Whether this focus change is the fallout of a komorebi-initiated
                // focus operation (layer toggle, workspace/monitor switch); such
                // events must not flip a Floating workspace to Tiling.
                let flip_suppressed = self.is_layer_flip_suppressed();

                {
                    let workspace = self.focused_workspace_mut()?;
                    let floating_window_idx = workspace
                        .floating_windows()
                        .iter()
                        .position(|w| w.hwnd == window.hwnd);
                    focused_own_float = floating_window_idx.is_some();

                    match floating_window_idx {
                        None => {
                            if let Some(w) = &workspace.maximized_window
                                && w.hwnd == window.hwnd
                            {
                                return Ok(());
                            }

                            if is_pinned {
                                if flip_suppressed {
                                    // Komorebi-initiated focus fallout (layer toggle,
                                    // workspace/monitor switch): just-activated pinned
                                    // windows that komorebi itself surfaced must not
                                    // clobber the "last used float" memory nor flip
                                    // the layer. A genuine user activation during this
                                    // window is a rare race and loses.
                                    tracing::debug!(
                                        hwnd = window.hwnd,
                                        "pin focus: skipping last-focused recording (komorebi-initiated focus fallout)"
                                    );
                                } else {
                                    tracing::info!(
                                        hwnd = window.hwnd,
                                        "pin focus: recording pinned window as last focused float"
                                    );
                                    // A pinned float is focused: remember it as the
                                    // layer's last-used window and surface the
                                    // Floating overlay, mirroring an own floating
                                    // window.
                                    workspace.last_focused_floating_hwnd = Some(window.hwnd);
                                    workspace.last_focused_cycle_window_hwnd = Some(window.hwnd);
                                    workspace.layer = WorkspaceLayer::Floating;
                                }
                            } else {
                                if let Some(monocle) = &workspace.monocle_container {
                                    if let Some(window) = monocle.focused_window() {
                                        window.focus(false)?;
                                    }
                                } else {
                                    tracing::debug!(
                                        hwnd = window.hwnd,
                                        "FocusChange: updating last_focused_hwnd on current workspace"
                                    );
                                    workspace.focus_container_by_window(window.hwnd)?;
                                }

                                match focus_behaviour {
                                    WorkspaceLayerFocusBehaviour::RespectLock => {
                                        if !workspace.layer_lock && !flip_suppressed {
                                            workspace.layer = WorkspaceLayer::Tiling;
                                        }
                                    }
                                    WorkspaceLayerFocusBehaviour::SwitchLayer
                                    | WorkspaceLayerFocusBehaviour::SwitchLayerOverlay => {
                                        if !flip_suppressed {
                                            workspace.layer = WorkspaceLayer::Tiling;
                                            workspace.layer_lock = false;
                                            if matches!(
                                                focus_behaviour,
                                                WorkspaceLayerFocusBehaviour::SwitchLayerOverlay
                                            ) {
                                                keep_overlay_intact = true;
                                            }
                                        }
                                    }
                                }

                                if matches!(
                                    self.focused_workspace()?.layout,
                                    Layout::Default(DefaultLayout::Scrolling)
                                ) && !self.focused_workspace()?.containers().is_empty()
                                {
                                    self.update_focused_workspace(self.mouse_follows_focus, false)?;
                                }
                            }
                        }
                        Some(idx) => {
                            if workspace.focus_floating_window(idx) {
                                workspace.layer = WorkspaceLayer::Floating;
                            }
                        }
                    }
                }

                // If the focus event flipped the workspace layer, re-establish the
                // layer stack so the newly focused layer is raised above its base
                // (e.g. floating windows above the tiling base after focusing one).
                if previous_layer != self.focused_workspace()?.layer {
                    if keep_overlay_intact {
                        // Keep the floating overlay entirely intact: both the own
                        // floats and the pinned band keep their relative z-order
                        // (pins stay below the floats), and only the focused
                        // tiling window that triggered the flip is synchronously
                        // raised and re-activated on top of the overlay.
                        window.raise_sync()?;
                        WindowsApi::raise_and_focus_window(window.hwnd)?;
                    } else {
                        self.focused_monitor()
                            .ok_or_eyre("there is no monitor with this idx")?
                            .enforce_layer_stack()?;
                    }
                }

                // The overlay maintenance below re-taps the focused float, the
                // floating band and the pinned band via the transient TopMost-
                // band raise. The OS can answer each raise with a new
                // SystemForeground event for the raised window, and re-running
                // the raise on every single FocusChange then feeds a
                // self-sustaining focus-change storm (a ~50ms cascade that
                // renders the floated window unresponsive). Throttling the
                // maintenance collapses that cascade to one pass per interval:
                // the overlay's z-order does not degrade on a no-op focus event
                // in between, so skipping the redundant re-tap is safe.
                let overlay_maintenance_suppressed = self.is_overlay_maintenance_suppressed();
                // While a komorebi-initiated operation (layer toggle, float
                // toggle, pin toggle, workspace/monitor switch) is in its
                // suppression window, the command itself already asserted the
                // final focus and z-order. The maintenance is skipped in that
                // window so the DWM fallout events it generates (tiled-window
                // reparent echoes) cannot re-raise the pinned band ~1s later
                // and reactivate a pin over the just-focused window.

                // Skip the re-assert when the focused window is itself part of the
                // Floating overlay (an own floating window or a pinned window):
                // tapping the overlay above it would visually cover the window that
                // was just focused (e.g. when cycle-focusing between floating
                // windows). The overlay is still re-tapped over an active tiled
                // window so it stays in the Floating band.
                {
                    let focused_workspace = self.focused_workspace()?;
                    let layer = focused_workspace.layer;
                    let floats = focused_workspace
                        .floating_windows()
                        .iter()
                        .copied()
                        .collect::<Vec<_>>();

                    if !overlay_maintenance_suppressed
                        && !flip_suppressed
                        && layer == WorkspaceLayer::Floating
                        && !focused_own_float
                        && !is_pinned
                    {
                        // Re-tap the whole Floating overlay (this workspace's own
                        // floating windows, then the pinned band of the other
                        // workspaces on this monitor) so it remains above the tiling
                        // base across focus changes that settle on a tiling window.
                        // This mirrors why the pinned band is reliable: it is
                        // re-asserted here on every such focus change instead of
                        // relying on a one-shot raise during the layer toggle.
                        // The raises are posted to the apply worker so a Not
                        // Responding window can never block the window-manager thread.
                        ApplyWorker::raise_above_active(floats);
                        self.focused_monitor()
                            .ok_or_eyre("there is no monitor with this idx")?
                            .raise_pinned_windows();
                        self.note_overlay_maintenance();
                    }

                    // The focused window is one of this workspace's own floating
                    // windows: lift it above the pinned band regardless of what raised
                    // the pins first (a re-tap over a focused tiled window, the layer
                    // flip, or the forward toggle), so the last-focused window is never
                    // covered by pins.
                    if !overlay_maintenance_suppressed
                        && !flip_suppressed
                        && layer == WorkspaceLayer::Floating
                        && focused_own_float
                    {
                        // Posted after the overlay re-tap above so the FIFO worker
                        // lifts the focused float last, above the pinned band.
                        ApplyWorker::raise_above_active(vec![window]);
                        self.note_overlay_maintenance();
                    }
                }

                if self.capture_native_maximize(window)? {
                    return Ok(());
                }
            }
            WindowManagerEvent::Show(_, window)
            | WindowManagerEvent::Manage(window)
            | WindowManagerEvent::Uncloak(_, window) => {
                // Pinned windows live outside every workspace list
                // (monitor.pinned_floating alone), so a Show/Uncloak event from
                // their komorebi-driven hide/restore is unrecognisable as an
                // already-managed window here and would be treated as a brand-new
                // window: re-added to a workspace, focused and re-laid-out. That
                // focuses the pin (flipping the layer to Floating), spawns
                // phantom tiles and feeds a hide/show event storm. Their
                // visibility is owned exclusively by apply_pin_visibility, so
                // management events for them are dropped.
                let is_pinned = self
                    .monitors()
                    .iter()
                    .any(|monitor| monitor.is_pinned(window.hwnd));

                if is_pinned {
                    return Ok(());
                }

                if matches!(event, WindowManagerEvent::Uncloak(_, _))
                    && self.consume_expected_uncloak(window.hwnd)
                {
                    tracing::info!("ignoring uncloak after monocle move by mouse across monitors");
                } else {
                    let focused_monitor_idx = self.focused_monitor_idx();
                    let focused_workspace_idx =
                        self.focused_workspace_idx_for_monitor_idx(focused_monitor_idx)?;

                    let mut needs_reconciliation = None;

                    // There are some applications such as Firefox where, if they are focused when a
                    // workspace switch takes place, it will fire an additional Show event, which will
                    // result in them being associated with both the original workspace and the workspace
                    // being switched to. This loop is to try to ensure that we don't end up with
                    // duplicates across multiple workspaces, as it results in ghost layout tiles.
                    let mut proceed = true;

                    // Check for potential `alt-tab` event
                    if matches!(
                        event,
                        WindowManagerEvent::Uncloak(_, _) | WindowManagerEvent::Show(_, _)
                    ) {
                        needs_reconciliation = self.needs_reconciliation(window)?;

                        if let Some((m_idx, ws_idx)) = needs_reconciliation {
                            self.perform_reconciliation(window, (m_idx, ws_idx))?;

                            // Since there was a reconciliation after an `alt-tab`, that means this
                            // window is already handled by komorebi so we shouldn't proceed with
                            // adding it as a new window.
                            proceed = false;
                        }
                    }

                    if let Some((m_idx, w_idx)) = self.known_hwnds.get(&window.hwnd)
                        && let Some(focused_workspace_idx) = self
                            .monitors()
                            .get(*m_idx)
                            .map(|m| m.focused_workspace_idx())
                        && *m_idx != self.focused_monitor_idx()
                        && *w_idx != focused_workspace_idx
                    {
                        tracing::debug!(
                            "ignoring show event for window already associated with another workspace"
                        );

                        window.hide();
                        proceed = false;
                    }

                    // after enforce_workspace_rules() has run, check if window exists in ANY workspace
                    // to prevent duplication when workspace rules move windows across workspaces
                    if proceed {
                        let window_already_managed = self
                            .monitors()
                            .iter()
                            .flat_map(|m| m.workspaces())
                            .any(|ws| ws.contains_window(window.hwnd));

                        if window_already_managed {
                            tracing::debug!(
                                "skipping window addition, already managed after workspace rule enforcement"
                            );

                            proceed = false;
                        }
                    }

                    if proceed {
                        let behaviour = self.window_management_behaviour(
                            focused_monitor_idx,
                            focused_workspace_idx,
                        );
                        let workspace = self.focused_workspace_mut()?;
                        let workspace_contains_window = workspace.contains_window(window.hwnd);
                        let monocle_container = workspace.monocle_container.clone();
                        let previous_layer = workspace.layer;
                        let mut monitor_pinned_hwnd = None;

                        if !workspace_contains_window && needs_reconciliation.is_none() {
                            // The rule locks are scoped to the float/pin matching
                            // below and dropped before any mutation, focus or z-order
                            // work: holding REGEX_IDENTIFIERS across that work would
                            // self-deadlock the window-manager thread, because the
                            // layer-stack and ignored-window passes enumerate windows
                            // and re-enter should_manage(), which re-locks it.
                            // Pinned-floating rule matching must not depend on the
                            // floating-applications list being non-empty: a window
                            // that is only matched by the pinned rules (e.g. a
                            // file manager pinned across all workspaces) must still
                            // be auto-pinned when a new instance replaces a
                            // previous one whose HWND died.
                            let (should_float, should_pin) = {
                                let floating_applications = FLOATING_APPLICATIONS.lock();
                                let pinned_floating_applications =
                                    PINNED_FLOATING_APPLICATIONS.lock();
                                let regex_identifiers = REGEX_IDENTIFIERS.lock();
                                let mut should_float = false;
                                let mut should_pin = false;

                                if let (Ok(title), Ok(exe_name), Ok(class), Ok(path)) =
                                    (window.title(), window.exe(), window.class(), window.path())
                                {
                                    if !floating_applications.is_empty() {
                                        should_float = should_act(
                                            &title,
                                            &exe_name,
                                            &class,
                                            &path,
                                            &floating_applications,
                                            &regex_identifiers,
                                        )
                                        .is_some();
                                    }

                                    if !pinned_floating_applications.is_empty() {
                                        should_pin = should_act(
                                            &title,
                                            &exe_name,
                                            &class,
                                            &path,
                                            &pinned_floating_applications,
                                            &regex_identifiers,
                                        )
                                        .is_some();
                                    }
                                }

                                (should_float, should_pin)
                            };

                            if behaviour.float_override
                                || behaviour.floating_layer_override
                                || (should_float && !matches!(event, WindowManagerEvent::Manage(_)))
                            {
                                let placement = if behaviour.floating_layer_override {
                                    // Floating layer override placement
                                    behaviour.floating_layer_placement
                                } else if behaviour.float_override {
                                    // Float override placement
                                    behaviour.float_override_placement
                                } else {
                                    // Float rule placement
                                    behaviour.float_rule_placement
                                };

                                // Center floating windows according to the proper placement if not
                                // on a floating workspace
                                let center_spawned_floats =
                                    placement.should_center() && workspace.tile;
                                if should_pin {
                                    monitor_pinned_hwnd = Some(window.hwnd);
                                }
                                workspace.floating_windows_mut().push_back(window);
                                workspace.layer = WorkspaceLayer::Floating;
                                if center_spawned_floats {
                                    let mut floating_window = window;
                                    floating_window.center(
                                        &workspace.globals.work_area,
                                        placement.should_resize(),
                                    )?;
                                }
                                self.update_focused_workspace(false, false)?;

                                // Pinning re-homes the window from the workspace's
                                // floating list into the monitor's pinned set once
                                // the workspace borrow has ended.
                                if let Some(hwnd) = monitor_pinned_hwnd {
                                    if let Some(monitor) = self.focused_monitor_mut() {
                                        monitor.pin_floating_window(hwnd);
                                    }

                                    // Re-homing the new float into the pin set can
                                    // empty the workspace again (an auto-pinned window
                                    // never joins the workspace's own windows), so
                                    // re-evaluate the hide-on-empty visibility to
                                    // re-hide pins that the workspace update above
                                    // restored for the now-emptied workspace.
                                    if let Some(monitor) = self.focused_monitor() {
                                        monitor.apply_pin_visibility()?;
                                    }
                                }
                            } else if let Some(monocle) = &mut workspace.monocle_container {
                                monocle.add_window(window);
                                if !workspace.layer_lock {
                                    workspace.layer = WorkspaceLayer::Tiling;
                                }
                            } else {
                                match behaviour.current_behaviour {
                                    WindowContainerBehaviour::Create => {
                                        workspace.new_container_for_window(window);
                                        if !workspace.layer_lock {
                                            workspace.layer = WorkspaceLayer::Tiling;
                                        }
                                        self.update_focused_workspace(false, false)?;
                                    }
                                    WindowContainerBehaviour::Append => {
                                        workspace
                                            .focused_container_mut()
                                            .ok_or_eyre("there is no focused container")?
                                            .add_window(window);
                                        if !workspace.layer_lock {
                                            workspace.layer = WorkspaceLayer::Tiling;
                                        }
                                        self.update_focused_workspace(true, false)?;
                                        stackbar_manager::send_notification();
                                    }
                                }
                            }

                            if monocle_container.is_some() {
                                self.update_focused_workspace(true, true)?;
                            } else if self.focus_new_windows
                                || (self.focused_workspace()?.containers().len() == 1
                                    && self.focused_workspace()?.floating_windows().is_empty())
                                || (self.focused_workspace()?.containers().is_empty()
                                    && self.focused_workspace()?.floating_windows().len() == 1)
                            {
                                // If after adding this window the workspace only contains 1 window, it
                                // means it was previously empty and we focused the desktop to unfocus
                                // any previous window from other workspace, so now we need to focus
                                // this window again. This is needed because sometimes some windows
                                // first send the `FocusChange` event and only the `Show` event after
                                // and we will be focusing the desktop on the `FocusChange` event since
                                // it is still empty.
                                window.focus(self.mouse_follows_focus)?;
                            }
                        }

                        // If adding the window flipped the workspace layer, re-establish
                        // the layer stack so the whole layer the new window joined is
                        // drawn above its base consistently.
                        if previous_layer != self.focused_workspace()?.layer {
                            self.focused_monitor()
                                .ok_or_eyre("there is no monitor with this idx")?
                                .enforce_layer_stack()?;
                        }

                        // Re-assert the pinned floating band even without a layer flip
                        // so a window that just joined a Floating workspace cannot bury
                        // the pinned windows above it.
                        let focused_workspace = self.focused_workspace()?;
                        if focused_workspace.layer == WorkspaceLayer::Floating {
                            self.focused_monitor()
                                .ok_or_eyre("there is no monitor with this idx")?
                                .raise_pinned_windows();
                        }

                        if workspace_contains_window {
                            let mut monocle_window_event = false;
                            if let Some(ref monocle) = monocle_container
                                && let Some(monocle_window) = monocle.focused_window()
                                && monocle_window.hwnd == window.hwnd
                            {
                                monocle_window_event = true;
                            }

                            let workspace = self.focused_workspace()?;
                            if !(monocle_window_event || workspace.layer != WorkspaceLayer::Tiling)
                                && monocle_container.is_some()
                            {
                                window.hide();
                            }
                        }
                    }
                }
            }
            WindowManagerEvent::MoveResizeStart(_, window) => {
                let monitor_idx = self.focused_monitor_idx();
                let workspace_idx = self
                    .focused_monitor()
                    .ok_or_eyre("there is no monitor with this idx")?
                    .focused_workspace_idx();

                WindowsApi::bring_window_to_top(window.hwnd)?;

                let pending_move_op = Arc::make_mut(&mut self.pending_move_op);
                *pending_move_op = Option::from((monitor_idx, workspace_idx, window.hwnd));
            }
            WindowManagerEvent::MoveResizeEnd(_, window) => {
                if self.capture_native_maximize(window)? {
                    return Ok(());
                }

                // We need this because if the event ends on a different monitor,
                // that monitor will already have been focused and updated in the state
                let pending = *self.pending_move_op;
                // Always consume the pending move op whenever this event is handled
                let pending_move_op = Arc::make_mut(&mut self.pending_move_op);
                *pending_move_op = None;

                // If the window handles don't match then something went wrong and the pending
                // move is not related to this current move. The pending op has already
                // been cleared above; discard the stale origin rather than aborting the
                // whole event, which would drop this MoveResizeEnd's processing, its
                // subscriber notification and the known_hwnds refresh.
                let pending = if let Some((_, _, w_hwnd)) = pending
                    && w_hwnd != window.hwnd
                {
                    tracing::debug!(
                        "window handles for move operation don't match: {} != {}",
                        w_hwnd,
                        window.hwnd
                    );
                    None
                } else {
                    pending
                };

                let target_monitor_idx = self
                    .monitor_idx_from_current_pos()
                    .ok_or_eyre("cannot get monitor idx from current position")?;

                let focused_monitor_idx = self.focused_monitor_idx();
                let focused_workspace_idx = self.focused_workspace_idx().unwrap_or_default();
                let window_management_behaviour =
                    self.window_management_behaviour(focused_monitor_idx, focused_workspace_idx);

                let workspace = self.focused_workspace_mut()?;
                let focused_container_idx = workspace.focused_container_idx();
                let new_position = WindowsApi::window_rect(window.hwnd)?;
                let old_position = *workspace
                    .latest_layout
                    .get(focused_container_idx)
                    // If the move was to another monitor with an empty workspace, the
                    // workspace here will refer to that empty workspace, which won't
                    // have any latest layout set. We fall back to a Default for Rect
                    // which allows us to make a reasonable guess that the drag has taken
                    // place across a monitor boundary to an empty workspace
                    .unwrap_or(&Rect::default());

                // This will be true if we have moved to another monitor
                let mut moved_across_monitors = false;

                if let Some((m_idx, _)) = self.known_hwnds.get(&window.hwnd)
                    && *m_idx != target_monitor_idx
                {
                    moved_across_monitors = true;
                }

                if let Some((origin_monitor_idx, origin_workspace_idx, _)) = pending {
                    // If we didn't move to another monitor with an empty workspace, it is
                    // still possible that we moved to another monitor with a populated workspace
                    if !moved_across_monitors {
                        // So we'll check if the origin monitor index and the target monitor index
                        // are different, if they are, we can set the override
                        moved_across_monitors = origin_monitor_idx != target_monitor_idx;

                        if moved_across_monitors {
                            // Want to make sure that we exclude unmanaged windows from cross-monitor
                            // moves with a mouse, otherwise the currently focused idx container will
                            // be moved when we just want to drag an unmanaged window
                            let origin_workspace = self
                                .monitors()
                                .get(origin_monitor_idx)
                                .ok_or_eyre("cannot get monitor idx")?
                                .workspaces()
                                .get(origin_workspace_idx)
                                .ok_or_eyre("cannot get workspace idx")?;

                            let managed_window = origin_workspace.contains_window(window.hwnd);

                            if !managed_window {
                                moved_across_monitors = false;
                            }
                        }
                    }
                }

                let workspace = self.focused_workspace_mut()?;
                if (workspace.tile && workspace.contains_managed_window(window.hwnd))
                    || moved_across_monitors
                {
                    let resize = Rect {
                        left: new_position.left - old_position.left,
                        top: new_position.top - old_position.top,
                        right: new_position.right - old_position.right,
                        bottom: new_position.bottom - old_position.bottom,
                    };

                    // If we have moved across the monitors, use that override, otherwise determine
                    // if a move has taken place by ruling out a resize
                    let is_move = is_container_translation_move(resize, moved_across_monitors);

                    if is_move {
                        tracing::info!("moving with mouse");

                        if moved_across_monitors {
                            if let Some((origin_monitor_idx, origin_workspace_idx, w_hwnd)) =
                                pending
                            {
                                let target_workspace_idx = self
                                    .monitors()
                                    .get(target_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?
                                    .focused_workspace_idx();

                                let target_container_idx = self
                                    .monitors()
                                    .get(target_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?
                                    .focused_workspace()
                                    .ok_or_eyre("there is no focused workspace for this monitor")?
                                    .container_idx_from_current_point()
                                    // Default to 0 in the case of an empty workspace
                                    .unwrap_or(0);

                                let origin = (origin_monitor_idx, origin_workspace_idx, w_hwnd);
                                let target = (
                                    target_monitor_idx,
                                    target_workspace_idx,
                                    target_container_idx,
                                );
                                self.transfer_window(origin, target)?;

                                // We want to make sure both the origin and target monitors are updated,
                                // so that we don't have ghost tiles until we force an interaction on
                                // the origin monitor's focused workspace
                                self.focus_monitor(origin_monitor_idx)?;
                                let origin_monitor = self
                                    .monitors_mut()
                                    .get_mut(origin_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?;
                                origin_monitor.focus_workspace(origin_workspace_idx)?;
                                self.update_focused_workspace(false, false)?;

                                self.focus_monitor(target_monitor_idx)?;
                                let target_monitor = self
                                    .monitors_mut()
                                    .get_mut(target_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?;
                                target_monitor.focus_workspace(target_workspace_idx)?;
                                self.update_focused_workspace(false, false)?;

                                // Make sure to give focus to the moved window again
                                window.focus(self.mouse_follows_focus)?;
                            }
                        } else if window_management_behaviour.float_override {
                            workspace.floating_windows_mut().push_back(window);
                            self.update_focused_workspace(false, false)?;
                        } else {
                            match window_management_behaviour.current_behaviour {
                                WindowContainerBehaviour::Create => {
                                    match workspace.container_idx_from_current_point() {
                                        Some(target_idx) => {
                                            workspace
                                                .swap_containers(focused_container_idx, target_idx);
                                            self.update_focused_workspace(false, false)?;
                                        }
                                        None => {
                                            self.update_focused_workspace(
                                                self.mouse_follows_focus,
                                                false,
                                            )?;
                                        }
                                    }
                                }
                                WindowContainerBehaviour::Append => {
                                    match workspace.container_idx_from_current_point() {
                                        Some(target_idx) => {
                                            workspace.move_window_to_container(target_idx)?;
                                            self.update_focused_workspace(false, false)?;
                                        }
                                        None => {
                                            self.update_focused_workspace(
                                                self.mouse_follows_focus,
                                                false,
                                            )?;
                                        }
                                    }

                                    stackbar_manager::send_notification();
                                }
                            }
                        }
                    } else {
                        tracing::info!("resizing with mouse");
                        let mut ops = vec![];

                        macro_rules! resize_op {
                            ($coordinate:expr, $comparator:tt, $direction:expr) => {{
                                let adjusted = $coordinate * 2;
                                let sizing = if adjusted $comparator 0 {
                                    Sizing::Decrease
                                } else {
                                    Sizing::Increase
                                };

                                ($direction, sizing, adjusted.abs())
                            }};
                        }

                        if resize.left != 0 {
                            ops.push(resize_op!(resize.left, >, OperationDirection::Left));
                        }

                        if resize.top != 0 {
                            ops.push(resize_op!(resize.top, >, OperationDirection::Up));
                        }

                        // TODO: Determine if this is still needed
                        let top_left_constant = BORDER_WIDTH.load(Ordering::SeqCst)
                            + BORDER_OFFSET.load(Ordering::SeqCst);

                        if resize.right != 0
                            && (resize.left == top_left_constant || resize.left == 0)
                        {
                            ops.push(resize_op!(resize.right, <, OperationDirection::Right));
                        }

                        if resize.bottom != 0
                            && (resize.top == top_left_constant || resize.top == 0)
                        {
                            ops.push(resize_op!(resize.bottom, <, OperationDirection::Down));
                        }

                        for (edge, sizing, delta) in ops {
                            self.resize_window(edge, sizing, delta, true)?;
                        }

                        self.update_focused_workspace(false, false)?;
                    }
                }
            }
            WindowManagerEvent::LocationChange(_, window) => {
                if self.capture_native_maximize(window)? {
                    return Ok(());
                }
            }
            WindowManagerEvent::MouseCapture(..)
            | WindowManagerEvent::Cloak(..)
            | WindowManagerEvent::TitleUpdate(..) => {}
        };

        // If we unmanaged a window, it shouldn't be immediately hidden behind managed windows
        if let WindowManagerEvent::Unmanage(mut window) = event {
            window.center(&self.focused_monitor_work_area()?, true)?;
        }

        // Update list of known_hwnds and their monitor/workspace index pair. This
        // rebuilds the entire hwnd map and can write it out to disk, so only do it
        // when the event can change how windows map onto monitors and workspaces;
        // events such as focus changes, title updates, cloaks or mouse captures
        // never alter the mapping.
        if matches!(
            event,
            WindowManagerEvent::Destroy(..)
                | WindowManagerEvent::Unmanage(_)
                | WindowManagerEvent::Hide(..)
                | WindowManagerEvent::Minimize(..)
                | WindowManagerEvent::Show(..)
                | WindowManagerEvent::Manage(_)
                | WindowManagerEvent::Uncloak(..)
                | WindowManagerEvent::MoveResizeEnd(..)
        ) {
            self.update_known_hwnds();
        }

        // Skip the payload state construction and serialization entirely when
        // no bar/subscriber is connected; this avoids deep-cloning and JSON
        // serializing the whole state tree (with live Win32 window probes) on
        // every single event.
        if has_subscribers() {
            notify_subscribers(
                NotificationEvent::WindowManager(event),
                initial_state.has_been_modified(self.as_ref()),
                || self.as_ref().into(),
            )?;
        }

        border_manager::send_notification(Some(event.hwnd()));
        transparency_manager::send_notification();
        stackbar_manager::send_notification();

        // Too many spammy OBJECT_NAMECHANGE events from JetBrains IDEs
        if !matches!(
            event,
            WindowManagerEvent::Show(WinEvent::ObjectNameChange, _)
        ) {
            tracing::info!("processed: {}", event.window().to_string());
        } else {
            tracing::trace!("processed: {}", event.window().to_string());
        }

        Ok(())
    }

    /// Checks if this window is from another unfocused workspace or is an unfocused window on a
    /// stack container. If it is it will return the monitor/workspace index pair of this window so
    /// that a reconciliation of that monitor/workspace can be done.
    fn needs_reconciliation(&self, window: Window) -> color_eyre::Result<Option<(usize, usize)>> {
        let focused_monitor_idx = self.focused_monitor_idx();
        let focused_workspace_idx =
            self.focused_workspace_idx_for_monitor_idx(focused_monitor_idx)?;

        let focused_pair = (focused_monitor_idx, focused_workspace_idx);

        let mut needs_reconciliation = None;

        if let Some((m_idx, ws_idx)) = self.known_hwnds.get(&window.hwnd) {
            if (*m_idx, *ws_idx) != focused_pair {
                tracing::debug!("Needs reconciliation for a different monitor/workspace pair");
                needs_reconciliation = Some((*m_idx, *ws_idx));
            }
        }

        Ok(needs_reconciliation)
    }

    /// When there was an `alt-tab` to a hidden window we need to perform a reconciliation, meaning
    /// we need to update the focused monitor, workspace, container and window indices to the ones
    /// corresponding to the window the user just alt-tabbed into.
    fn perform_reconciliation(
        &mut self,
        window: Window,
        reconciliation_pair: (usize, usize),
    ) -> color_eyre::Result<()> {
        let (m_idx, ws_idx) = reconciliation_pair;

        tracing::debug!("performing reconciliation");
        self.focus_monitor(m_idx)?;
        let mouse_follows_focus = self.mouse_follows_focus;
        let offset = self.work_area_offset;

        if let Some(monitor) = self.focused_monitor_mut() {
            if ws_idx != monitor.focused_workspace_idx() {
                let previous_idx = monitor.focused_workspace_idx();
                monitor.last_focused_workspace = Option::from(previous_idx);
                monitor.focus_workspace(ws_idx)?;
            }
            if let Some(workspace) = monitor.focused_workspace_mut() {
                let mut layer = WorkspaceLayer::Tiling;
                if let Some((monocle, idx)) = workspace
                    .monocle_container
                    .as_mut()
                    .and_then(|m| m.idx_for_window(window.hwnd).map(|i| (m, i)))
                {
                    monocle.focus_window(idx);
                } else if workspace
                    .floating_windows()
                    .iter()
                    .any(|w| w.hwnd == window.hwnd)
                {
                    layer = WorkspaceLayer::Floating;
                } else if workspace
                    .maximized_window
                    .is_none_or(|w| w.hwnd != window.hwnd)
                {
                    // If the window is the maximized window do nothing, else we
                    // reintegrate the monocle if it exists and then focus the
                    // container
                    if workspace.monocle_container.is_some() {
                        tracing::info!("disabling monocle");
                        for container in workspace.containers_mut() {
                            container.restore();
                        }
                        for window in workspace.floating_windows_mut() {
                            window.restore();
                        }
                        workspace.reintegrate_monocle_container()?;
                    }
                    workspace.focus_container_by_window(window.hwnd)?;
                }
                if !workspace.layer_lock {
                    workspace.layer = layer;
                }
            }
            monitor.load_focused_workspace(mouse_follows_focus, true)?;
            monitor.update_focused_workspace(offset)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::is_container_translation_move;
    use crate::core::Rect;

    #[test]
    fn plain_click_is_not_a_move() {
        // A click on a window border fires a MoveResizeEnd with zero deltas.
        let resize = Rect::default();
        assert!(!is_container_translation_move(resize, false));
    }

    #[test]
    fn title_bar_translation_is_a_move() {
        // Dragging a window keeps its size while changing position.
        let resize = Rect {
            left: 40,
            top: 20,
            right: 0,
            bottom: 0,
        };
        assert!(is_container_translation_move(resize, false));
    }

    #[test]
    fn left_edge_resize_is_not_a_move() {
        // Dragging the left border changes the width, not the position only.
        let resize = Rect {
            left: -30,
            top: 0,
            right: 30,
            bottom: 0,
        };
        assert!(!is_container_translation_move(resize, false));
    }

    #[test]
    fn top_edge_resize_is_not_a_move() {
        let resize = Rect {
            left: 0,
            top: -30,
            right: 0,
            bottom: 30,
        };
        assert!(!is_container_translation_move(resize, false));
    }

    #[test]
    fn corner_resize_is_not_a_move() {
        let resize = Rect {
            left: -30,
            top: -20,
            right: 30,
            bottom: 20,
        };
        assert!(!is_container_translation_move(resize, false));
    }

    #[test]
    fn cross_monitor_is_always_a_move() {
        assert!(is_container_translation_move(Rect::default(), true));
    }
}
