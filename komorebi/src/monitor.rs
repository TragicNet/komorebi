use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use color_eyre::eyre::bail;
use serde::Deserialize;
use serde::Serialize;

use crate::apply_worker::ApplyWorker;
use crate::border_manager::BORDER_ENABLED;
use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_WIDTH;
use crate::core::Rect;

use crate::DEFAULT_CONTAINER_PADDING;
use crate::DEFAULT_WORKSPACE_PADDING;
use crate::DefaultLayout;
use crate::FloatingLayerBehaviour;
use crate::HIDE_PINNED_ON_EMPTY_WORKSPACES;
use crate::Layout;
use crate::OperationDirection;
use crate::Wallpaper;
use crate::Window;
use crate::WindowsApi;
use crate::container::Container;
use crate::ring::Ring;
use crate::windows_callbacks;
use crate::workspace::Workspace;
use crate::workspace::WorkspaceGlobals;
use crate::workspace::WorkspaceLayer;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Monitor {
    pub id: isize,
    pub name: String,
    pub device: String,
    pub device_id: String,
    pub serial_number_id: Option<String>,
    pub size: Rect,
    pub work_area_size: Rect,
    pub work_area_offset: Option<Rect>,
    pub window_based_work_area_offset: Option<Rect>,
    pub window_based_work_area_offset_limit: isize,
    pub workspaces: Ring<Workspace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_focused_workspace: Option<usize>,
    /// Epoch milliseconds (`SystemTime`) of the last workspace/monitor switch. Used
    /// only to scope non-widget (fullscreen game) foreground restoration to the short
    /// event storm that follows a switch, so games never lose focus during gameplay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_switch_at: Option<u64>,
    pub workspace_names: HashMap<usize, String>,
    pub container_padding: Option<i32>,
    pub workspace_padding: Option<i32>,
    pub wallpaper: Option<Wallpaper>,
    pub floating_layer_behaviour: Option<FloatingLayerBehaviour>,
    /// HWNDs of floating windows pinned across all workspaces on this monitor.
    /// Pinned windows live here (not in any workspace's `floating_windows`) and
    /// are treated as part of the floating overlay of whichever workspace is
    /// focused. Only floating windows can be pinned.
    #[serde(default)]
    pub pinned_floating: Vec<isize>,
    /// Subset of `pinned_floating` rendered above everything else via the
    /// persistent TopMost band (`WS_EX_TOPMOST`), like an "always on top"
    /// status bar. Toggled per pinned window; never lowered or cleared by the
    /// layer stack, only by the toggle itself or by unpinning.
    #[serde(default)]
    pub pinned_always_on_top: Vec<isize>,
}

impl_ring_elements!(Monitor, Workspace);

#[derive(Serialize)]
pub struct MonitorInformation {
    pub id: isize,
    pub name: String,
    pub device: String,
    pub device_id: String,
    pub serial_number_id: Option<String>,
    pub size: Rect,
}

impl From<&Monitor> for MonitorInformation {
    fn from(monitor: &Monitor) -> Self {
        Self {
            id: monitor.id,
            name: monitor.name.clone(),
            device: monitor.device.clone(),
            device_id: monitor.device_id.clone(),
            serial_number_id: monitor.serial_number_id.clone(),
            size: monitor.size,
        }
    }
}

pub fn new(
    id: isize,
    size: Rect,
    work_area_size: Rect,
    name: String,
    device: String,
    device_id: String,
    serial_number_id: Option<String>,
) -> Monitor {
    let mut workspaces = Ring::default();
    workspaces.elements_mut().push_back(Workspace::default());

    Monitor {
        id,
        name,
        device,
        device_id,
        serial_number_id,
        size,
        work_area_size,
        work_area_offset: None,
        window_based_work_area_offset: None,
        window_based_work_area_offset_limit: 1,
        workspaces,
        last_focused_workspace: None,
        last_switch_at: None,
        workspace_names: HashMap::default(),
        container_padding: None,
        workspace_padding: None,
        wallpaper: None,
        floating_layer_behaviour: None,
        pinned_floating: vec![],
        pinned_always_on_top: vec![],
    }
}

/// Current Unix epoch time in milliseconds.
fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

impl Monitor {
    pub fn new(
        id: isize,
        size: Rect,
        work_area_size: Rect,
        name: String,
        device: String,
        device_id: String,
        serial_number_id: Option<String>,
    ) -> Self {
        new(
            id,
            size,
            work_area_size,
            name,
            device,
            device_id,
            serial_number_id,
        )
    }

    pub fn placeholder() -> Self {
        Self {
            id: 0,
            name: "PLACEHOLDER".to_string(),
            device: "".to_string(),
            device_id: "".to_string(),
            serial_number_id: None,
            size: Default::default(),
            work_area_size: Default::default(),
            work_area_offset: None,
            window_based_work_area_offset: None,
            window_based_work_area_offset_limit: 0,
            workspaces: Default::default(),
            last_focused_workspace: None,
            last_switch_at: None,
            workspace_names: Default::default(),
            container_padding: None,
            workspace_padding: None,
            wallpaper: None,
            floating_layer_behaviour: None,
            pinned_floating: vec![],
            pinned_always_on_top: vec![],
        }
    }

    pub fn focused_workspace_name(&self) -> Option<String> {
        self.focused_workspace()
            .map(|w| w.name.clone())
            .unwrap_or(None)
    }

    pub fn focused_workspace_layout(&self) -> Option<Layout> {
        self.focused_workspace().and_then(|workspace| {
            if workspace.tile {
                Some(workspace.layout.clone())
            } else {
                None
            }
        })
    }

    pub fn load_focused_workspace(
        &mut self,
        mouse_follows_focus: bool,
        trigger_focus: bool,
    ) -> eyre::Result<()> {
        self.last_switch_at = Some(now_epoch_ms());

        let focused_idx = self.focused_workspace_idx();
        let hmonitor = self.id;
        let monitor_wp = self.wallpaper.clone();
        for (i, workspace) in self.workspaces_mut().iter_mut().enumerate() {
            if i == focused_idx {
                workspace.restore(mouse_follows_focus, trigger_focus, hmonitor, &monitor_wp)?;
            } else {
                // hide() hides the workspace's floats and containers; pinned
                // windows are not in any workspace's floating list, so they
                // remain visible across all workspaces on this monitor.
                workspace.hide(None);
            }
        }

        // Re-establish the workspace layer stack on this monitor so the whole
        // tiling/floating base is drawn consistently after a workspace or
        // monitor switch. Skipped while the ignored windows have been manually
        // raised above managed with `toggle-ignored-window-layer`. Ignored
        // (unmanaged) windows are never moved by komorebi on their own.
        //
        // The raise primitives clamp below the active (foreground) window,
        // which during a switch is usually still the window that held focus
        // before the switch: `Window::focus` activates asynchronously via
        // sendInput, so by the time the stack is rebuilt the foreground has
        // rarely landed on the newly focused managed window. `climb_active`
        // selects the raise primitive per band: when the stale foreground is
        // not one of this monitor's managed windows (an ignored fullscreen
        // game, a widget, the shell) the overlay bands are raised via the
        // transient TopMost-band raise so the reloaded workspace determinis-
        // tically renders above it without ever reordering the ignored window.
        //
        // The Tiling band itself is always raised via the transient TopMost
        // band on the switch path (`climb_tiled`): hiding the previous
        // workspace's windows auto-promotes whatever is visible next - usually
        // a pinned window, which is not contained in any workspace - and that
        // promotion races the plain HWND_TOP raises, leaving the pins stuck
        // above the tiled layer. The TopMost dance climbs the active window
        // unconditionally at apply time, so the tiled band is positioned above
        // the promoted foreground regardless of the timing.
        let climb_active = !WindowsApi::foreground_window()
            .ok()
            .is_some_and(|foreground| {
                self.workspaces()
                    .iter()
                    .any(|workspace| workspace.contains_window(foreground))
            });
        self.enforce_layer_stack_inner(climb_active, true)?;

        Ok(())
    }

