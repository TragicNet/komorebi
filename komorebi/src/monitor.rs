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

use crate::border_manager::BORDER_ENABLED;
use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_WIDTH;
use crate::core::Rect;

use crate::DEFAULT_CONTAINER_PADDING;
use crate::DEFAULT_WORKSPACE_PADDING;
use crate::DefaultLayout;
use crate::FloatingLayerBehaviour;
use crate::LOWER_IGNORED_WINDOWS_ON_FOCUS;
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
                // hide() skips pinned floating windows, which remain visible across
                // all workspaces on this monitor
                workspace.hide(None);
                workspace.restore_pinned();
            }
        }

        // Re-establish the workspace layer stack on this monitor so the whole
        // tiling/floating base is drawn above the floating overlay and the
        // ignored (unmanaged widget/fullscreen) windows after a workspace or
        // monitor switch. Skipped when the ignored windows have been manually
        // raised above managed or there are no managed windows to cover.
        self.enforce_layer_stack()?;

        // A workspace/monitor switch always re-establishes the layer: demote
        // ignored windows below the managed base so a fullscreen game can never
        // occlude the status bar, regardless of the focus-only lowering config.
        self.demote_ignored_windows_on_switch()?;

        Ok(())
    }

    /// Re-establishes the workspace layer stack for the focused workspace so
    /// that the layering is consistent after any transition (workspace switch,
    /// monitor switch, focus-driven layer flip):
    ///
    /// ```text
    /// bottom -> top: ignored windows < base layer < top layer < focused window
    /// ```
    ///
    /// In `Tiling` mode the floating windows form the base and the tiled windows
    /// the top layer; in `Floating` mode the layers are reversed. Ignored
    /// (unmanaged widget / fullscreen) windows are only demoted when
    /// `LOWER_IGNORED_WINDOWS_ON_FOCUS` is enabled and the workspace is
    /// non-empty, and the manual `ignored_windows_above_managed` layer toggle is
    /// always respected.
    #[tracing::instrument(skip(self))]
    pub fn enforce_layer_stack(&self) -> eyre::Result<()> {
        let Some(workspace) = self.focused_workspace() else {
            return Ok(());
        };

        // The manual `ignored_windows_above_managed` layer toggle is always
        // respected: the user is in charge of the layer stack, including where
        // the pinned windows sit.
        if workspace.ignored_windows_above_managed {
            return Ok(());
        }

        // Fullscreen (monocle / maximized) windows manage their own stacking, so
        // only make sure ignored windows do not occlude them and that pinned
        // windows stay in their band below.
        if workspace.monocle_container.is_some() || workspace.maximized_window.is_some() {
            self.lower_ignored_windows_on_focus()?;
            self.reposition_pinned_windows(workspace.layer)?;
            return Ok(());
        }

        if workspace.is_empty() {
            self.reposition_pinned_windows(workspace.layer)?;
            return Ok(());
        }

        let focused_window = match workspace.layer {
            WorkspaceLayer::Tiling => workspace
                .focused_container()
                .and_then(|container| container.focused_window())
                .copied(),
            WorkspaceLayer::Floating => workspace.focused_floating_window().copied(),
        };

        match workspace.layer {
            WorkspaceLayer::Tiling => {
                // Floating windows form the base, tiled windows the top layer.
                for window in workspace.floating_windows() {
                    self.raise_managed_window(window);
                }
                for window in workspace.containers().iter().rev() {
                    if let Some(window) = window.focused_window() {
                        self.raise_managed_window(window);
                    }
                }
            }
            WorkspaceLayer::Floating => {
                // Tiled windows form the base, floating windows the top layer.
                for window in workspace.containers() {
                    if let Some(window) = window.focused_window() {
                        self.raise_managed_window(window);
                    }
                }
                for window in workspace.floating_windows().iter().rev() {
                    self.raise_managed_window(window);
                }
            }
        }

        // Keep the focused window of the top layer on the very top.
        if let Some(window) = focused_window {
            self.raise_managed_window(&window);
        }

        // Demote ignored windows below the managed windows as the final z-order
        // operation, so that desktop widgets and unmanaged fullscreen windows can
        // never end up above the base or top layer regardless of the async
        // SetWindowPos ordering. This mirrors the ordering used by the working
        // ToggleWorkspaceLayer flow, which lowers ignored windows last.
        self.lower_ignored_windows_on_focus()?;

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

        // Pinned floating windows from other workspaces on this monitor belong
        // to the focused workspace's layer band: they join the Floating overlay
        // when the layer is Floating, and sit in the floating base below the
        // tiled top layer when it is Tiling.
        self.reposition_pinned_windows(workspace.layer)?;

        Ok(())
    }

    /// Places the pinned floating windows of all workspaces on this monitor in
    /// the layer band belonging to the focused workspace's layer: they join the
    /// Floating overlay when the layer is Floating, and sit in the floating base
    /// below the tiled top layer when it is Tiling.
    ///
    /// Uses synchronous z-order operations, so the placement is fully applied
    /// before this returns regardless of `WINDOW_HANDLING_BEHAVIOUR`: every
    /// raise issued before it ends up above the pinned windows, and no later
    /// operation can push them back over the top layer.
    fn reposition_pinned_windows(&self, layer: WorkspaceLayer) -> eyre::Result<()> {
        match layer {
            WorkspaceLayer::Floating => self.raise_pinned_windows(),
            WorkspaceLayer::Tiling => self.lower_pinned_windows(),
        };

        Ok(())
    }

    /// Raise the pinned floating windows of all workspaces on this monitor so
    /// they join the Floating overlay when a workspace layer is toggled to
    /// Floating. Non-activating so the focused workspace's window keeps focus.
    /// Applied synchronously so the overlay placement is deterministic.
    pub fn raise_pinned_windows(&self) {
        for workspace in self.workspaces() {
            for window in workspace.pinned_floating_windows() {
                if let Err(error) = window.raise_sync() {
                    tracing::warn!(
                        hwnd = window.hwnd,
                        exe = window.exe().unwrap_or_default(),
                        title = window.title().unwrap_or_default(),
                        "could not raise pinned window: {error}"
                    );
                }
            }
        }
    }

    /// Lower the pinned floating windows of all workspaces on this monitor so
    /// they drop back behind the tiling base when a workspace layer is toggled
    /// back to Tiling. Applied synchronously so they can never end up above the
    /// tiled windows regardless of the async SetWindowPos ordering.
    pub fn lower_pinned_windows(&self) {
        for workspace in self.workspaces() {
            for window in workspace.pinned_floating_windows() {
                if let Err(error) = window.lower_sync() {
                    tracing::warn!(
                        hwnd = window.hwnd,
                        exe = window.exe().unwrap_or_default(),
                        title = window.title().unwrap_or_default(),
                        "could not lower pinned window: {error}"
                    );
                }
            }
        }
    }

    fn raise_managed_window(&self, window: &Window) {
        if let Err(error) = window.raise() {
            tracing::warn!(
                hwnd = window.hwnd,
                exe = window.exe().unwrap_or_default(),
                title = window.title().unwrap_or_default(),
                "could not raise managed window: {error}"
            );
        }
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

    /// Lower every ignored window on this monitor below the managed windows, so
    /// that unmanaged windows (e.g. desktop widgets or fullscreen games) never
    /// visually occlude the tiling or floating base layer.
    pub fn lower_ignored_windows(&self) -> eyre::Result<()> {
        self.lower_ignored_windows_filtered(|_| true)
    }

    /// Shared lowering implementation, restricted to the windows that satisfy
    /// `should_lower`.
    ///
    /// Each SetWindowPos(HWND_BOTTOM) call pushes the window below all the
    /// ones lowered before it, so the ordering below controls the stacking
    /// between the ignored windows themselves. Unmanaged fullscreen games
    /// and application windows are lowered first so they end up above the
    /// desktop widgets (e.g. Rainmeter meters), which are lowered last and
    /// therefore end up at the very bottom. Within each group the largest
    /// window is lowered first so it remains highest, keeping a fullscreen
    /// game above a near-fullscreen visualizer of the same footprint.
    fn lower_ignored_windows_filtered(
        &self,
        should_lower: impl Fn(&Window) -> bool,
    ) -> eyre::Result<()> {
        let mut ignored_windows = self.ignored_windows();
        ignored_windows.sort_by_key(|window| {
            let is_widget = window.is_widget_window();
            let area = WindowsApi::window_rect(window.hwnd)
                .map(|rect| (rect.right - rect.left).max(0) * (rect.bottom - rect.top).max(0))
                .unwrap_or_default();
            (is_widget, std::cmp::Reverse(area))
        });

        for window in ignored_windows {
            if !should_lower(&window) {
                continue;
            }

            // Never demote a widget window the cursor is currently over: it is
            // being interacted with (e.g. a yasb popup or status bar), and
            // dropping it below the managed base would swallow the user's
            // clicks or pull the pointer onto a managed window beneath it.
            // Fullscreen games and regular application windows are always
            // demoted, even under the cursor, so they can never occlude the
            // status bar.
            if window.is_widget_window()
                && WindowsApi::window_at_cursor_pos()
                    .ok()
                    .is_some_and(|hwnd| hwnd == window.hwnd)
            {
                tracing::trace!(
                    hwnd = window.hwnd,
                    exe = window.exe().unwrap_or_default(),
                    title = window.title().unwrap_or_default(),
                    "skipping ignored widget window under the cursor"
                );
                continue;
            }

            if let Err(error) = window.lower() {
                tracing::warn!(
                    hwnd = window.hwnd,
                    exe = window.exe().unwrap_or_default(),
                    title = window.title().unwrap_or_default(),
                    "could not lower ignored window: {error}"
                );
            }
        }

        Ok(())
    }

    /// Whether an ignored window should be demoted below the managed base by an
    /// automatic (focus/switch) demotion. Games and unmanaged application
    /// windows are always demoted. Widget windows pinned to the topmost band
    /// (e.g. a yasb bar with always_on_top) are left untouched so frequent
    /// switches never flicker them; non-topmost widgets (e.g. Rainmeter meters)
    /// are still demoted so they cannot occlude tiled windows.
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
    fn within_switch_stabilization(&self) -> bool {
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

    /// When `LOWER_IGNORED_WINDOWS_ON_FOCUS` is enabled, lower ignored windows
    /// on this monitor below the managed windows of the newly focused workspace.
    /// This prevents unmanaged fullscreen windows (e.g. games) and desktop
    /// widgets from occluding tiled windows when switching workspaces.
    ///
    /// Lowering is skipped for empty workspaces so that focusing a workspace
    /// without any managed windows still shows the ignored windows on top, and
    /// it respects the manual `ignored_windows_above_managed` layer toggle.
    /// Topmost widget windows (status bars) are never demoted here.
    fn lower_ignored_windows_on_focus(&self) -> eyre::Result<()> {
        if !LOWER_IGNORED_WINDOWS_ON_FOCUS.load(Ordering::SeqCst) {
            return Ok(());
        }

        let Some(workspace) = self.focused_workspace() else {
            return Ok(());
        };
        if workspace.is_empty() || workspace.ignored_windows_above_managed {
            return Ok(());
        }

        self.lower_ignored_windows_filtered(Self::should_auto_demote)
    }

    /// Re-establishes the ignored window layer after a workspace or monitor
    /// switch, mirroring the `ToggleWorkspaceLayer` behavior so that the layer
    /// stack is consistent without relying on `LOWER_IGNORED_WINDOWS_ON_FOCUS`
    /// (which defaults to disabled).
    ///
    /// Covering windows (unmanaged fullscreen games and regular application
    /// windows) are always demoted below the managed base so they can never
    /// occlude the status bar. Topmost widget windows (status bars) are never
    /// touched so switches do not flicker them. On an empty workspace desktop
    /// widgets stay floating on top of the desktop, unless an unmanaged
    /// covering window (e.g. a fullscreen game) is up on the monitor - in that
    /// case it is demoted first so it sits above the widgets instead of hidden
    /// behind them. The manual `ignored_windows_above_managed` layer toggle is
    /// always respected.
    fn demote_ignored_windows_on_switch(&self) -> eyre::Result<()> {
        let Some(workspace) = self.focused_workspace() else {
            return Ok(());
        };
        if workspace.ignored_windows_above_managed {
            return Ok(());
        }

        if workspace.is_empty() {
            // Empty workspace: keep desktop widgets floating on top of the
            // empty desktop, unless an unmanaged covering window is up. A
            // fullscreen game must never be left behind a non-topmost widget,
            // so in that case demote it (and the non-topmost widgets) too.
            if self
                .ignored_windows()
                .iter()
                .any(|window| !window.is_widget_window())
            {
                self.lower_ignored_windows_filtered(Self::should_auto_demote)
            } else {
                Ok(())
            }
        } else {
            self.lower_ignored_windows_filtered(Self::should_auto_demote)
        }
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
        let workspace = self
            .focused_workspace_mut()
            .ok_or_eyre("there is no workspace")?;

        if workspace.maximized_window.is_some() {
            bail!("cannot move native maximized window to another monitor or workspace");
        }

        let foreground_hwnd = WindowsApi::foreground_window()?;
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
        let source_stack = m
            .focused_workspace()
            .unwrap()
            .containers()
            .front()
            .unwrap();
        assert_eq!(source_stack.windows().len(), 1);
        assert!(source_stack.contains_window(1));

        // Target workspace has only the focused window [0]
        m.focus_workspace(1).unwrap();
        assert_eq!(m.focused_workspace().unwrap().containers().len(), 1);
        let moved = m
            .focused_workspace()
            .unwrap()
            .containers()
            .front()
            .unwrap();
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
        assert!(Monitor::is_ignored_window_candidate(false, true, false, false));
        assert!(Monitor::is_ignored_window_candidate(false, false, true, false));
        assert!(Monitor::is_ignored_window_candidate(false, false, false, true));

        // A managed window (e.g. a browser that dropped its caption during
        // HTML5 fullscreen) must never enter the ignored window layer.
        assert!(!Monitor::is_ignored_window_candidate(true, false, true, false));
        assert!(!Monitor::is_ignored_window_candidate(true, true, false, false));

        // System shell surfaces stay excluded.
        assert!(!Monitor::is_ignored_window_candidate(false, false, false, false));
    }

    #[test]
    fn test_should_auto_demote_excludes_topmost_widgets() {
        // Games / unmanaged application windows are always demoted.
        let is_widget = false;
        assert!(Monitor::should_auto_demote_predicate(
            is_widget,
            false
        ));
        assert!(Monitor::should_auto_demote_predicate(is_widget, true));

        // Non-topmost widgets (e.g. Rainmeter meters) are still demoted so they
        // cannot occlude tiled windows.
        assert!(Monitor::should_auto_demote_predicate(true, false));

        // Topmost widgets (e.g. a yasb bar with always_on_top) are never
        // demoted by automatic focus/switch demotions.
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
        assert!(!Monitor::should_steal_ignored_window_foreground(false, false));
    }
}
