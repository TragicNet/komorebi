use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;

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
        workspace_names: HashMap::default(),
        container_padding: None,
        workspace_padding: None,
        wallpaper: None,
        floating_layer_behaviour: None,
    }
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
        let focused_idx = self.focused_workspace_idx();
        let hmonitor = self.id;
        let monitor_wp = self.wallpaper.clone();
        for (i, workspace) in self.workspaces_mut().iter_mut().enumerate() {
            if i == focused_idx {
                workspace.restore(mouse_follows_focus, trigger_focus, hmonitor, &monitor_wp)?;
            } else {
                workspace.hide(None);
            }
        }

        // Re-establish the workspace layer stack on this monitor so the whole
        // tiling/floating base is drawn above the floating overlay and the
        // ignored (unmanaged widget/fullscreen) windows after a workspace or
        // monitor switch. Skipped when the ignored windows have been manually
        // raised above managed or there are no managed windows to cover.
        self.enforce_layer_stack()?;

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

        // Fullscreen (monocle / maximized) windows manage their own stacking, so
        // only make sure ignored windows do not occlude them.
        if workspace.monocle_container.is_some() || workspace.maximized_window.is_some() {
            self.lower_ignored_windows_on_focus()?;
            return Ok(());
        }

        if workspace.is_empty() || workspace.ignored_windows_above_managed {
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

        // If an ignored window was auto-promoted to the foreground while the
        // switch was in flight (Windows draws foreground windows above everything
        // else), hand focus back to the top-layer window so the layer stack is
        // not visually reverted once the event storm settles.
        if let Some(foreground) = WindowsApi::foreground_window().ok()
            && self
                .ignored_windows()
                .iter()
                .any(|window| window.hwnd == foreground)
            && let Some(window) = focused_window
        {
            tracing::debug!(
                hwnd = foreground,
                "ignored window auto-promoted to foreground during switch, restoring managed focus",
            );
            let _ = WindowsApi::raise_and_focus_window(window.hwnd);
        }

        Ok(())
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
                let is_fullscreen = !is_normal && window.is_fullscreen();
                let is_widget = !is_normal && !is_fullscreen && window.is_widget_window();

                if is_normal || is_fullscreen || is_widget {
                    tracing::debug!(
                        hwnd = window.hwnd,
                        exe = window.exe().unwrap_or_default(),
                        title = window.title().unwrap_or_default(),
                        is_normal_application_window = is_normal,
                        is_fullscreen = is_fullscreen,
                        is_widget_window = is_widget,
                        "ignored window layer candidate on monitor"
                    );
                }

                is_normal || is_fullscreen || is_widget
            })
            .collect()
    }

    /// Lower every ignored window on this monitor below the managed windows, so
    /// that unmanaged windows (e.g. desktop widgets or fullscreen games) never
    /// visually occlude the tiling or floating base layer.
    pub fn lower_ignored_windows(&self) -> eyre::Result<()> {
        for window in self.ignored_windows() {
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

    /// When `LOWER_IGNORED_WINDOWS_ON_FOCUS` is enabled, lower ignored windows
    /// on this monitor below the managed windows of the newly focused workspace.
    /// This prevents unmanaged fullscreen windows (e.g. games) and desktop
    /// widgets from occluding tiled windows when switching workspaces.
    ///
    /// Lowering is skipped for empty workspaces so that focusing a workspace
    /// without any managed windows still shows the ignored windows on top, and
    /// it respects the manual `ignored_windows_above_managed` layer toggle.
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

        self.lower_ignored_windows()
    }

    /// Updates the `globals` field of all workspaces
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
            }
        } else {
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
}