    /// Re-establishes the workspace layer stack for the focused workspace so
    /// that the layering is consistent after any transition (workspace switch,
    /// monitor switch, focus-driven layer flip):
    ///
    /// ```text
    /// bottom -> top: base layer < top layer < focused window
    /// ```
    ///
    /// In `Tiling` mode the floating windows form the base and the tiled windows
    /// the top layer; in `Floating` mode the layers are reversed. Ignored
    /// (unmanaged) windows are never reordered here; the manual
    /// `toggle-ignored-window-layer` toggle is always respected.
    #[tracing::instrument(skip(self))]
    pub fn enforce_layer_stack(&self) -> eyre::Result<()> {
        self.enforce_layer_stack_inner(false, false)
    }

    /// Re-establishes the workspace layer stack with the raise primitive
    /// selected per band.
    ///
    /// With `climb_active` the managed bands are raised via the transient
    /// TopMost-band raise so they deterministically pop above the active
    /// (foreground) window — used when a workspace/monitor switch rebuilds the
    /// stack while an unmanaged window (an ignored fullscreen game, a widget)
    /// still holds the foreground and a plain `HWND_TOP` raise could not climb
    /// it. The focused-window raise climbs too, so the top-layer window is
    /// asserted above the whole stack even before its asynchronous activation
    /// lands.
    ///
    /// With `climb_tiled` the Tiling band (the focused windows of every
    /// container) is additionally raised via the transient TopMost-band raise,
    /// so it is positioned above whatever holds the foreground *at apply time*:
    /// hiding the previous workspace's windows during a switch auto-promotes a
    /// visible window - usually a pinned window - to the foreground, and that
    /// promotion races the plain `HWND_TOP` raises (the old foreground is
    /// still reported as contained, so `climb_active` alone would pick the
    /// plain raise and leave the pins stuck above the tiled layer). The TopMost
    /// dance climbs the active window unconditionally, so the tiled band cannot
    /// be beaten by the promoted foreground regardless of timing.
    #[tracing::instrument(skip(self))]
    fn enforce_layer_stack_inner(&self, climb_active: bool, climb_tiled: bool) -> eyre::Result<()> {
        let Some(workspace) = self.focused_workspace() else {
            return Ok(());
        };

        self.apply_pin_visibility()?;

        // The manual `ignored_windows_above_managed` layer toggle is always
        // respected: the user is in charge of the layer stack, including where
        // the pinned windows sit.
        if workspace.ignored_windows_above_managed {
            // Keep the hide-on-empty invariant intact even on the manual
            // overlay toggle.
            self.apply_pin_visibility()?;
            return Ok(());
        }

        // Fullscreen (monocle / maximized) windows manage their own stacking, so
        // only make sure pinned windows stay in their band below.
        if workspace.monocle_container.is_some() || workspace.maximized_window.is_some() {
            self.reposition_pinned_windows(workspace.layer)?;
            self.apply_pin_visibility()?;
            return Ok(());
        }

        if workspace.is_empty() {
            // Pins are the persistent overlay visible across workspaces, so an
            // empty workspace keeps them rendered in the overlay band instead
            // of sinking them to the very bottom of the Z order.
            self.raise_pinned_windows();
            self.apply_pin_visibility()?;
            return Ok(());
        }

        let focused_window = match workspace.layer {
            WorkspaceLayer::Tiling => workspace
                .focused_container()
                .and_then(|container| container.focused_window())
                .copied(),
            WorkspaceLayer::Floating => {
                // Surface a pinned window only when it is the layer's remembered
                // last-used float, i.e. the user's last float interaction was
                // that pinned window. Do NOT consult the live foreground here:
                // Window::focus() activates asynchronously via sendInput, so the
                // foreground is stale mid-toggle and an incidentally active pin
                // would wrongly be raised above the window that was focused.
                if let Some(hwnd) = workspace.last_focused_floating_hwnd
                    && self.is_pinned(hwnd)
                {
                    Some(Window::from(hwnd))
                } else {
                    workspace.focused_floating_window().copied()
                }
            }
        };

        // With `climb_active` every managed band is raised via the transient
        // TopMost-band raise so the layer stack can be assembled above the
        // active window; otherwise the plain HWND_TOP raise is used and the
        // active window stays above the raised bands.
        let raise = |window: &Window| {
            if climb_active {
                self.raise_managed_window_above_active(window);
            } else {
                self.raise_managed_window(window);
            }
        };

        match workspace.layer {
            WorkspaceLayer::Tiling => {
                // Pinned windows join the floating base of the managed band:
                // they are raised first (base of the band) so the working
                // floats and the tiled layer stack above them, instead of being
                // sunk to the very bottom of the Z order. The transparent
                // TopMost-band raise cannot climb a window stuck in the
                // persistent TopMost band, so the normal pins and the working
                // floats are demoted out of it before the band raises,
                // preserving the deterministic ordering.
                let pins = self
                    .pinned_windows()
                    .into_iter()
                    .filter(|window| window.is_window())
                    .collect::<Vec<_>>();
                let (always_on_top_pins, normal_pins): (Vec<Window>, Vec<Window>) = pins
                    .iter()
                    .partition(|window| self.is_pinned_always_on_top(window.hwnd));
                let floats = workspace.floating_windows();
                let topmost_batch = normal_pins
                    .iter()
                    .chain(floats.iter())
                    .copied()
                    .collect::<Vec<_>>();
                ApplyWorker::clear_topmost(topmost_batch);
                for window in &normal_pins {
                    raise(window);
                }
                for window in floats.iter() {
                    raise(window);
                }
                // The tiled band ends the managed-band assembly, so it must be
                // raised above the pins and floats that came before it AND above
                // whatever holds the foreground at apply time. A plain HWND_TOP
                // raise cannot climb the active window, so when the switch path
                // requests it (`climb_tiled`) the band climbs via the transient
                // TopMost-band raise: hiding the previous workspace's windows
                // auto-promotes a visible window - usually a pinned window - to
                // the foreground while these raises drain, and only the TopMost
                // dance is immune to that timing.
                for window in workspace.containers().iter().rev() {
                    if let Some(window) = window.focused_window() {
                        if climb_tiled {
                            self.raise_managed_window_above_active(window);
                        } else {
                            raise(window);
                        }
                    }
                }
                // Raised last so the always-on-top pins top the whole Tiling
                // layer, exactly like the Floating overlay.
                ApplyWorker::make_topmost(always_on_top_pins);
            }
            WorkspaceLayer::Floating => {
                // Tiled windows form the base, then the pinned band, then the
                // working floats. Pinned windows are just floats reachable from
                // every workspace, so they join the overlay's band BELOW the
                // floats: raising the pins first makes the layer read
                // base -> pins -> floats -> focused, with no pins-over-floats
                // artifact. A focused pin still surfaces via the focused-window
                // raise below.
                let pins = self
                    .pinned_windows()
                    .into_iter()
                    .filter(|window| window.is_window())
                    .collect::<Vec<_>>();
                // Always-on-top pins stay in the persistent TopMost band above
                // the whole overlay: they are excluded from the demotion batch
                // and re-asserted last so nothing can climb them.
                let (always_on_top_pins, normal_pins): (Vec<Window>, Vec<Window>) = pins
                    .iter()
                    .partition(|window| self.is_pinned_always_on_top(window.hwnd));
                let floats = workspace.floating_windows();
                // A window in the TopMost band can never be climbed by a plain
                // HWND_TOP raise from a normal-band window, so the pinned band
                // and the working floats must be demoted out of the TopMost band
                // first. Some applications (e.g. pinned/always-on-top tool
                // windows) re-assert their own TopMost state; that shows up in
                // the layered band diagnostics. Demoting runs on the apply
                // worker ahead of the raises below, preserving the ordering.
                let topmost_batch = normal_pins
                    .iter()
                    .chain(floats.iter())
                    .copied()
                    .collect::<Vec<_>>();
                ApplyWorker::clear_topmost(topmost_batch);
                for window in workspace.containers() {
                    if let Some(window) = window.focused_window() {
                        raise(window);
                    }
                }
                for window in &normal_pins {
                    self.raise_managed_window_above_active(window);
                }
                for window in floats.iter().rev() {
                    self.raise_managed_window_above_active(window);
                }
                // Raised last so the always-on-top pins top the whole overlay,
                // including the focused window raised below.
                ApplyWorker::make_topmost(always_on_top_pins);
            }
        }

        // Keep the focused window of the top layer on the very top. Done after
        // the pinned band is positioned so the raised focused window ends up
        // above the pinned floating windows and the rest of the overlay. On
        // the Floating layer, and whenever the stack is assembled climbing the
        // active window, the raise must clear the active window so the last
        // focused window tops the whole overlay regardless of which window
        // holds the foreground. The Tiling band is always raised above the
        // active window on the switch path (`climb_tiled`), so the focused
        // tiled window must clear it the same way or a foreground auto-promoted
        // during the switch (a pinned window) would sit above it.
        if let Some(window) = focused_window {
            if climb_tiled && matches!(workspace.layer, WorkspaceLayer::Tiling) {
                self.raise_managed_window_above_active(&window);
            } else {
                raise(&window);
            }
        }

        // If a window was auto-promoted to the foreground while the switch was
        // in flight (Windows draws foreground windows above everything else),
        // hand focus back to the top-layer window so the layer stack is not
        // visually reverted once the event storm settles.
        //
        // Desktop widgets, status bars and popup widgets (e.g. yasb bars/popups,
        // Rainmeter meters) are always eligible: Windows auto-promotes these to
        // the foreground and without restoring managed focus the layer is
        // visually reverted. Fullscreen games and other regular application
        // windows are only eligible for a short window after a workspace/monitor
        // switch, while the event storm is still settling - the shell tracks
        // them as a "fullscreen app" and `hide_on_fullscreen` status bars
        // flicker as their foreground toggles. Outside that window they are left
        // alone: they are foreground because they are being used, and stealing
        // activation away makes them randomly lose focus during gameplay. Never
        // steal while the cursor is directly over the foreground ignored window
        // (active interaction).
        if let Some(foreground) = WindowsApi::foreground_window().ok()
            && let Some(unmanaged) = self
                .ignored_windows()
                .iter()
                .find(|window| window.hwnd == foreground)
            && Self::should_steal_ignored_window_foreground(
                unmanaged.is_widget_window(),
                self.within_switch_stabilization(),
            )
            && WindowsApi::window_at_cursor_pos().ok() != Some(foreground)
            && let Some(window) = focused_window
        {
            tracing::debug!(
                hwnd = foreground,
                exe = unmanaged.exe().unwrap_or_default(),
                title = unmanaged.title().unwrap_or_default(),
                "ignored window auto-promoted to foreground during switch, restoring managed focus",
            );
            let _ = WindowsApi::raise_and_focus_window(window.hwnd);
        }

        // Final enforcement of the hide-on-empty invariant: a raise or focus
        // performed during the layer stack may have re-shown a hidden pin, so
        // coerce visibility back to the configured state before returning.
        self.apply_pin_visibility()?;

        Ok(())
    }

    /// Places the pinned floating windows of all workspaces on this monitor in
    /// the layer band belonging to the focused workspace's layer: they join the
    /// Floating overlay when the layer is Floating, and sit in the floating base
    /// below the tiled top layer when it is Tiling.
    ///
    /// Uses synchronous z-order operations, so the placement is fully applied
    /// before this returns regardless of `WINDOW_HANDLING_BEHAVIOUR`, and no
    /// pending async window-thread raise can land above the pinned windows. The
    /// focused top-layer window is raised/activated afterwards so it sits on top
    /// of the pinned band.
    fn reposition_pinned_windows(&self, layer: WorkspaceLayer) -> eyre::Result<()> {
        match layer {
            WorkspaceLayer::Floating => {
                self.raise_pinned_windows();
            }
            WorkspaceLayer::Tiling => {
                self.lower_pinned_windows();
            }
        };

        Ok(())
    }

    /// Returns the pinned floating windows of this monitor, in pin order.
    /// Pinned windows belong to the monitor as a whole: they are treated as
    /// part of the floating overlay of whichever workspace is currently
    /// focused.
    pub fn pinned_windows(&self) -> Vec<Window> {
        self.pinned_floating
            .iter()
            .map(|hwnd| Window::from(*hwnd))
            .collect()
    }

    /// Whether the given window HWND is pinned across all workspaces on this monitor.
    pub fn is_pinned(&self, hwnd: isize) -> bool {
        self.pinned_floating.contains(&hwnd)
    }

    /// Hide every pinned floating window on this monitor while the focused
    /// workspace is empty, and restore them once a workspace with managed
    /// windows is focused. A no-op unless the `pinning` config key's
    /// `hide_on_empty_workspaces` option is enabled: by default pins stay
    /// visible across every workspace regardless of its contents.
    pub fn apply_pin_visibility(&self) -> eyre::Result<()> {
        if !HIDE_PINNED_ON_EMPTY_WORKSPACES.load(Ordering::SeqCst) {
            return Ok(());
        }

        let hide = self
            .focused_workspace()
            .is_some_and(|workspace| workspace.is_empty());

        for window in self.pinned_windows() {
            if !window.is_window() {
                continue;
            }

            // Idempotent: only drive a real visibility change. Redundant
            // ShowWindow/SetCloak calls still emit Show/Hide events that re-enter
            // the event handlers and round-trip back through this pass. The
            // `is_shown` check reflects the actual rendered state across all
            // hiding behaviours (cloaked/minimized pins report IsWindowVisible
            // true, so `is_visible` could never detect them as hidden).
            if hide && window.is_shown() {
                window.hide();
            } else if !hide && !window.is_shown() {
                window.restore();
            }
        }

        Ok(())
    }

    /// Pin a floating window so it is visible across all workspaces on this
    /// monitor. Pinning removes the window from every workspace's floating
    /// list on this monitor: pinned windows live in `pinned_floating` alone.
    pub fn pin_floating_window(&mut self, hwnd: isize) {
        if !self.is_pinned(hwnd) {
            self.pinned_floating.push(hwnd);
        }

        for workspace in self.workspaces_mut() {
            workspace
                .floating_windows_mut()
                .retain(|window| window.hwnd != hwnd);
        }
    }

    /// Unpin a floating window, leaving it floating on its current workspace.
    pub fn unpin_floating_window(&mut self, hwnd: isize) {
        self.pinned_floating.retain(|h| *h != hwnd);
        if self.pinned_always_on_top.contains(&hwnd) {
            self.pinned_always_on_top.retain(|h| *h != hwnd);
            ApplyWorker::clear_topmost(vec![Window::from(hwnd)]);
        }
    }

    /// Whether the given pinned window HWND is rendered above everything else
    /// via the persistent TopMost band.
    pub fn is_pinned_always_on_top(&self, hwnd: isize) -> bool {
        self.pinned_always_on_top.contains(&hwnd)
    }

    /// Toggle a pinned window between the normal floating overlay band and the
    /// persistent TopMost band that renders it above everything else, like an
    /// "always on top" status bar. Posted to the apply worker so the
    /// synchronous TopMost call never blocks the window-manager thread on a Not
    /// Responding window.
    pub fn toggle_pin_always_on_top(&mut self, hwnd: isize) {
        if self.is_pinned_always_on_top(hwnd) {
            self.pinned_always_on_top.retain(|h| *h != hwnd);
            ApplyWorker::clear_topmost(vec![Window::from(hwnd)]);
        } else {
            self.pinned_always_on_top.push(hwnd);
            ApplyWorker::make_topmost(vec![Window::from(hwnd)]);
        }
    }

    /// Raise the pinned floating windows of all workspaces on this monitor so
    /// they join the Floating overlay when a workspace layer is toggled to
    /// Floating. Posted to the apply worker so the synchronous TopMost-band
    /// raise never blocks the window-manager thread on a Not Responding window;
    /// the worker preserves the deterministic overlay placement.
    /// Uses the transient TopMost band so the pinned windows are displayed above
    /// the currently active window without activating them or stealing focus.
    pub fn raise_pinned_windows(&self) -> Vec<Window> {
        // Pins that are not rendered (hidden by the visibility pass for an
        // empty workspace) are never raised: the raise primitives use a
        // show-window SetWindowPos and the focused window/band ordering would
        // be an unintended resurrection of the hidden pin.
        let windows = self
            .pinned_windows()
            .into_iter()
            .filter(|window| window.is_window() && window.is_shown())
            .collect::<Vec<_>>();
        let (always_on_top, normal) = windows
            .iter()
            .partition(|window| self.is_pinned_always_on_top(window.hwnd));
        ApplyWorker::raise_above_active(normal);
        ApplyWorker::make_topmost(always_on_top);
        windows
    }

    /// Lower the pinned floating windows of all workspaces on this monitor so
    /// they drop back behind the tiling base when a workspace layer is toggled
    /// back to Tiling. Posted to the apply worker so they can never block the
    /// window-manager thread on a Not Responding window and never end up above
    /// the tiled windows regardless of the apply ordering.
    pub fn lower_pinned_windows(&self) {
        let (normal, always_on_top) = self.pinned_window_bands();
        // Always-on-top pins are never lowered below the tiling base: they stay
        // in the persistent TopMost band, so the lower pass simply re-asserts
        // them there and lowers only the normal pinned band.
        ApplyWorker::lower(normal);
        ApplyWorker::make_topmost(always_on_top);
    }

    /// The rendered (shown) pinned floating windows of all workspaces on this
    /// monitor, split into the normal pinned band and the always-on-top band.
    /// No apply-worker operations are issued; the caller decides how to place
    /// the bands (e.g. folding both into a single deferred lower).
    pub fn pinned_window_bands(&self) -> (Vec<Window>, Vec<Window>) {
        // Pins that are not rendered (hidden by the visibility pass for an
        // empty workspace) are never lowered: the lower primitives use a
        // show-window SetWindowPos and the focused window/band ordering would
        // be an unintended resurrection of the hidden pin.
        let windows = self
            .pinned_windows()
            .into_iter()
            .filter(|window| window.is_window() && window.is_shown())
            .collect::<Vec<_>>();
        let (always_on_top, normal): (Vec<Window>, Vec<Window>) = windows
            .iter()
            .partition(|window| self.is_pinned_always_on_top(window.hwnd));
        (normal, always_on_top)
    }

    fn raise_managed_window(&self, window: &Window) {
        ApplyWorker::raise(vec![*window]);
    }

    /// Raise a window above the currently active (foreground) window without
    /// activating it, via the transient TopMost-band raise. The foreground
    /// window is always re-asserted to the top of the Z order, so a plain
    /// HWND_TOP raise cannot place the Floating overlay above it; this is the
    /// only raise that reliably pops the overlay (pinned band and working
    /// floats) over the tiling base even while a tiled window holds focus.
    /// Posted to the apply worker so the synchronous raise never blocks the
    /// window-manager thread.
    fn raise_managed_window_above_active(&self, window: &Window) {
        ApplyWorker::raise_above_active(vec![*window]);
    }

    /// Enumerates the ignored (unmanaged) windows currently visible on this
    /// monitor that look like regular application windows, fullscreen coverage
    /// windows or desktop widgets. System shell surfaces (taskbar, desktop) and
    /// tool/dialog windows are excluded so that they are never moved by the
    /// ignored window layer.
    #[tracing::instrument(skip(self))]
    pub fn ignored_windows(&self) -> Vec<Window> {
        let mut windows: Vec<Window> = vec![];
        if WindowsApi::enum_windows(
            Some(windows_callbacks::enum_ignored_window),
            &mut windows as *mut Vec<Window> as isize,
        )
        .is_err()
        {
            tracing::warn!("could not enumerate ignored windows");
            return vec![];
        }

        let monitor_id = self.id;

        windows
            .into_iter()
            .filter(|window| {
                if WindowsApi::monitor_from_window(window.hwnd) != monitor_id {
                    return false;
                }

                let is_normal = window.is_normal_application_window();
                let is_widget = !is_normal && window.is_widget_window();
                let is_fullscreen = !is_normal
                    && !is_widget
                    && (window.is_fullscreen() || window.covers_monitor_or_work_area());

                // A window that is still managed (e.g. a browser whose caption is
                // temporarily dropped while an HTML5 video plays in fullscreen) must
                // never be treated as an ignored window: lowering it would hide the
                // fullscreen video behind the tiled base layer.
                let is_managed = self
                    .workspaces()
                    .iter()
                    .any(|workspace| workspace.contains_window(window.hwnd));

                let is_candidate = Self::is_ignored_window_candidate(
                    is_managed,
                    is_normal,
                    is_fullscreen,
                    is_widget,
                );

                if is_candidate {
                    tracing::debug!(
                        hwnd = window.hwnd,
                        exe = window.exe().unwrap_or_default(),
                        title = window.title().unwrap_or_default(),
                        is_managed,
                        is_normal_application_window = is_normal,
                        is_fullscreen = is_fullscreen,
                        is_widget_window = is_widget,
                        "ignored window layer candidate on monitor"
                    );
                }

                is_candidate
            })
            .collect()
    }

    /// Decides whether an unmanaged window should be handled by the ignored
    /// window layer: it must be a regular application window, a fullscreen
    /// coverage window or a desktop widget, and it must not be a managed window
    /// that temporarily dropped its normal styles (e.g. a browser in fullscreen).
    fn is_ignored_window_candidate(
        is_managed: bool,
        is_normal: bool,
        is_fullscreen: bool,
        is_widget: bool,
    ) -> bool {
        !is_managed && (is_normal || is_fullscreen || is_widget)
    }

    /// Whether an ignored window should be moved at all by the manual
    /// `toggle-ignored-window-layer` command. Games and unmanaged application
    /// windows are always reorderable. Widget windows pinned to the topmost
    /// band (e.g. a yasb bar with always_on_top) are left untouched so the
    /// toggle never flickers them.
    pub(crate) fn should_auto_demote(window: &Window) -> bool {
        Self::should_auto_demote_predicate(window.is_widget_window(), window.is_always_on_top())
    }

    /// Pure form of `should_auto_demote` so the decision can be unit tested
    /// without touching the Win32 API.
    pub(crate) fn should_auto_demote_predicate(is_widget: bool, is_always_on_top: bool) -> bool {
        !(is_widget && is_always_on_top)
    }

    /// Whether we are inside the short event storm that follows a workspace or
    /// monitor switch. During this window the foreground of automatically
    /// re-promoted ignored windows (e.g. a fullscreen game covering the monitor)
    /// is restored to the focused managed window so the shell does not toggle
    /// `ABN_FULLSCREENAPP` and make `hide_on_fullscreen` status bars flicker.
    /// Once the storm settles the foreground is left alone so the game keeps
    /// focus during gameplay.
    pub(crate) fn within_switch_stabilization(&self) -> bool {
        const STABILIZATION_GRACE_MS: u64 = 400;

        self.last_switch_at
            .map(|last_switch_at| {
                now_epoch_ms().saturating_sub(last_switch_at) < STABILIZATION_GRACE_MS
            })
            .unwrap_or(false)
    }

    /// Whether the foreground of an ignored window should be handed back to the
    /// focused managed window. Desktop widgets, status bars and popup widgets
    /// are always eligible; unmanaged fullscreen windows (games) are only
    /// eligible while a switch's event storm is still settling, so active
    /// gameplay never loses focus.
    ///
    /// The caller additionally skips stealing while the cursor is directly over
    /// the foreground ignored window (active interaction).
    fn should_steal_ignored_window_foreground(
        is_widget: bool,
        within_switch_stabilization: bool,
    ) -> bool {
        is_widget || within_switch_stabilization
    }

    pub fn update_workspaces_globals(&mut self, offset: Option<Rect>) {
        let container_padding = self
            .container_padding
            .or(Some(DEFAULT_CONTAINER_PADDING.load(Ordering::SeqCst)));
        let workspace_padding = self
            .workspace_padding
            .or(Some(DEFAULT_WORKSPACE_PADDING.load(Ordering::SeqCst)));
        let (border_width, border_offset) = {
            let border_enabled = BORDER_ENABLED.load(Ordering::SeqCst);
            if border_enabled {
                let border_width = BORDER_WIDTH.load(Ordering::SeqCst);
                let border_offset = BORDER_OFFSET.load(Ordering::SeqCst);
                (border_width, border_offset)
            } else {
                (0, 0)
            }
        };
        let work_area = self.work_area_size;
        let work_area_offset = self.work_area_offset.or(offset);
        let window_based_work_area_offset = self.window_based_work_area_offset;
        let window_based_work_area_offset_limit = self.window_based_work_area_offset_limit;
        let floating_layer_behaviour = self.floating_layer_behaviour;

        for workspace in self.workspaces_mut() {
            workspace.globals = WorkspaceGlobals {
                container_padding,
                workspace_padding,
                border_width,
                border_offset,
                work_area,
                work_area_offset,
                window_based_work_area_offset,
                window_based_work_area_offset_limit,
                floating_layer_behaviour,
            }
        }
    }

    /// Updates the `globals` field of workspace with index `workspace_idx`
    pub fn update_workspace_globals(&mut self, workspace_idx: usize, offset: Option<Rect>) {
        let container_padding = self
            .container_padding
            .or(Some(DEFAULT_CONTAINER_PADDING.load(Ordering::SeqCst)));
        let workspace_padding = self
            .workspace_padding
            .or(Some(DEFAULT_WORKSPACE_PADDING.load(Ordering::SeqCst)));
        let (border_width, border_offset) = {
            let border_enabled = BORDER_ENABLED.load(Ordering::SeqCst);
            if border_enabled {
                let border_width = BORDER_WIDTH.load(Ordering::SeqCst);
                let border_offset = BORDER_OFFSET.load(Ordering::SeqCst);
                (border_width, border_offset)
            } else {
                (0, 0)
            }
        };
        let work_area = self.work_area_size;
        let work_area_offset = self.work_area_offset.or(offset);
        let window_based_work_area_offset = self.window_based_work_area_offset;
        let window_based_work_area_offset_limit = self.window_based_work_area_offset_limit;
        let floating_layer_behaviour = self.floating_layer_behaviour;

        if let Some(workspace) = self.workspaces_mut().get_mut(workspace_idx) {
            workspace.globals = WorkspaceGlobals {
                container_padding,
                workspace_padding,
                border_width,
                border_offset,
                work_area,
                work_area_offset,
                window_based_work_area_offset,
                window_based_work_area_offset_limit,
                floating_layer_behaviour,
            }
        }
    }

    pub fn add_container(
        &mut self,
        container: Container,
        workspace_idx: Option<usize>,
    ) -> eyre::Result<()> {
        let workspace = if let Some(idx) = workspace_idx {
            self.workspaces_mut()
                .get_mut(idx)
                .ok_or_eyre(format!("there is no workspace at index {idx}"))?
        } else {
            self.focused_workspace_mut()
                .ok_or_eyre("there is no workspace")?
        };

        workspace.add_container_to_back(container);

        Ok(())
    }

    /// Adds a container to this `Monitor` using the move direction to calculate if the container
    /// should be added in front of all containers, in the back or in place of the focused
    /// container, moving the rest along. The move direction should be from the origin monitor
    /// towards the target monitor or from the origin workspace towards the target workspace.
    pub fn add_container_with_direction(
        &mut self,
        container: Container,
        workspace_idx: Option<usize>,
        direction: OperationDirection,
    ) -> eyre::Result<()> {
        let workspace = if let Some(idx) = workspace_idx {
            self.workspaces_mut()
                .get_mut(idx)
                .ok_or_eyre(format!("there is no workspace at index {idx}"))?
        } else {
            self.focused_workspace_mut()
                .ok_or_eyre("there is no workspace")?
        };

        match direction {
            OperationDirection::Left => {
                // insert the container into the workspace on the monitor at the back (or rightmost position)
                // if we are moving across a boundary to the left (back = right side of the target)
                match workspace.layout {
                    Layout::Default(layout) => match layout {
                        DefaultLayout::RightMainVerticalStack => {
                            workspace.add_container_to_front(container);
                        }
                        DefaultLayout::UltrawideVerticalStack
                            if workspace.containers().len() == 1 =>
                        {
                            workspace.insert_container_at_idx(0, container);
                        }
                        _ => {
                            workspace.add_container_to_back(container);
                        }
                    },
                    Layout::Custom(_) => {
                        workspace.add_container_to_back(container);
                    }
                }
            }
            OperationDirection::Right => {
                // insert the container into the workspace on the monitor at the front (or leftmost position)
                // if we are moving across a boundary to the right (front = left side of the target)
                match workspace.layout {
                    Layout::Default(layout) => {
                        let target_index = layout.leftmost_index(workspace.containers().len());

                        match layout {
                            DefaultLayout::RightMainVerticalStack
                            | DefaultLayout::UltrawideVerticalStack
                                if workspace.containers().len() == 1 =>
                            {
                                workspace.add_container_to_back(container);
                            }
                            _ => {
                                workspace.insert_container_at_idx(target_index, container);
                            }
                        }
                    }
                    Layout::Custom(_) => {
                        workspace.add_container_to_front(container);
                    }
                }
            }
            OperationDirection::Up | OperationDirection::Down => {
                // insert the container into the workspace on the monitor at the position
                // where the currently focused container on that workspace is
                workspace.insert_container_at_idx(workspace.focused_container_idx(), container);
            }
        };

        Ok(())
    }

    pub fn remove_workspace_by_idx(&mut self, idx: usize) -> Option<Workspace> {
        if idx < self.workspaces().len() {
            return self.workspaces_mut().remove(idx);
        }

        if idx == 0 {
            self.workspaces_mut().push_back(Workspace::default());
        } else {
            self.focus_workspace(idx.saturating_sub(1)).ok()?;
        };

        None
    }

    pub fn ensure_workspace_count(&mut self, ensure_count: usize) {
        if self.workspaces().len() < ensure_count {
            self.workspaces_mut()
                .resize(ensure_count, Workspace::default());
        }
    }

    pub fn remove_workspaces(&mut self) -> VecDeque<Workspace> {
        self.workspaces_mut().drain(..).collect()
    }

    #[tracing::instrument(skip(self))]
    pub fn move_container_to_workspace(
        &mut self,
        target_workspace_idx: usize,
        follow: bool,
        direction: Option<OperationDirection>,
    ) -> eyre::Result<()> {
        let foreground_hwnd = WindowsApi::foreground_window()?;

        // A pinned window is already visible on every workspace of this
        // monitor, so there is nothing to move within the monitor.
        if self.is_pinned(foreground_hwnd) {
            return Ok(());
        }

        let workspace = self
            .focused_workspace_mut()
            .ok_or_eyre("there is no workspace")?;

        if workspace.maximized_window.is_some() {
            bail!("cannot move native maximized window to another monitor or workspace");
        }

        let floating_window_index = workspace
            .floating_windows()
            .iter()
            .position(|w| w.hwnd == foreground_hwnd);

        if let Some(idx) = floating_window_index {
            if let Some(window) = workspace.floating_windows_mut().remove(idx) {
                let workspaces = self.workspaces_mut();
                #[allow(clippy::option_if_let_else)]
                let target_workspace = match workspaces.get_mut(target_workspace_idx) {
                    None => {
                        workspaces.resize(target_workspace_idx + 1, Workspace::default());
                        workspaces.get_mut(target_workspace_idx).unwrap()
                    }
                    Some(workspace) => workspace,
                };

                target_workspace.floating_windows_mut().push_back(window);
                target_workspace.layer = WorkspaceLayer::Floating;
                target_workspace.layer_lock = false;
            }
        } else {
            if workspace
                .focused_container()
                .is_some_and(|container| container.windows().len() > 1)
            {
                // A stack is a group; only the focused window should move to the
                // target workspace (inverse of the stack command).
                workspace.new_container_for_focused_window()?;
            }

            let container = workspace
                .remove_focused_container()
                .ok_or_eyre("there is no container")?;

            let workspaces = self.workspaces_mut();

            #[allow(clippy::option_if_let_else)]
            let target_workspace = match workspaces.get_mut(target_workspace_idx) {
                None => {
                    workspaces.resize(target_workspace_idx + 1, Workspace::default());
                    workspaces.get_mut(target_workspace_idx).unwrap()
                }
                Some(workspace) => workspace,
            };

            if target_workspace.monocle_container.is_some() {
                for container in target_workspace.containers_mut() {
                    container.restore();
                }

                for window in target_workspace.floating_windows_mut() {
                    window.restore();
                }

                target_workspace.reintegrate_monocle_container()?;
            }

            target_workspace.layer = WorkspaceLayer::Tiling;
            target_workspace.layer_lock = false;

            if let Some(direction) = direction {
                self.add_container_with_direction(
                    container,
                    Some(target_workspace_idx),
                    direction,
                )?;
            } else {
                target_workspace.add_container_to_back(container);
            }
        }

        if follow {
            self.focus_workspace(target_workspace_idx)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn focus_workspace(&mut self, idx: usize) -> eyre::Result<()> {
        tracing::info!("focusing workspace");

        {
            let workspaces = self.workspaces_mut();

            if workspaces.get(idx).is_none() {
                workspaces.resize(idx + 1, Workspace::default());
            }
            self.last_focused_workspace = Some(self.workspaces.focused_idx());
            self.workspaces.focus(idx);
        }

        // Always set the latest known name when creating the workspace for the first time
        {
            let name = { self.workspace_names.get(&idx).cloned() };
            if name.is_some() {
                self.workspaces_mut()
                    .get_mut(idx)
                    .ok_or_eyre("there is no workspace")?
                    .name = name;
            }
        }

        Ok(())
    }

    pub fn new_workspace_idx(&self) -> usize {
        self.workspaces().len()
    }

    pub fn update_focused_workspace(&mut self, offset: Option<Rect>) -> eyre::Result<()> {
        let offset = if self.work_area_offset.is_some() {
            self.work_area_offset
        } else {
            offset
        };

        let focused_workspace_idx = self.focused_workspace_idx();
        self.update_workspace_globals(focused_workspace_idx, offset);
        self.focused_workspace_mut()
            .ok_or_eyre("there is no workspace")?
            .update()?;

        // Every update of the focused workspace can change whether it is empty
        // (a window added, moved away or destroyed), so re-evaluate whether the
        // monitor's pinned windows should be hidden: pins hide once the focused
        // workspace loses its last window and are restored once a new window
        // appears there. Idempotent and a no-op unless the `pinning` config
        // key's `hide_on_empty_workspaces` option is enabled.
        self.apply_pin_visibility()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_container() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // Add container to the default workspace
        m.add_container(Container::default(), Some(0)).unwrap();

        // Should contain a container in the current focused workspace
        let workspace = m.focused_workspace_mut().unwrap();
        assert_eq!(workspace.containers().len(), 1);
    }

    #[test]
    fn test_remove_workspace_by_idx() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        // Create workspace 2
        m.focus_workspace(new_workspace_index).unwrap();

        // Should have 2 workspaces
        assert_eq!(m.workspaces().len(), 2);

        // Create workspace 3
        m.focus_workspace(new_workspace_index + 1).unwrap();

        // Should have 3 workspaces
        assert_eq!(m.workspaces().len(), 3);

        // Remove workspace 1
        m.remove_workspace_by_idx(1);

        // Should have only 2 workspaces
        assert_eq!(m.workspaces().len(), 2);
    }

    #[test]
    fn test_remove_workspaces() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        // Create workspace 2
        m.focus_workspace(new_workspace_index).unwrap();

        // Should have 2 workspaces
        assert_eq!(m.workspaces().len(), 2);

        // Create workspace 3
        m.focus_workspace(new_workspace_index + 1).unwrap();

        // Should have 3 workspaces
        assert_eq!(m.workspaces().len(), 3);

        // Remove all workspaces
        m.remove_workspaces();

        // All workspaces should be removed
        assert_eq!(m.workspaces().len(), 0);
    }

    #[test]
    fn test_remove_nonexistent_workspace() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // Try to remove a workspace that doesn't exist
        let removed_workspace = m.remove_workspace_by_idx(1);

        // Should return None since there is no workspace at index 1
        assert!(removed_workspace.is_none());
    }

    #[test]
    fn test_focus_workspace() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        // Focus workspace 2
        m.focus_workspace(new_workspace_index).unwrap();

        // Should have 2 workspaces
        assert_eq!(m.workspaces().len(), 2);

        // Should be focused on workspace 2
        assert_eq!(m.focused_workspace_idx(), 1);
    }

    #[test]
    fn test_new_workspace_idx() {
        let m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let new_workspace_index = m.new_workspace_idx();

        // Should be the last workspace index: 1
        assert_eq!(new_workspace_index, 1);
    }

    #[test]
    fn test_move_container_to_workspace() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        {
            // Create workspace 1 and add 3 containers
            let workspace = m.focused_workspace_mut().unwrap();
            for _ in 0..3 {
                let container = Container::default();
                workspace.add_container_to_back(container);
            }

            // Should have 3 containers in workspace 1
            assert_eq!(m.focused_workspace().unwrap().containers().len(), 3);
        }

        // Create and focus workspace 2
        m.focus_workspace(new_workspace_index).unwrap();

        // Focus workspace 1
        m.focus_workspace(0).unwrap();

        // Move container to workspace 2
        m.move_container_to_workspace(1, true, None).unwrap();

        // Should be focused on workspace 2
        assert_eq!(m.focused_workspace_idx(), 1);

        // Workspace 2 should have 1 container now
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 1);

        // Move to workspace 1
        m.focus_workspace(0).unwrap();

        // Workspace 1 should have 2 containers
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 2);

        // Move a another container from workspace 1 to workspace 2 without following
        m.move_container_to_workspace(1, false, None).unwrap();

        // Should have 1 container
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 1);

        // Should still be focused on workspace 1
        assert_eq!(m.focused_workspace_idx(), 0);

        // Switch to workspace 2
        m.focus_workspace(1).unwrap();

        // Workspace 2 should now have 2 containers
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 2);
    }

    #[test]
    fn test_move_container_to_workspace_moves_only_focused_window_from_stack() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        {
            // Create workspace 1 with a stack of two windows and one single-window container
            let workspace = m.focused_workspace_mut().unwrap();

            let mut stack = Container::default();
            stack.windows_mut().push_back(Window::from(0));
            stack.windows_mut().push_back(Window::from(1));
            workspace.add_container_to_back(stack);

            let mut single = Container::default();
            single.windows_mut().push_back(Window::from(2));
            workspace.add_container_to_back(single);

            workspace.focus_container(0);

            assert_eq!(workspace.containers().len(), 2);
            assert_eq!(workspace.focused_container_idx(), 0);
        }

        // Move the focused window out of the stack to workspace 2
        m.move_container_to_workspace(1, false, None).unwrap();

        // Source workspace keeps the remaining stack [1] and the single container [2]
        assert_eq!(m.focused_workspace_idx(), 0);
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 2);
        let source_stack = m.focused_workspace().unwrap().containers().front().unwrap();
        assert_eq!(source_stack.windows().len(), 1);
        assert!(source_stack.contains_window(1));

        // Target workspace has only the focused window [0]
        m.focus_workspace(1).unwrap();
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 1);
        let moved = m.focused_workspace().unwrap().containers().front().unwrap();
        assert_eq!(moved.windows().len(), 1);
        assert!(moved.contains_window(0));
    }

    #[test]
    fn test_move_container_to_nonexistent_workspace() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        {
            // Create workspace 1 and add 3 containers
            let workspace = m.focused_workspace_mut().unwrap();
            for _ in 0..3 {
                let container = Container::default();
                workspace.add_container_to_back(container);
            }

            // Should have 3 containers in workspace 1
            assert_eq!(m.focused_workspace().unwrap().containers().len(), 3);
        }

        // Should only have 1 workspace
        assert_eq!(m.workspaces().len(), 1);

        // Try to move a container to a workspace that doesn't exist
        m.move_container_to_workspace(8, true, None).unwrap();

        // Should have 9 workspaces now
        assert_eq!(m.workspaces().len(), 9);

        // Should be focused on workspace 8
        assert_eq!(m.focused_workspace_idx(), 8);

        // Should have 1 container in workspace 8
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 1);
    }

    #[test]
    fn test_ensure_workspace_count_workspace_contains_two_workspaces() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // Create and focus another workspace
        let new_workspace_index = m.new_workspace_idx();
        m.focus_workspace(new_workspace_index).unwrap();

        // Should have 2 workspaces now
        assert_eq!(m.workspaces().len(), 2, "Monitor should have 2 workspaces");

        // Ensure the monitor has at least 5 workspaces
        m.ensure_workspace_count(5);

        // Monitor should have 5 workspaces
        assert_eq!(m.workspaces().len(), 5, "Monitor should have 5 workspaces");
    }

    #[test]
    fn test_ensure_workspace_count_only_default_workspace() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // Ensure the monitor has at least 5 workspaces
        m.ensure_workspace_count(5);

        // Monitor should have 5 workspaces
        assert_eq!(m.workspaces().len(), 5, "Monitor should have 5 workspaces");

        // Try to call the ensure workspace count again to ensure it doesn't change
        m.ensure_workspace_count(3);
        assert_eq!(m.workspaces().len(), 5, "Monitor should have 5 workspaces");
    }

    #[test]
    fn test_ignored_window_candidate_excludes_managed_windows() {
        // Unmanaged normal / fullscreen / widget windows are candidates.
        assert!(Monitor::is_ignored_window_candidate(
            false, true, false, false
        ));
        assert!(Monitor::is_ignored_window_candidate(
            false, false, true, false
        ));
        assert!(Monitor::is_ignored_window_candidate(
            false, false, false, true
        ));

        // A managed window (e.g. a browser that dropped its caption during
        // HTML5 fullscreen) must never enter the ignored window layer.
        assert!(!Monitor::is_ignored_window_candidate(
            true, false, true, false
        ));
        assert!(!Monitor::is_ignored_window_candidate(
            true, true, false, false
        ));

        // System shell surfaces stay excluded.
        assert!(!Monitor::is_ignored_window_candidate(
            false, false, false, false
        ));
    }

    #[test]
    fn test_should_auto_demote_excludes_topmost_widgets() {
        // Games / unmanaged application windows can always be moved by the
        // manual `toggle-ignored-window-layer` command.
        let is_widget = false;
        assert!(Monitor::should_auto_demote_predicate(is_widget, false));
        assert!(Monitor::should_auto_demote_predicate(is_widget, true));

        // Non-topmost widgets (e.g. Rainmeter meters) can still be toggled above
        // or below the managed layer.
        assert!(Monitor::should_auto_demote_predicate(true, false));

        // Topmost widgets (e.g. a yasb bar with always_on_top) are never moved.
        assert!(!Monitor::should_auto_demote_predicate(true, true));
    }

    #[test]
    fn test_should_steal_ignored_window_foreground() {
        // Widgets (bars, popups, meters) always steal so the layer stays intact.
        assert!(Monitor::should_steal_ignored_window_foreground(true, false));
        assert!(Monitor::should_steal_ignored_window_foreground(true, true));

        // Fullscreen games / unmanaged windows only steal while the switch's
        // event storm is still settling.
        assert!(Monitor::should_steal_ignored_window_foreground(false, true));

        // Outside the stabilization window the game keeps its foreground:
        // stealing it there makes the game randomly lose focus during gameplay.
        assert!(!Monitor::should_steal_ignored_window_foreground(
            false, false
        ));
    }

    #[test]
    fn test_toggle_pin_always_on_top() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // Always-on-top only applies to already-pinned windows
        m.pin_floating_window(10);
        assert!(!m.is_pinned_always_on_top(10));

        m.toggle_pin_always_on_top(10);
        assert!(m.is_pinned_always_on_top(10));

        // Toggling again removes the always-on-top state
        m.toggle_pin_always_on_top(10);
        assert!(!m.is_pinned_always_on_top(10));

        // Re-assert then unpin: always-on-top state must be cleaned up
        m.toggle_pin_always_on_top(10);
        m.unpin_floating_window(10);
        assert!(!m.is_pinned(10));
        assert!(!m.is_pinned_always_on_top(10));
    }

    #[test]
    fn test_apply_pin_visibility_gated_by_config() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );
        m.pin_floating_window(10);

        // Without the `pinning` config key (default) the method is a no-op
        // and the pin list is untouched
        HIDE_PINNED_ON_EMPTY_WORKSPACES.store(false, Ordering::SeqCst);
        m.apply_pin_visibility().unwrap();
        assert_eq!(m.pinned_floating, vec![10]);

        // With the feature enabled the loop over pins runs (skipping
        // non-window HWNDs) and never mutates the pin list
        HIDE_PINNED_ON_EMPTY_WORKSPACES.store(true, Ordering::SeqCst);
        m.apply_pin_visibility().unwrap();
        assert_eq!(m.pinned_floating, vec![10]);

        // A non-empty focused workspace also leaves the pins intact
        m.focused_workspace_mut()
            .unwrap()
            .add_container_to_back(Container::default());
        m.apply_pin_visibility().unwrap();
        assert_eq!(m.pinned_floating, vec![10]);

        HIDE_PINNED_ON_EMPTY_WORKSPACES.store(false, Ordering::SeqCst);
    }

    #[test]
    fn test_update_focused_workspace_rechecks_pin_visibility() {
        let mut m = Monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );
        m.pin_floating_window(10);

        // update_focused_workspace re-evaluates hide-on-empty visibility on
        // every focused-workspace update (a window added, moved away or
        // destroyed), so the pin set must survive the pass whether the focused
        // workspace is empty or populated.
        HIDE_PINNED_ON_EMPTY_WORKSPACES.store(true, Ordering::SeqCst);

        // Empty focused workspace: the empty-workspace hide pass must not
        // mutate the pin set.
        m.update_focused_workspace(None).unwrap();
        assert_eq!(m.pinned_floating, vec![10]);

        // Populated focused workspace: the restore pass is a no-op for
        // non-window pin HWNDs and must not mutate the pin set either.
        m.focused_workspace_mut()
            .unwrap()
            .add_container_to_back(Container::default());
        m.update_focused_workspace(None).unwrap();
        assert_eq!(m.pinned_floating, vec![10]);

        HIDE_PINNED_ON_EMPTY_WORKSPACES.store(false, Ordering::SeqCst);
    }
}
