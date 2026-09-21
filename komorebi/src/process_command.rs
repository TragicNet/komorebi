use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use color_eyre::eyre::WrapErr;
use komorebi_themes::colour::Rgb;
use miow::pipe::connect;
use net2::TcpStreamExt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::net::TcpListener;
use std::net::TcpStream;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use uds_windows::UnixStream;

use crate::CUSTOM_FFM;
use crate::DATA_DIR;
use crate::DISPLAY_INDEX_PREFERENCES;
use crate::FLOATING_APPLICATIONS;
use crate::HIDING_BEHAVIOUR;
use crate::IGNORE_IDENTIFIERS;
use crate::INITIAL_CONFIGURATION_LOADED;
use crate::LAYERED_WHITELIST;
use crate::MANAGE_IDENTIFIERS;
use crate::MONITOR_INDEX_PREFERENCES;
use crate::NO_TITLEBAR;
use crate::NotificationEvent;
use crate::OBJECT_NAME_CHANGE_ON_LAUNCH;
use crate::REMOVE_TITLEBARS;
use crate::SESSION_FLOATING_APPLICATIONS;
use crate::SUBSCRIPTION_PIPES;
use crate::SUBSCRIPTION_SOCKET_OPTIONS;
use crate::SUBSCRIPTION_SOCKETS;
use crate::TCP_CONNECTIONS;
use crate::TRAY_AND_MULTI_WINDOW_IDENTIFIERS;
use crate::WINDOWS_11;
use crate::WORKSPACE_MATCHING_RULES;
use crate::animation::ANIMATION_DURATION_GLOBAL;
use crate::animation::ANIMATION_DURATION_PER_ANIMATION;
use crate::animation::ANIMATION_ENABLED_GLOBAL;
use crate::animation::ANIMATION_ENABLED_PER_ANIMATION;
use crate::animation::ANIMATION_FPS;
use crate::animation::ANIMATION_STYLE_GLOBAL;
use crate::animation::ANIMATION_STYLE_PER_ANIMATION;
use crate::apply_worker::ApplyWorker;
use crate::border_manager;
use crate::border_manager::IMPLEMENTATION;
use crate::border_manager::STYLE;
use crate::build;
use crate::config_generation::WorkspaceMatchingRule;
use crate::core::ApplicationIdentifier;
use crate::core::Axis;
use crate::core::BorderImplementation;
use crate::core::FocusFollowsMouseImplementation;
use crate::core::Layout;
use crate::core::LayoutOptions;
use crate::core::MonocleFocusBehaviour;
use crate::core::MoveBehaviour;
use crate::core::OperationDirection;
use crate::core::Rect;
use crate::core::ScrollingLayoutOptions;
use crate::core::Sizing;
use crate::core::CycleFocusWindowContent;
use crate::core::SocketMessage;
use crate::core::StateQuery;
use crate::core::WindowContainerBehaviour;
use crate::core::WindowKind;
use crate::core::config_generation::IdWithIdentifier;
use crate::core::config_generation::MatchingRule;
use crate::core::config_generation::MatchingStrategy;
use crate::current_virtual_desktop;
use crate::monitor::MonitorInformation;
use crate::HIDE_PINNED_ON_EMPTY_WORKSPACES;
use crate::notify_subscribers;
use crate::stackbar_manager;
use crate::stackbar_manager::STACKBAR_FONT_FAMILY;
use crate::stackbar_manager::STACKBAR_FONT_SIZE;
use crate::state;
use crate::state::GlobalState;
use crate::state::State;
use crate::static_config::StaticConfig;
use crate::theme_manager;
use crate::transparency_manager;
use crate::window::RuleDebug;
use crate::window::Window;
use crate::window_manager::WindowManager;
use crate::windows_api::WindowsApi;
use crate::winevent_listener;
use crate::workspace::WorkspaceLayer;
use crate::workspace::WorkspaceWindowLocation;
use stackbar_manager::STACKBAR_FOCUSED_TEXT_COLOUR;
use stackbar_manager::STACKBAR_LABEL;
use stackbar_manager::STACKBAR_MODE;
use stackbar_manager::STACKBAR_TAB_BACKGROUND_COLOUR;
use stackbar_manager::STACKBAR_TAB_HEIGHT;
use stackbar_manager::STACKBAR_TAB_WIDTH;
use stackbar_manager::STACKBAR_UNFOCUSED_TEXT_COLOUR;

#[tracing::instrument]
pub fn listen_for_commands(wm: Arc<Mutex<WindowManager>>) {
    std::thread::spawn(move || {
        loop {
            let wm = wm.clone();

            let _ = std::thread::spawn(move || {
                let listener = wm
                    .lock()
                    .command_listener
                    .try_clone()
                    .expect("could not clone unix listener");

                tracing::info!("listening on komorebi.sock");
                for client in listener.incoming() {
                    match client {
                        Ok(stream) => {
                            let wm_clone = wm.clone();
                            std::thread::spawn(move || {
                                match stream.set_read_timeout(Some(Duration::from_secs(1))) {
                                    Ok(()) => {}
                                    Err(error) => tracing::error!("{}", error),
                                }
                                match read_commands_uds(&wm_clone, stream) {
                                    Ok(()) => {}
                                    Err(error) => tracing::error!("{}", error),
                                }
                            });
                        }
                        Err(error) => {
                            tracing::error!("{}", error);
                            break;
                        }
                    }
                }
            })
            .join();

            tracing::error!("restarting failed thread");
        }
    });
}

#[tracing::instrument]
pub fn listen_for_commands_tcp(wm: Arc<Mutex<WindowManager>>, port: usize) {
    let listener =
        TcpListener::bind(format!("0.0.0.0:{port}")).expect("could not start tcp server");

    std::thread::spawn(move || {
        tracing::info!("listening on 0.0.0.0:43663");
        for client in listener.incoming() {
            match client {
                Ok(mut stream) => {
                    stream
                        .set_keepalive(Some(Duration::from_secs(30)))
                        .expect("TCP keepalive should be set");

                    let addr = stream
                        .peer_addr()
                        .expect("incoming connection should have an address")
                        .to_string();

                    let mut connections = TCP_CONNECTIONS.lock();

                    connections.insert(
                        addr.clone(),
                        stream.try_clone().expect("stream should be cloneable"),
                    );

                    tracing::info!("listening for incoming tcp messages from {}", &addr);

                    match read_commands_tcp(&wm, &mut stream, &addr) {
                        Ok(()) => {}
                        Err(error) => tracing::error!("{}", error),
                    }
                }
                Err(error) => {
                    tracing::error!("{}", error);
                    break;
                }
            }
        }
    });
}

impl WindowManager {
    // TODO(raggi): wrap reply in a newtype that can decorate a human friendly
    // name for the peer, such as getting the pid of the komorebic process for
    // the UDS or the IP:port for TCP.
    #[tracing::instrument(skip(self, reply))]
    pub fn process_command(
        &mut self,
        message: SocketMessage,
        mut reply: impl std::io::Write,
    ) -> eyre::Result<()> {
        if let Some(virtual_desktop_id) = &self.virtual_desktop_id
            && let Some(id) = current_virtual_desktop()
            && id != *virtual_desktop_id
        {
            tracing::info!(
                "ignoring events and commands while not on virtual desktop {:?}",
                virtual_desktop_id
            );
            return Ok(());
        }

        #[allow(clippy::useless_asref)]
        // We don't have From implemented for &mut WindowManager
        let initial_state = State::from(self.as_ref());

        let mut force_update_borders = false;
        match message {
            SocketMessage::Promote => self.promote_container_to_front()?,
            SocketMessage::PromoteSwap => self.promote_container_swap()?,
            SocketMessage::PromoteFocus => self.promote_focus_to_front()?,
            SocketMessage::PromoteWindow(direction) => {
                self.focus_container_in_direction(direction)?;
                self.promote_container_to_front()?
            }
            SocketMessage::EagerFocus(ref exe) => {
                let focused_monitor_idx = self.focused_monitor_idx();

                let mut window_location = None;
                let mut monitor_to_focus = None;
                let mut needs_workspace_loading = false;

                'search: for (monitor_idx, monitor) in self.monitors_mut().iter_mut().enumerate() {
                    for (workspace_idx, workspace) in monitor.workspaces().iter().enumerate() {
                        if let Some(location) = workspace.location_from_exe(exe) {
                            window_location = Some(location);

                            if monitor_idx != focused_monitor_idx {
                                monitor_to_focus = Some(monitor_idx);
                            }

                            // Focus workspace if it is not already the focused one, without
                            // loading it so that we don't give focus to the wrong window, we will
                            // load it later after focusing the wanted window
                            let focused_ws_idx = monitor.focused_workspace_idx();
                            if focused_ws_idx != workspace_idx {
                                monitor.last_focused_workspace = Option::from(focused_ws_idx);
                                monitor.focus_workspace(workspace_idx)?;
                                needs_workspace_loading = true;
                            }

                            break 'search;
                        }
                    }
                }

                if let Some(monitor_idx) = monitor_to_focus {
                    self.focus_monitor(monitor_idx)?;
                }

                if let Some(location) = window_location {
                    match location {
                        WorkspaceWindowLocation::Monocle(window_idx) => {
                            self.focus_container_window(window_idx)?;
                        }
                        WorkspaceWindowLocation::Maximized => {
                            if let Some(window) =
                                &mut self.focused_workspace_mut()?.maximized_window
                            {
                                window.focus(self.mouse_follows_focus)?;
                            }
                        }
                        WorkspaceWindowLocation::Container(container_idx, window_idx) => {
                            let focused_container_idx = self.focused_container_idx()?;
                            if container_idx != focused_container_idx {
                                self.focused_workspace_mut()?.focus_container(container_idx);
                            }

                            self.focus_container_window(window_idx)?;
                        }
                        WorkspaceWindowLocation::Floating(window_idx) => {
                            let workspace = self.focused_workspace_mut()?;
                            if workspace.focus_floating_window(window_idx)
                                && let Some(window) = workspace.floating_windows_mut().get_mut(window_idx)
                            {
                                window.focus(self.mouse_follows_focus)?;
                            }
                        }
                    }

                    if needs_workspace_loading {
                        let mouse_follows_focus = self.mouse_follows_focus;
                        if let Some(monitor) = self.focused_monitor_mut() {
                            monitor.load_focused_workspace(mouse_follows_focus, true)?;
                        }
                    }
                }
            }
            SocketMessage::FocusWindow(direction) => {
                let focused_workspace = self.focused_workspace()?;
                match focused_workspace.layer {
                    WorkspaceLayer::Tiling => {
                        self.focus_container_in_direction(direction)?;
                    }
                    WorkspaceLayer::Floating => {
                        self.focus_floating_window_in_direction(direction)?;
                    }
                }
            }
            SocketMessage::PreselectDirection(direction) => {
                let focused_workspace = self.focused_workspace()?;
                let mut update = false;

                if focused_workspace.preselected_container_idx.is_some() {
                    tracing::warn!(
                        "ignoring command as this workspace already has a direction preselect set"
                    );
                } else if matches!(focused_workspace.layer, WorkspaceLayer::Tiling) {
                    self.preselect_container_in_direction(direction)?;
                    update = true;
                }

                if update {
                    self.focused_workspace_mut()?.update()?;
                }
            }
            SocketMessage::CancelPreselect => {
                let focused_workspace = self.focused_workspace_mut()?;
                focused_workspace.cancel_preselect();
                focused_workspace.update()?;
            }
            SocketMessage::MoveWindow(direction) => {
                let focused_workspace = self.focused_workspace()?;
                match focused_workspace.layer {
                    WorkspaceLayer::Tiling => {
                        self.move_container_in_direction(direction)?;
                    }
                    WorkspaceLayer::Floating => {
                        self.move_floating_window_in_direction(direction)?;
                    }
                }
            }
            SocketMessage::CycleFocusWindow(content) => {
                let (direction, cycle_focus_across_monitors_override) = match content {
                    CycleFocusWindowContent::DirectionOnly(d) => (d, None),
                    CycleFocusWindowContent::DirectionWithOverride(d, o) => (d, o),
                };
                let focused_workspace = self.focused_workspace()?;
                match focused_workspace.layer {
                    WorkspaceLayer::Tiling => {
                        // A workspace with no tiled content still hosts the
                        // monitor's pinned floating windows, so when there is
                        // nothing to cycle in the tiling containers fall back
                        // to the floating cycle pool (pins) instead of doing
                        // nothing.
                        let workspace_empty = focused_workspace.is_empty();
                        let has_pins = self
                            .focused_monitor()
                            .map(|monitor| !monitor.pinned_windows().is_empty())
                            .unwrap_or(false);
                        if workspace_empty && has_pins {
                            self.focus_floating_window_in_cycle_direction(direction)?;
                        } else {
                            self.focus_container_in_cycle_direction(
                                direction,
                                cycle_focus_across_monitors_override,
                            )?;
                        }
                    }
                    WorkspaceLayer::Floating => {
                        self.focus_floating_window_in_cycle_direction(direction)?;
                    }
                }
            }
            SocketMessage::CycleMoveWindow(direction) => {
                self.move_container_in_cycle_direction(direction)?;
            }
            SocketMessage::StackWindow(direction) => self.add_window_to_container(direction)?,
            SocketMessage::UnstackWindow => self.remove_window_from_container()?,
            SocketMessage::StackAll => self.stack_all()?,
            SocketMessage::UnstackAll => self.unstack_all(true)?,
            SocketMessage::CycleStack(direction) => {
                self.cycle_container_window_in_direction(direction)?;
            }
            SocketMessage::CycleStackIndex(direction) => {
                self.cycle_container_window_index_in_direction(direction)?;
            }
            SocketMessage::FocusStackWindow(idx) => {
                // In case you are using this command on a bar on a monitor
                // different from the currently focused one, you'd want that
                // monitor to be focused so that the FocusStackWindow happens
                // on the monitor with the bar you just pressed.
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos() {
                    self.focus_monitor(monitor_idx)?;
                }
                self.focus_container_window(idx)?;
            }
            SocketMessage::ForceFocus => {
                let focused_window = self.focused_window()?;
                let focused_window_rect = WindowsApi::window_rect(focused_window.hwnd)?;
                WindowsApi::center_cursor_in_rect(&focused_window_rect)?;
                WindowsApi::left_click();
            }
            SocketMessage::Close => {
                Window::from(WindowsApi::foreground_window()?).close()?;
            }
            SocketMessage::Minimize => {
                Window::from(WindowsApi::foreground_window()?).minimize();
            }
            SocketMessage::ReclaimMinimizedWindows => {
                self.restore_minimized_windows()?;
            }
            SocketMessage::ReclaimLastMinimizedWindow => {
                self.restore_last_minimized_window()?;
            }
            SocketMessage::LockMonitorWorkspaceContainer(
                monitor_idx,
                workspace_idx,
                container_idx,
            ) => {
                let monitor = self
                    .monitors_mut()
                    .get_mut(monitor_idx)
                    .ok_or_eyre("no monitor at the given index")?;

                let workspace = monitor
                    .workspaces_mut()
                    .get_mut(workspace_idx)
                    .ok_or_eyre("no workspace at the given index")?;

                if let Some(container) = workspace.containers_mut().get_mut(container_idx) {
                    container.locked = true;
                }
            }
            SocketMessage::UnlockMonitorWorkspaceContainer(
                monitor_idx,
                workspace_idx,
                container_idx,
            ) => {
                let monitor = self
                    .monitors_mut()
                    .get_mut(monitor_idx)
                    .ok_or_eyre("no monitor at the given index")?;

                let workspace = monitor
                    .workspaces_mut()
                    .get_mut(workspace_idx)
                    .ok_or_eyre("no workspace at the given index")?;

                if let Some(container) = workspace.containers_mut().get_mut(container_idx) {
                    container.locked = false;
                }
            }
            SocketMessage::ToggleLock => self.toggle_lock()?,
            SocketMessage::ToggleFloat => self.toggle_float(false)?,
            SocketMessage::TogglePin => self.toggle_pin_floating_window()?,
            SocketMessage::TogglePinAlwaysOnTop => self.toggle_pin_always_on_top()?,
            SocketMessage::ToggleMonocle => self.toggle_monocle()?,
            SocketMessage::ToggleMaximize => self.toggle_maximize()?,
            SocketMessage::ContainerPadding(monitor_idx, workspace_idx, size) => {
                self.set_container_padding(monitor_idx, workspace_idx, size)?;
            }
            SocketMessage::NamedWorkspaceContainerPadding(ref workspace, size) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.set_container_padding(monitor_idx, workspace_idx, size)?;
                }
            }
            SocketMessage::WorkspacePadding(monitor_idx, workspace_idx, size) => {
                self.set_workspace_padding(monitor_idx, workspace_idx, size)?;
            }
            SocketMessage::NamedWorkspacePadding(ref workspace, size) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.set_workspace_padding(monitor_idx, workspace_idx, size)?;
                }
            }
            SocketMessage::InitialWorkspaceRule(identifier, ref id, monitor_idx, workspace_idx) => {
                let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                let workspace_matching_rule = WorkspaceMatchingRule {
                    monitor_index: monitor_idx,
                    workspace_index: workspace_idx,
                    matching_rule: MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.to_string(),
                        matching_strategy: Some(MatchingStrategy::Legacy),
                    }),
                    initial_only: true,
                };

                if !workspace_rules.contains(&workspace_matching_rule) {
                    workspace_rules.push(workspace_matching_rule);
                }
            }
            SocketMessage::InitialNamedWorkspaceRule(identifier, ref id, ref workspace) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                    let workspace_matching_rule = WorkspaceMatchingRule {
                        monitor_index: monitor_idx,
                        workspace_index: workspace_idx,
                        matching_rule: MatchingRule::Simple(IdWithIdentifier {
                            kind: identifier,
                            id: id.to_string(),
                            matching_strategy: Some(MatchingStrategy::Legacy),
                        }),
                        initial_only: true,
                    };

                    if !workspace_rules.contains(&workspace_matching_rule) {
                        workspace_rules.push(workspace_matching_rule);
                    }
                }
            }
            SocketMessage::WorkspaceRule(identifier, ref id, monitor_idx, workspace_idx) => {
                let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                let workspace_matching_rule = WorkspaceMatchingRule {
                    monitor_index: monitor_idx,
                    workspace_index: workspace_idx,
                    matching_rule: MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.to_string(),
                        matching_strategy: Some(MatchingStrategy::Legacy),
                    }),
                    initial_only: false,
                };

                if !workspace_rules.contains(&workspace_matching_rule) {
                    workspace_rules.push(workspace_matching_rule);
                }
            }
            SocketMessage::NamedWorkspaceRule(identifier, ref id, ref workspace) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                    let workspace_matching_rule = WorkspaceMatchingRule {
                        monitor_index: monitor_idx,
                        workspace_index: workspace_idx,
                        matching_rule: MatchingRule::Simple(IdWithIdentifier {
                            kind: identifier,
                            id: id.to_string(),
                            matching_strategy: Some(MatchingStrategy::Legacy),
                        }),
                        initial_only: false,
                    };

                    if !workspace_rules.contains(&workspace_matching_rule) {
                        workspace_rules.push(workspace_matching_rule);
                    }
                }
            }
            SocketMessage::ClearWorkspaceRules(monitor_idx, workspace_idx) => {
                let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();

                workspace_rules.retain(|r| {
                    r.monitor_index != monitor_idx && r.workspace_index != workspace_idx
                });
            }
            SocketMessage::ClearNamedWorkspaceRules(ref workspace) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                    workspace_rules.retain(|r| {
                        r.monitor_index != monitor_idx && r.workspace_index != workspace_idx
                    });
                }
            }
            SocketMessage::ClearAllWorkspaceRules => {
                let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                workspace_rules.clear();
            }
            SocketMessage::EnforceWorkspaceRules => {
                {
                    let mut already_moved = self.already_moved_window_handles.lock();
                    already_moved.clear();
                }
                self.enforce_workspace_rules()?;
            }
            SocketMessage::EnforceStackRules => self.enforce_stack_rules()?,
            SocketMessage::ManageRule(identifier, ref id) => {
                let mut manage_identifiers = MANAGE_IDENTIFIERS.lock();

                let mut should_push = true;
                for m in &*manage_identifiers {
                    if let MatchingRule::Simple(m) = m
                        && m.id.eq(id)
                    {
                        should_push = false;
                    }
                }

                if should_push {
                    manage_identifiers.push(MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.clone(),
                        matching_strategy: Option::from(MatchingStrategy::Legacy),
                    }));
                }
            }
            SocketMessage::SessionFloatRule => {
                let foreground_window = WindowsApi::foreground_window()?;
                let window = Window::from(foreground_window);
                if let (Ok(exe), Ok(title), Ok(class)) =
                    (window.exe(), window.title(), window.class())
                {
                    let rule = MatchingRule::Composite(vec![
                        IdWithIdentifier {
                            kind: ApplicationIdentifier::Exe,
                            id: exe,
                            matching_strategy: Option::from(MatchingStrategy::Equals),
                        },
                        IdWithIdentifier {
                            kind: ApplicationIdentifier::Title,
                            id: title,
                            matching_strategy: Option::from(MatchingStrategy::Equals),
                        },
                        IdWithIdentifier {
                            kind: ApplicationIdentifier::Class,
                            id: class,
                            matching_strategy: Option::from(MatchingStrategy::Equals),
                        },
                    ]);

                    let mut floating_applications = FLOATING_APPLICATIONS.lock();
                    floating_applications.push(rule.clone());
                    let mut session_floating_applications = SESSION_FLOATING_APPLICATIONS.lock();
                    session_floating_applications.push(rule.clone());

                    self.toggle_float(true)?;
                }
            }
            SocketMessage::SessionFloatRules => {
                let session_floating_applications = SESSION_FLOATING_APPLICATIONS.lock();
                let rules = match serde_json::to_string_pretty(&*session_floating_applications) {
                    Ok(rules) => rules,
                    Err(error) => error.to_string(),
                };

                reply.write_all(rules.as_bytes())?;
            }
            SocketMessage::ClearSessionFloatRules => {
                let mut floating_applications = FLOATING_APPLICATIONS.lock();
                let mut session_floating_applications = SESSION_FLOATING_APPLICATIONS.lock();
                floating_applications.retain(|r| !session_floating_applications.contains(r));
                session_floating_applications.clear()
            }
            SocketMessage::IgnoreRule(identifier, ref id) => {
                let mut ignore_identifiers = IGNORE_IDENTIFIERS.lock();

                let mut should_push = true;
                for i in &*ignore_identifiers {
                    if let MatchingRule::Simple(i) = i
                        && i.id.eq(id)
                    {
                        should_push = false;
                    }
                }

                if should_push {
                    ignore_identifiers.push(MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.clone(),
                        matching_strategy: Option::from(MatchingStrategy::Legacy),
                    }));
                }

                let offset = self.work_area_offset;

                let mut hwnds_to_purge = vec![];
                for (i, monitor) in self.monitors().iter().enumerate() {
                    for container in monitor
                        .focused_workspace()
                        .ok_or_eyre("there is no workspace")?
                        .containers()
                    {
                        for window in container.windows() {
                            match identifier {
                                ApplicationIdentifier::Path => {
                                    if window.path()? == *id {
                                        hwnds_to_purge.push((i, window.hwnd));
                                    }
                                }
                                ApplicationIdentifier::Exe => {
                                    if window.exe()? == *id {
                                        hwnds_to_purge.push((i, window.hwnd));
                                    }
                                }
                                ApplicationIdentifier::Class => {
                                    if window.class()? == *id {
                                        hwnds_to_purge.push((i, window.hwnd));
                                    }
                                }
                                ApplicationIdentifier::Title => {
                                    if window.title()? == *id {
                                        hwnds_to_purge.push((i, window.hwnd));
                                    }
                                }
                            }
                        }
                    }
                }

                for (monitor_idx, hwnd) in hwnds_to_purge {
                    let monitor = self
                        .monitors_mut()
                        .get_mut(monitor_idx)
                        .ok_or_eyre("there is no monitor")?;

                    monitor
                        .focused_workspace_mut()
                        .ok_or_eyre("there is no focused workspace")?
                        .remove_window(hwnd)?;

                    monitor.update_focused_workspace(offset)?;
                }
            }
            SocketMessage::FocusedWorkspaceContainerPadding(adjustment) => {
                let focused_monitor_idx = self.focused_monitor_idx();

                let focused_monitor = self.focused_monitor().ok_or_eyre("there is no monitor")?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();

                self.set_container_padding(focused_monitor_idx, focused_workspace_idx, adjustment)?;
            }
            SocketMessage::FocusedWorkspacePadding(adjustment) => {
                let focused_monitor_idx = self.focused_monitor_idx();

                let focused_monitor = self.focused_monitor().ok_or_eyre("there is no monitor")?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();

                self.set_workspace_padding(focused_monitor_idx, focused_workspace_idx, adjustment)?;
            }
            SocketMessage::AdjustContainerPadding(sizing, adjustment) => {
                self.adjust_container_padding(sizing, adjustment)?;
            }
            SocketMessage::AdjustWorkspacePadding(sizing, adjustment) => {
                self.adjust_workspace_padding(sizing, adjustment)?;
            }
            SocketMessage::MoveContainerToLastWorkspace => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                let idx = self
                    .focused_monitor()
                    .ok_or_eyre("there is no monitor")?
                    .focused_workspace_idx();

                if let Some(monitor) = self.focused_monitor_mut()
                    && let Some(last_focused_workspace) = monitor.last_focused_workspace
                {
                    self.move_container_to_workspace(last_focused_workspace, true, None)?;
                }

                self.focused_monitor_mut()
                    .ok_or_eyre("there is no monitor")?
                    .last_focused_workspace = Option::from(idx);
            }
            SocketMessage::SendContainerToLastWorkspace => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                let idx = self
                    .focused_monitor()
                    .ok_or_eyre("there is no monitor")?
                    .focused_workspace_idx();

                if let Some(monitor) = self.focused_monitor_mut()
                    && let Some(last_focused_workspace) = monitor.last_focused_workspace
                {
                    self.move_container_to_workspace(last_focused_workspace, false, None)?;
                }
                self.focused_monitor_mut()
                    .ok_or_eyre("there is no monitor")?
                    .last_focused_workspace = Option::from(idx);
            }
            SocketMessage::MoveContainerToWorkspaceNumber(workspace_idx) => {
                self.move_container_to_workspace(workspace_idx, true, None)?;
            }
            SocketMessage::CycleMoveContainerToWorkspace(direction) => {
                let focused_monitor = self.focused_monitor().ok_or_eyre("there is no monitor")?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();
                let workspaces = focused_monitor.workspaces().len();

                let workspace_idx = direction.next_idx(
                    focused_workspace_idx,
                    NonZeroUsize::new(workspaces)
                        .ok_or_eyre("there must be at least one workspace")?,
                );

                self.move_container_to_workspace(workspace_idx, true, None)?;
            }
            SocketMessage::MoveContainerToMonitorNumber(monitor_idx) => {
                let direction = self.direction_from_monitor_idx(monitor_idx);
                self.move_container_to_monitor(monitor_idx, None, true, direction)?;
            }
            SocketMessage::SwapWorkspacesToMonitorNumber(monitor_idx) => {
                self.swap_focused_monitor(monitor_idx)?;
            }
            SocketMessage::CycleMoveContainerToMonitor(direction) => {
                let monitor_idx = direction.next_idx(
                    self.focused_monitor_idx(),
                    NonZeroUsize::new(self.monitors().len())
                        .ok_or_eyre("there must be at least one monitor")?,
                );

                let direction = self.direction_from_monitor_idx(monitor_idx);
                self.move_container_to_monitor(monitor_idx, None, true, direction)?;
            }
            SocketMessage::SendContainerToWorkspaceNumber(workspace_idx) => {
                self.move_container_to_workspace(workspace_idx, false, None)?;
            }
            SocketMessage::CycleSendContainerToWorkspace(direction) => {
                let focused_monitor = self.focused_monitor().ok_or_eyre("there is no monitor")?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();
                let workspaces = focused_monitor.workspaces().len();

                let workspace_idx = direction.next_idx(
                    focused_workspace_idx,
                    NonZeroUsize::new(workspaces)
                        .ok_or_eyre("there must be at least one workspace")?,
                );

                self.move_container_to_workspace(workspace_idx, false, None)?;
            }
            SocketMessage::SendContainerToMonitorNumber(monitor_idx) => {
                let direction = self.direction_from_monitor_idx(monitor_idx);
                self.move_container_to_monitor(monitor_idx, None, false, direction)?;
            }
            SocketMessage::CycleSendContainerToMonitor(direction) => {
                let monitor_idx = direction.next_idx(
                    self.focused_monitor_idx(),
                    NonZeroUsize::new(self.monitors().len())
                        .ok_or_eyre("there must be at least one monitor")?,
                );

                let direction = self.direction_from_monitor_idx(monitor_idx);
                self.move_container_to_monitor(monitor_idx, None, false, direction)?;
            }
            SocketMessage::SendContainerToMonitorWorkspaceNumber(monitor_idx, workspace_idx) => {
                let direction = self.direction_from_monitor_idx(monitor_idx);
                self.move_container_to_monitor(
                    monitor_idx,
                    Option::from(workspace_idx),
                    false,
                    direction,
                )?;
            }
            SocketMessage::MoveContainerToMonitorWorkspaceNumber(monitor_idx, workspace_idx) => {
                let direction = self.direction_from_monitor_idx(monitor_idx);
                self.move_container_to_monitor(
                    monitor_idx,
                    Option::from(workspace_idx),
                    true,
                    direction,
                )?;
            }
            SocketMessage::SendContainerToNamedWorkspace(ref workspace) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    let direction = self.direction_from_monitor_idx(monitor_idx);
                    self.move_container_to_monitor(
                        monitor_idx,
                        Option::from(workspace_idx),
                        false,
                        direction,
                    )?;
                }
            }
            SocketMessage::MoveContainerToNamedWorkspace(ref workspace) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    let direction = self.direction_from_monitor_idx(monitor_idx);
                    self.move_container_to_monitor(
                        monitor_idx,
                        Option::from(workspace_idx),
                        true,
                        direction,
                    )?;
                }
            }

            SocketMessage::MoveWorkspaceToMonitorNumber(monitor_idx) => {
                self.move_workspace_to_monitor(monitor_idx)?;
            }
            SocketMessage::CycleMoveWorkspaceToMonitor(direction) => {
                let monitor_idx = direction.next_idx(
                    self.focused_monitor_idx(),
                    NonZeroUsize::new(self.monitors().len())
                        .ok_or_eyre("there must be at least one monitor")?,
                );

                self.move_workspace_to_monitor(monitor_idx)?;
            }
            SocketMessage::TogglePause => {
                if self.is_paused {
                    tracing::info!("resuming");
                } else {
                    tracing::info!("pausing");
                }

                self.is_paused = !self.is_paused;
                self.retile_all(true)?;
            }
            SocketMessage::ToggleTiling => {
                self.toggle_tiling()?;
            }
            SocketMessage::CycleFocusMonitor(direction) => {
                let monitor_idx = direction.next_idx(
                    self.focused_monitor_idx(),
                    NonZeroUsize::new(self.monitors().len())
                        .ok_or_eyre("there must be at least one monitor")?,
                );

                self.focus_monitor(monitor_idx)?;
                self.update_focused_workspace(self.mouse_follows_focus, true)?;
            }
            SocketMessage::FocusMonitorNumber(monitor_idx) => {
                self.focus_monitor(monitor_idx)?;
                self.update_focused_workspace(self.mouse_follows_focus, true)?;
            }
            SocketMessage::FocusMonitorAtCursor => {
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos() {
                    self.focus_monitor(monitor_idx)?;
                }
            }
            SocketMessage::Retile => {
                border_manager::destroy_all_borders()?;
                force_update_borders = true;
                self.retile_all(false)?
            }
            SocketMessage::RetileWithResizeDimensions => {
                border_manager::destroy_all_borders()?;
                force_update_borders = true;
                self.retile_all(true)?
            }
            SocketMessage::FlipLayout(layout_flip) => self.flip_layout(layout_flip)?,
            SocketMessage::ScrollingLayoutColumns(count) => {
                let focused_workspace = self.focused_workspace_mut()?;

                let options = match focused_workspace.layout_options {
                    Some(mut opts) => {
                        if let Some(scrolling) = &mut opts.scrolling {
                            scrolling.columns = count.into();
                        }

                        opts
                    }
                    None => LayoutOptions {
                        scrolling: Some(ScrollingLayoutOptions {
                            columns: count.into(),
                            center_focused_column: Default::default(),
                        }),
                        grid: None,
                        column_ratios: None,
                        row_ratios: None,
                    },
                };

                focused_workspace.layout_options = Some(options);
                self.update_focused_workspace(false, false)?;
            }
            SocketMessage::ChangeLayout(layout) => self.change_workspace_layout_default(layout)?,
            SocketMessage::CycleLayout(direction) => self.cycle_layout(direction)?,
            SocketMessage::LayoutRatios(ref columns, ref rows) => {
                use crate::core::validate_ratios;

                let focused_workspace = self.focused_workspace_mut()?;

                let mut options = focused_workspace.layout_options.unwrap_or(LayoutOptions {
                    scrolling: None,
                    grid: None,
                    column_ratios: None,
                    row_ratios: None,
                });

                if let Some(cols) = columns {
                    options.column_ratios = Some(validate_ratios(cols));
                }

                if let Some(rws) = rows {
                    options.row_ratios = Some(validate_ratios(rws));
                }

                focused_workspace.layout_options = Some(options);
                self.update_focused_workspace(false, false)?;
            }
            SocketMessage::ChangeLayoutCustom(ref path) => {
                self.change_workspace_custom_layout(path)?;
            }
            SocketMessage::WorkspaceLayoutCustom(monitor_idx, workspace_idx, ref path) => {
                self.set_workspace_layout_custom(monitor_idx, workspace_idx, path)?;
            }
            SocketMessage::WorkspaceTiling(monitor_idx, workspace_idx, tile) => {
                self.set_workspace_tiling(monitor_idx, workspace_idx, tile)?;
            }
            SocketMessage::WorkspaceLayout(monitor_idx, workspace_idx, layout) => {
                self.set_workspace_layout_default(monitor_idx, workspace_idx, layout)?;
            }
            SocketMessage::WorkspaceLayoutRule(
                monitor_idx,
                workspace_idx,
                at_container_count,
                layout,
            ) => {
                self.add_workspace_layout_default_rule(
                    monitor_idx,
                    workspace_idx,
                    at_container_count,
                    layout,
                )?;
            }
            SocketMessage::WorkspaceLayoutCustomRule(
                monitor_idx,
                workspace_idx,
                at_container_count,
                ref path,
            ) => {
                self.add_workspace_layout_custom_rule(
                    monitor_idx,
                    workspace_idx,
                    at_container_count,
                    path,
                )?;
            }
            SocketMessage::ClearWorkspaceLayoutRules(monitor_idx, workspace_idx) => {
                self.clear_workspace_layout_rules(monitor_idx, workspace_idx)?;
            }
            SocketMessage::NamedWorkspaceLayoutCustom(ref workspace, ref path) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.set_workspace_layout_custom(monitor_idx, workspace_idx, path)?;
                }
            }
            SocketMessage::NamedWorkspaceTiling(ref workspace, tile) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.set_workspace_tiling(monitor_idx, workspace_idx, tile)?;
                }
            }
            SocketMessage::NamedWorkspaceLayout(ref workspace, layout) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.set_workspace_layout_default(monitor_idx, workspace_idx, layout)?;
                }
            }
            SocketMessage::NamedWorkspaceLayoutRule(ref workspace, at_container_count, layout) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.add_workspace_layout_default_rule(
                        monitor_idx,
                        workspace_idx,
                        at_container_count,
                        layout,
                    )?;
                }
            }
            SocketMessage::NamedWorkspaceLayoutCustomRule(
                ref workspace,
                at_container_count,
                ref path,
            ) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.add_workspace_layout_custom_rule(
                        monitor_idx,
                        workspace_idx,
                        at_container_count,
                        path,
                    )?;
                }
            }
            SocketMessage::ClearNamedWorkspaceLayoutRules(ref workspace) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(workspace)
                {
                    self.clear_workspace_layout_rules(monitor_idx, workspace_idx)?;
                }
            }
            SocketMessage::CycleFocusWorkspace(direction) => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                let focused_monitor = self.focused_monitor().ok_or_eyre("there is no monitor")?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();
                let workspaces = focused_monitor.workspaces().len();

                let workspace_idx = direction.next_idx(
                    focused_workspace_idx,
                    NonZeroUsize::new(workspaces)
                        .ok_or_eyre("there must be at least one workspace")?,
                );

                self.focus_workspace(workspace_idx)?;
            }
            SocketMessage::CycleFocusEmptyWorkspace(direction) => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                let focused_monitor = self.focused_monitor().ok_or_eyre("there is no monitor")?;

                let focused_workspace_idx = focused_monitor.focused_workspace_idx();
                let workspaces = focused_monitor.workspaces().len();

                let mut empty_workspaces = vec![];

                for (idx, w) in focused_monitor.workspaces().iter().enumerate() {
                    if w.is_empty() {
                        empty_workspaces.push(idx);
                    }
                }

                if !empty_workspaces.is_empty() {
                    let mut workspace_idx = direction.next_idx(
                        focused_workspace_idx,
                        NonZeroUsize::new(workspaces)
                            .ok_or_eyre("there must be at least one workspace")?,
                    );

                    while !empty_workspaces.contains(&workspace_idx) {
                        workspace_idx = direction.next_idx(
                            workspace_idx,
                            NonZeroUsize::new(workspaces)
                                .ok_or_eyre("there must be at least one workspace")?,
                        );
                    }

                    self.focus_workspace(workspace_idx)?;
                }
            }
            SocketMessage::CloseWorkspace => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                let mut can_close = false;

                if let Some(monitor) = self.focused_monitor_mut() {
                    let focused_workspace_idx = monitor.focused_workspace_idx();
                    let next_focused_workspace_idx = focused_workspace_idx.saturating_sub(1);

                    if let Some(workspace) = monitor.focused_workspace()
                        && monitor.workspaces().len() > 1
                        && workspace.containers().is_empty()
                        && workspace.floating_windows().is_empty()
                        && workspace.monocle_container.is_none()
                        && workspace.maximized_window.is_none()
                        && workspace.name.is_none()
                    {
                        can_close = true;
                    }

                    if can_close
                        && monitor
                            .workspaces_mut()
                            .remove(focused_workspace_idx)
                            .is_some()
                    {
                        self.focus_workspace(next_focused_workspace_idx)?;
                    }
                }
            }
            SocketMessage::FocusLastWorkspace => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                let idx = self
                    .focused_monitor()
                    .ok_or_eyre("there is no monitor")?
                    .focused_workspace_idx();

                if let Some(monitor) = self.focused_monitor_mut()
                    && let Some(last_focused_workspace) = monitor.last_focused_workspace
                {
                    self.focus_workspace(last_focused_workspace)?;
                }

                self.focused_monitor_mut()
                    .ok_or_eyre("there is no monitor")?
                    .last_focused_workspace = Option::from(idx);
            }
            SocketMessage::FocusWorkspaceNumber(workspace_idx) => {
                // This is to ensure that even on an empty workspace on a secondary monitor, the
                // secondary monitor where the cursor is focused will be used as the target for
                // the workspace switch op
                if let Some(monitor_idx) = self.monitor_idx_from_current_pos()
                    && monitor_idx != self.focused_monitor_idx()
                    && let Some(monitor) = self.monitors().get(monitor_idx)
                    && let Some(workspace) = monitor.focused_workspace()
                    && workspace.is_empty()
                {
                    self.focus_monitor(monitor_idx)?;
                }

                if self.focused_workspace_idx().unwrap_or_default() != workspace_idx {
                    self.focus_workspace(workspace_idx)?;
                }
            }
            SocketMessage::FocusWorkspaceNumbers(workspace_idx) => {
                let focused_monitor_idx = self.focused_monitor_idx();

                tracing::debug!(
                    "focus_workspace_numbers: start, target_workspace={}, focused_monitor={}",
                    workspace_idx,
                    focused_monitor_idx
                );

                // Debug: log last_focused_hwnd for the target workspace before any changes
                if let Some(monitor) = self.focused_monitor() {
                    if let Some(workspace) = monitor.workspaces().get(workspace_idx) {
                        tracing::debug!(
                            "focus_workspace_numbers: pre-switch last_focused_hwnd={:?}, \
                             focused_container_idx={}, containers={}",
                            workspace.last_focused_hwnd,
                            workspace.focused_container_idx(),
                            workspace.containers().len(),
                        );
                        if let Some(hwnd) = workspace.last_focused_hwnd {
                            tracing::debug!(
                                "focus_workspace_numbers: container for last_focused_hwnd: {:?}",
                                workspace.container_idx_for_window(hwnd),
                            );
                        }
                    }
                }

                // Switch workspaces on all other monitors silently first (no focus/cursor changes)
                for (i, monitor) in self.monitors_mut().iter_mut().enumerate() {
                    if i != focused_monitor_idx {
                        monitor.focus_workspace(workspace_idx)?;
                        monitor.load_focused_workspace(false, false)?;
                    }
                }

                // Debug: log focus state before self.focus_workspace
                if let Some(workspace) = self
                    .focused_monitor()
                    .and_then(|m| m.workspaces().get(workspace_idx))
                {
                    tracing::debug!(
                        "focus_workspace_numbers: before focus_workspace, last_focused_hwnd={:?}, \
                         focused_container_idx={}",
                        workspace.last_focused_hwnd,
                        workspace.focused_container_idx(),
                    );
                }

                // Finally, focus the workspace on the original monitor with cursor follow.
                self.focus_workspace(workspace_idx)?;

                // Debug: log focus state after self.focus_workspace
                if let Some(workspace) = self
                    .focused_monitor()
                    .and_then(|m| m.workspaces().get(workspace_idx))
                {
                    tracing::debug!(
                        "focus_workspace_numbers: after focus_workspace, last_focused_hwnd={:?}, \
                         focused_container_idx={}, containers={}",
                        workspace.last_focused_hwnd,
                        workspace.focused_container_idx(),
                        workspace.containers().len(),
                    );
                    if let Some(hwnd) = workspace.last_focused_hwnd {
                        tracing::debug!(
                            "focus_workspace_numbers: container for last_focused_hwnd: {:?}",
                            workspace.container_idx_for_window(hwnd),
                        );
                    }

                    // If last_focused_hwnd was lost, try to restore it by finding
                    // the last container that still has a window
                    if workspace.last_focused_hwnd.is_none()
                        || workspace
                            .last_focused_hwnd
                            .and_then(|hwnd| workspace.container_idx_for_window(hwnd))
                            .is_none()
                    {
                        tracing::debug!(
                            "focus_workspace_numbers: last_focused_hwnd lost or invalid, \
                             falling back to last container with windows",
                        );
                        // Focus the last container that has windows
                        for i in (0..workspace.containers().len()).rev() {
                            if let Some(container) = workspace.containers().get(i) {
                                if !container.windows().is_empty() {
                                    tracing::debug!(
                                        "focus_workspace_numbers: restoring focus to container {}",
                                        i,
                                    );
                                    // We can't call focus_container here due to borrow rules,
                                    // but the next user interaction will fix the focus.
                                    // Log it so we can see what happened.
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            SocketMessage::FocusMonitorWorkspaceNumber(monitor_idx, workspace_idx) => {
                let focused_monitor_idx = self.focused_monitor_idx();
                let focused_workspace_idx = self.focused_workspace_idx().unwrap_or_default();

                let focused_pair = (focused_monitor_idx, focused_workspace_idx);

                if focused_pair != (monitor_idx, workspace_idx) {
                    self.focus_monitor(monitor_idx)?;
                    self.focus_workspace(workspace_idx)?;
                }
            }
            SocketMessage::FocusNamedWorkspace(ref name) => {
                if let Some((monitor_idx, workspace_idx)) =
                    self.monitor_workspace_index_by_name(name)
                {
                    self.focus_monitor(monitor_idx)?;
                    self.focus_workspace(workspace_idx)?;
                }
            }
            SocketMessage::ToggleWorkspaceLayer => {
                let mouse_follows_focus = self.mouse_follows_focus;

                // The focus calls made to enter the new layer fire FocusChange
                // events that must not immediately flip it back.
                self.suppress_layer_flips();

                let (workspace_layer, must_lower_ignored, empty_and_hide_pins) = {
                    let workspace = self.focused_workspace()?;

                    // When toggling the workspace layer, demote any ignored windows
                    // (e.g. desktop widgets) so they never sit above the tiled base
                    // layer. Skipped when the ignored windows have been manually
                    // raised above managed or there are no managed windows to cover.
                    let must_lower_ignored =
                        !workspace.ignored_windows_above_managed && !workspace.is_empty();

                    // Track whether the pinned hide-on-empty feature is active and
                    // this workspace is empty: the monitor's pins are hidden by
                    // `apply_pin_visibility`, and the toggle below must not
                    // resurrect them.
                    let empty_and_hide_pins = HIDE_PINNED_ON_EMPTY_WORKSPACES
                        .load(Ordering::SeqCst)
                        && workspace.is_empty();

                    (workspace.layer, must_lower_ignored, empty_and_hide_pins)
                };

                match workspace_layer {
                    WorkspaceLayer::Tiling => {
                        // Pinned floating windows of every workspace on this monitor
                        // belong to the focused workspace's Floating overlay: they are
                        // treated as part of this workspace's floating set on whichever
                        // workspace is focused, not just their home workspace. Capture
                        // them before borrowing the focused workspace.
                        let mut pinned_overlay = self
                            .focused_monitor()
                            .ok_or_eyre("there is no monitor")?
                            .pinned_windows();

                        // Hidden pins (hide-on-empty on an empty workspace) must not
                        // be restored or focused by the toggle: restoring them re-shows
                        // their borders (racing the trailing hide) and focusing one
                        // steals focus to the pin on an otherwise empty layer. Drop them
                        // from the overlay entirely so neither the focus-memory lookup
                        // nor the restore loop below can touch them.
                        if empty_and_hide_pins {
                            pinned_overlay.retain(|window| window.is_shown());
                        }

                        // The Floating layer remembers its own last-focused window;
                        // a pinned window is just a float, so it can be that memory
                        // too. Snapshot it before the mutable workspace borrow.
                        let last_focused_floating_hwnd =
                            self.focused_workspace()?.last_focused_floating_hwnd;

                        let workspace = self.focused_workspace_mut()?;
                        workspace.layer = WorkspaceLayer::Floating;
                        workspace.layer_lock = true;

                        // Show the floating overlay on top of the base layer. The base (tiling)
                        // layer keeps its positions, but the focused window of every tiling
                        // container is synchronously lowered to the bottom of the z-order
                        // after the floats are raised, so the overlay is guaranteed to sit
                        // above the base even if a float's raise fails.
                        let focused_idx = workspace.focused_floating_window_idx();
                        let mut window_idx_pairs = workspace
                            .floating_windows_mut()
                            .make_contiguous()
                            .iter()
                            .enumerate()
                            .collect::<Vec<_>>();

                        // Sort by window area
                        window_idx_pairs.sort_by_key(|(_, w)| {
                            let rect = WindowsApi::window_rect(w.hwnd).unwrap_or_default();
                            rect.right * rect.bottom
                        });
                        window_idx_pairs.reverse();

                        // Restore the layer's remembered window focus on toggle: prefer the
                        // last-focused floating window recorded by the layer (an
                        // own float or a pinned window - both are floats), and
                        // fall back to the floating ring's focused element when
                        // that handle is no longer around.
                        let mut to_focus = None;
                        let mut focus_source = "none";
                        for (_, window) in window_idx_pairs.iter() {
                            if Some(window.hwnd) == last_focused_floating_hwnd {
                                to_focus = Some(**window);
                                focus_source = "own-float-memory";
                                break;
                            }
                        }
                        if to_focus.is_none()
                            && let Some(hwnd) = last_focused_floating_hwnd
                            && let Some(window) = pinned_overlay
                                .iter()
                                .find(|window| window.hwnd == hwnd)
                        {
                            to_focus = Some(*window);
                            focus_source = "pinned-memory";
                        }
                        if to_focus.is_none() {
                            for (i, window) in window_idx_pairs.iter() {
                                if *i == focused_idx {
                                    to_focus = Some(**window);
                                    focus_source = "ring-fallback";
                                    break;
                                }
                            }
                        }

                        tracing::info!(
                            "toggle_to_floating: to_focus_hwnd={:?} source={} last_focused_floating_hwnd={:?}",
                            to_focus.map(|w| w.hwnd),
                            focus_source,
                            last_focused_floating_hwnd,
                        );

                        let mut floats_to_raise = Vec::with_capacity(window_idx_pairs.len());
                        for (_, window) in window_idx_pairs.iter() {
                            let window = **window;
                            if to_focus.is_some_and(|w| w.hwnd == window.hwnd) {
                                continue;
                            }
                            window.restore();
                            floats_to_raise.push(window);
                        }

                        if let Some(focused_window) = &to_focus {
                            // The focused window should be the last one raised to make sure it is
                            // on top
                            focused_window.restore();
                            ApplyWorker::raise_above_active(vec![*focused_window]);
                        }

                        // Raise the rest of the workspace's floating windows on the apply
                        // worker so a Not Responding window can never block the
                        // window-manager thread while toggling the layer.
                        ApplyWorker::raise_above_active(floats_to_raise);

                        // Show the monitor's pinned windows from other workspaces alongside
                        // this workspace's own floating windows. Their deterministic z-order
                        // is fixed by the layer re-stack (`enforce_layer_stack`) at the end
                        // of this arm.
                        let own_hwnds = workspace
                            .floating_windows()
                            .iter()
                            .map(|window| window.hwnd)
                            .collect::<Vec<_>>();
                        pinned_overlay.retain(|window| !own_hwnds.contains(&window.hwnd));
                        for window in pinned_overlay {
                            window.restore();
                        }

                        // Hoist the monocle window so the workspace borrow ends here,
                        // before the monitor borrows taken by the focus step and the
                        // final layer re-stack (`enforce_layer_stack`).
                        let monocle_window = workspace
                            .monocle_container
                            .as_ref()
                            .and_then(|monocle| monocle.focused_window())
                            .copied();

                        // If there are no floating windows to restore, focus the desktop
                        // instead so that lowering the monocle window does not trigger an
                        // auto-focus.
                        if let Some(window) = to_focus {
                            window.focus(mouse_follows_focus)?;
                        } else {
                            WindowsApi::raise_and_focus_window(WindowsApi::desktop_window()?)?;
                        }

                        // Only the monocle window (fullscreen) needs to be lowered so the
                        // floating overlay can sit above it; every other base window keeps its
                        // position. Posted to the apply worker ahead of the layer re-stack
                        // below so the FIFO order lowers it before the overlay raises.
                        if let Some(window) = monocle_window {
                            tracing::info!(
                                hwnd = window.hwnd,
                                "Tiling->Floating: lowering monocle window",
                            );
                            ApplyWorker::lower(vec![window]);
                        }

                        // Deterministically re-assert the whole layer stack using the
                        // same synchronous re-stacker that workspace/monitor switches
                        // rely on: base (tiled) windows are raised, then the floating
                        // windows above them, then the focused top-layer window on top
                        // of the pinned band, with ignored windows demoted last. This
                        // is independent of the transient TopMost-band raise used for
                        // the instantaneous effect above, so the overlay is guaranteed
                        // to sit above the tiling base as long as this call succeeds.
                        self.focused_monitor()
                            .ok_or_eyre("there is no monitor")?
                            .enforce_layer_stack()?;

                        // Deterministically re-assert foreground on the remembered
                        // float AFTER the whole layer stack has settled. Windows can
                        // otherwise leave the foreground on a pinned band window that
                        // ended up at the top of the managed stack (e.g. because the
                        // application re-asserts its own TopMost state), which both
                        // steals the keyboard focus from the last-used float AND
                        // records the pin as the last-used float for the next toggle.
                        if let Ok(foreground) = WindowsApi::foreground_window() {
                            tracing::info!(
                                foreground,
                                "Tiling->Floating: foreground before last-used-float re-assert"
                            );
                        }
                        if let Some(window) = to_focus {
                            match WindowsApi::raise_and_focus_window(window.hwnd) {
                                Ok(()) => tracing::info!(
                                    hwnd = window.hwnd,
                                    "Tiling->Floating: re-asserted foreground on last used float"
                                ),
                                Err(error) => tracing::warn!(
                                    hwnd = window.hwnd,
                                    "could not re-assert foreground on last used float: {error}"
                                ),
                            }
                            if let Ok(foreground) = WindowsApi::foreground_window() {
                                tracing::info!(
                                    foreground,
                                    "Tiling->Floating: foreground after re-assert"
                                );
                            }
                        }

                        // DIAGNOSTIC: dump the top-to-bottom top-level z-order (own
                        // floating, tiling base, pinned, and anything else that ended
                        // up in the way) so the layer result is observable in the
                        // RUST_LOG output. Remove once the toggle reliably raises
                        // every floating window.
                        {
                            let workspace = self.focused_workspace()?;
                            let float_hwnds = workspace
                                .floating_windows()
                                .iter()
                                .map(|window| window.hwnd)
                                .collect::<HashSet<_>>();
                            let base_hwnds = workspace
                                .containers()
                                .iter()
                                .filter_map(|container| container.focused_window())
                                .map(|window| window.hwnd)
                                .collect::<HashSet<_>>();
                            let pinned_hwnds = self
                                .focused_monitor()
                                .map(|monitor| monitor.pinned_windows())
                                .unwrap_or_default()
                                .iter()
                                .map(|window| window.hwnd)
                                .collect::<HashSet<_>>();
                            let mut zorder_hwnds = Vec::new();
                            WindowsApi::enum_windows(
                                Some(crate::windows_callbacks::enum_all_visible_window),
                                &mut zorder_hwnds as *mut Vec<isize> as isize,
                            )?;
                            for hwnd in zorder_hwnds {
                                let role = if float_hwnds.contains(&hwnd) {
                                    "float"
                                } else if base_hwnds.contains(&hwnd) {
                                    "base"
                                } else if pinned_hwnds.contains(&hwnd) {
                                    "pin"
                                } else {
                                    "other"
                                };
                                let is_topmost =
                                    WindowsApi::is_topmost_window(hwnd).unwrap_or(false);
                                tracing::info!(
                                    hwnd,
                                    role,
                                    is_topmost,
                                    title = Window::from(hwnd).title().unwrap_or_default(),
                                    exe = Window::from(hwnd).exe().unwrap_or_default(),
                                    "Tiling->Floating: z-order (top to bottom)",
                                );
                            }
                        }
                    }
                    WorkspaceLayer::Floating => {
                        {
                            let workspace = self.focused_workspace_mut()?;
                            workspace.layer = WorkspaceLayer::Tiling;
                            workspace.layer_lock = false;

                            // The base layer was never moved during the toggle, so it needs no
                            // restoration. Focus the tiling window before lowering the floating
                            // overlay so that managed windows no longer have keyboard focus while
                            // the lowers are performed. This prevents Windows from auto-focusing
                            // and reverting the layer.
                            if let Some(monocle) = &workspace.monocle_container {
                                if let Some(window) = monocle.focused_window() {
                                    window.raise()?;
                                    window.focus(mouse_follows_focus)?;
                                }
                            } else if let Some(window) = workspace
                                .focused_container()
                                .and_then(|container| container.focused_window())
                            {
                                window.focus(mouse_follows_focus)?;
                            }
                        }

                        // Fully switch back to the base layer: lower the entire floating
                        // overlay (this workspace's floating windows along with the pinned
                        // floating windows of other workspaces on this monitor) below the
                        // intact tiling base without raising any tiled window.
                        self.lower_floating_overlay()?;

                        let workspace = self.focused_workspace()?;
                        tracing::info!(
                            container_windows = workspace
                                .containers()
                                .iter()
                                .flat_map(|c| c.windows())
                                .count(),
                            floating_count = workspace.floating_windows().len(),
                            "Floating->Tiling: post-toggle workspace state",
                        );
                    }
                };

                if must_lower_ignored {
                    tracing::info!("lowering ignored windows below managed windows");
                    self.lower_ignored_windows()?;
                }
            }
            SocketMessage::ToggleIgnoredWindowLayer => {
                // Reuse the same predicate as automatic demotion so that always-on-top
                // widget windows (e.g. a status bar) are never moved by the manual
                // toggle: they are already pinned to the topmost band above everything
                // else, so raising or lowering them is a pointless no-op that flickers
                // the bar.
                let ignored_windows = self
                    .ignored_windows()
                    .into_iter()
                    .filter(crate::monitor::Monitor::should_auto_demote)
                    .collect::<Vec<_>>();

                let workspace = self.focused_workspace_mut()?;

                workspace.ignored_windows_above_managed = !workspace.ignored_windows_above_managed;

                if workspace.ignored_windows_above_managed {
                    tracing::info!(
                        ignored_windows = ignored_windows.len(),
                        "raising ignored windows above managed windows"
                    );
                    // EnumWindows enumerates in top-to-bottom z-order; raising bottom-to-top
                    // preserves each ignored window's relative stacking with the current
                    // topmost window ending up on top. The originally-topmost ignored
                    // window is therefore the final one raised, i.e. `ignored_windows.first()`.
                    for window in ignored_windows.iter().rev() {
                        window.restore();
                        window.raise()?;
                    }

                    // Raising with HWND_TOP alone is not enough when a managed window still
                    // holds the foreground: Windows keeps the foreground window drawn above
                    // everything else. Activate the now-topmost raised window (e.g. a
                    // fullscreen game such as DFO) so that it actually comes to the front,
                    // which is the point of the toggle.
                    if let Some(window) = ignored_windows.first() {
                        WindowsApi::raise_and_focus_window(window.hwnd)?;
                    }
                } else {
                    tracing::info!(
                        ignored_windows = ignored_windows.len(),
                        "lowering ignored windows below managed windows"
                    );
                    // Lowering top-to-bottom moves each window to the very bottom, ending
                    // with all ignored windows below the base layer in their original
                    // relative order.
                    for window in ignored_windows {
                        window.lower()?;
                    }
                }
            }
            SocketMessage::Stop => {
                self.stop(false)?;
            }
            SocketMessage::StopIgnoreRestore => {
                self.stop(true)?;
            }
            SocketMessage::MonitorIndexPreference(index_preference, left, top, right, bottom) => {
                let mut monitor_index_preferences = MONITOR_INDEX_PREFERENCES.lock();
                monitor_index_preferences.insert(
                    index_preference,
                    Rect {
                        left,
                        top,
                        right,
                        bottom,
                    },
                );
            }
            SocketMessage::DisplayIndexPreference(index_preference, ref display) => {
                let mut display_index_preferences = DISPLAY_INDEX_PREFERENCES.write();
                display_index_preferences.insert(index_preference, display.clone());
            }
            SocketMessage::EnsureWorkspaces(monitor_idx, workspace_count) => {
                self.ensure_workspaces_for_monitor(monitor_idx, workspace_count)?;
            }
            SocketMessage::EnsureNamedWorkspaces(monitor_idx, ref names) => {
                self.ensure_named_workspaces_for_monitor(monitor_idx, names)?;
            }
            SocketMessage::NewWorkspace => {
                self.new_workspace()?;
            }
            SocketMessage::WorkspaceName(monitor_idx, workspace_idx, ref name) => {
                self.set_workspace_name(monitor_idx, workspace_idx, name.to_string())?;
            }
            SocketMessage::State => {
                let state = match serde_json::to_string_pretty(&state::State::from(&*self)) {
                    Ok(state) => state,
                    Err(error) => error.to_string(),
                };

                tracing::info!("replying to state");

                reply.write_all(state.as_bytes())?;

                tracing::info!("replying to state done");
            }
            SocketMessage::GlobalState => {
                let state = match serde_json::to_string_pretty(&GlobalState::default()) {
                    Ok(state) => state,
                    Err(error) => error.to_string(),
                };

                tracing::info!("replying to global state");

                reply.write_all(state.as_bytes())?;

                tracing::info!("replying to global state done");
            }
            SocketMessage::VisibleWindows => {
                let mut monitor_visible_windows = HashMap::new();

                for monitor in self.monitors() {
                    if let Some(ws) = monitor.focused_workspace() {
                        monitor_visible_windows.insert(
                            monitor.device_id.clone(),
                            ws.visible_window_details().clone(),
                        );
                    }
                }

                let visible_windows_state = serde_json::to_string_pretty(&monitor_visible_windows)
                    .unwrap_or_else(|error| error.to_string());

                reply.write_all(visible_windows_state.as_bytes())?;
            }
            SocketMessage::MonitorInformation => {
                let mut monitors = vec![];
                for monitor in self.monitors() {
                    monitors.push(MonitorInformation::from(monitor));
                }

                let monitors_state = serde_json::to_string_pretty(&monitors)
                    .unwrap_or_else(|error| error.to_string());

                reply.write_all(monitors_state.as_bytes())?;
            }
            SocketMessage::Query(query) => {
                let response = match query {
                    StateQuery::FocusedMonitorIndex => self.focused_monitor_idx().to_string(),
                    StateQuery::FocusedWorkspaceIndex => self
                        .focused_monitor()
                        .ok_or_eyre("there is no monitor")?
                        .focused_workspace_idx()
                        .to_string(),
                    StateQuery::FocusedContainerIndex => self
                        .focused_workspace()?
                        .focused_container_idx()
                        .to_string(),
                    StateQuery::FocusedWindowIndex => {
                        self.focused_container()?.focused_window_idx().to_string()
                    }
                    StateQuery::FocusedWorkspaceName => {
                        let focused_monitor =
                            self.focused_monitor().ok_or_eyre("there is no monitor")?;

                        focused_monitor
                            .focused_workspace_name()
                            .unwrap_or_else(|| focused_monitor.focused_workspace_idx().to_string())
                    }
                    StateQuery::Version => build::RUST_VERSION.to_string(),
                    StateQuery::FocusedWorkspaceLayout => {
                        let focused_monitor =
                            self.focused_monitor().ok_or_eyre("there is no monitor")?;

                        focused_monitor.focused_workspace_layout().map_or_else(
                            || "None".to_string(),
                            |layout| match layout {
                                Layout::Default(default_layout) => default_layout.to_string(),
                                Layout::Custom(_) => "Custom".to_string(),
                            },
                        )
                    }
                    StateQuery::FocusedContainerKind => {
                        match self.focused_workspace()?.focused_container() {
                            None => "None".to_string(),
                            Some(container) => {
                                if container.windows().len() > 1 {
                                    "Stack".to_string()
                                } else {
                                    "Single".to_string()
                                }
                            }
                        }
                    }
                };

                reply.write_all(response.as_bytes())?;
            }
            SocketMessage::ResizeWindowEdge(direction, sizing) => {
                self.resize_window(direction, sizing, self.resize_delta, true)?;
            }
            SocketMessage::ResizeWindowAxis(axis, sizing) => {
                // If the user has a custom layout, allow for the resizing of the primary column
                // with this signal
                let workspace = self.focused_workspace_mut()?;
                let container_len = workspace.containers().len();
                let no_layout_rules = workspace.layout_rules.is_empty();

                if let Layout::Custom(custom) = &mut workspace.layout {
                    if matches!(axis, Axis::Horizontal) {
                        #[allow(clippy::cast_precision_loss)]
                        let percentage = custom
                            .primary_width_percentage()
                            .unwrap_or(100.0 / (custom.len() as f32));

                        if no_layout_rules {
                            match sizing {
                                Sizing::Increase => {
                                    custom.set_primary_width_percentage(percentage + 5.0);
                                }
                                Sizing::Decrease => {
                                    custom.set_primary_width_percentage(percentage - 5.0);
                                }
                            }
                        } else {
                            for rule in &mut workspace.layout_rules {
                                if container_len >= rule.0
                                    && let Layout::Custom(ref mut custom) = rule.1
                                {
                                    match sizing {
                                        Sizing::Increase => {
                                            custom.set_primary_width_percentage(percentage + 5.0);
                                        }
                                        Sizing::Decrease => {
                                            custom.set_primary_width_percentage(percentage - 5.0);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Otherwise proceed with the resizing logic for individual window containers in the
                    // assumed BSP layout
                } else {
                    match axis {
                        Axis::Horizontal => {
                            self.resize_window(
                                OperationDirection::Left,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                            self.resize_window(
                                OperationDirection::Right,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                        }
                        Axis::Vertical => {
                            self.resize_window(
                                OperationDirection::Up,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                            self.resize_window(
                                OperationDirection::Down,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                        }
                        Axis::HorizontalAndVertical => {
                            self.resize_window(
                                OperationDirection::Left,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                            self.resize_window(
                                OperationDirection::Right,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                            self.resize_window(
                                OperationDirection::Up,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                            self.resize_window(
                                OperationDirection::Down,
                                sizing,
                                self.resize_delta,
                                false,
                            )?;
                        }
                    }
                }

                self.update_focused_workspace(false, false)?;
            }
            SocketMessage::FocusFollowsMouse(mut implementation, enable) => {
                if !CUSTOM_FFM.load(Ordering::SeqCst) {
                    tracing::warn!(
                        "komorebi was not started with the --ffm flag, so the komorebi implementation of focus follows mouse cannot be enabled; defaulting to windows implementation"
                    );
                    implementation = FocusFollowsMouseImplementation::Windows;
                }

                match implementation {
                    FocusFollowsMouseImplementation::Komorebi => {
                        if WindowsApi::focus_follows_mouse()? {
                            tracing::warn!(
                                "the komorebi implementation of focus follows mouse cannot be enabled while the windows implementation is enabled"
                            );
                        } else if enable {
                            self.focus_follows_mouse = Option::from(implementation);
                        } else {
                            self.focus_follows_mouse = None;
                            self.has_pending_raise_op = false;
                        }
                    }
                    FocusFollowsMouseImplementation::Windows => {
                        if matches!(
                            self.focus_follows_mouse,
                            Some(FocusFollowsMouseImplementation::Komorebi)
                        ) {
                            tracing::warn!(
                                "the windows implementation of focus follows mouse cannot be enabled while the komorebi implementation is enabled"
                            );
                        } else if enable {
                            WindowsApi::enable_focus_follows_mouse()?;
                            self.focus_follows_mouse =
                                Option::from(FocusFollowsMouseImplementation::Windows);
                        } else {
                            WindowsApi::disable_focus_follows_mouse()?;
                            self.focus_follows_mouse = None;
                        }
                    }
                }
            }
            SocketMessage::ToggleFocusFollowsMouse(mut implementation) => {
                if !CUSTOM_FFM.load(Ordering::SeqCst) {
                    tracing::warn!(
                        "komorebi was not started with the --ffm flag, so the komorebi implementation of focus follows mouse cannot be toggled; defaulting to windows implementation"
                    );
                    implementation = FocusFollowsMouseImplementation::Windows;
                }

                match implementation {
                    FocusFollowsMouseImplementation::Komorebi => {
                        if WindowsApi::focus_follows_mouse()? {
                            tracing::warn!(
                                "the komorebi implementation of focus follows mouse cannot be toggled while the windows implementation is enabled"
                            );
                        } else {
                            match self.focus_follows_mouse {
                                None => {
                                    self.focus_follows_mouse = Option::from(implementation);
                                    self.has_pending_raise_op = false;
                                }
                                Some(FocusFollowsMouseImplementation::Komorebi) => {
                                    self.focus_follows_mouse = None;
                                }
                                Some(FocusFollowsMouseImplementation::Windows) => {
                                    tracing::warn!(
                                        "ignoring command that could mix different focus follows mouse implementations"
                                    );
                                }
                            }
                        }
                    }
                    FocusFollowsMouseImplementation::Windows => {
                        if matches!(
                            self.focus_follows_mouse,
                            Some(FocusFollowsMouseImplementation::Komorebi)
                        ) {
                            tracing::warn!(
                                "the windows implementation of focus follows mouse cannot be toggled while the komorebi implementation is enabled"
                            );
                        } else {
                            match self.focus_follows_mouse {
                                None => {
                                    WindowsApi::enable_focus_follows_mouse()?;
                                    self.focus_follows_mouse = Option::from(implementation);
                                }
                                Some(FocusFollowsMouseImplementation::Windows) => {
                                    WindowsApi::disable_focus_follows_mouse()?;
                                    self.focus_follows_mouse = None;
                                }
                                Some(FocusFollowsMouseImplementation::Komorebi) => {
                                    tracing::warn!(
                                        "ignoring command that could mix different focus follows mouse implementations"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            SocketMessage::ReloadConfiguration => {
                Self::reload_configuration();
                force_update_borders = true;
            }
            SocketMessage::ReplaceConfiguration(ref config) => {
                // Check that this is a valid static config file first
                if StaticConfig::read(config).is_ok() {
                    // Clear workspace rules; these will need to be replaced
                    WORKSPACE_MATCHING_RULES.lock().clear();
                    // Pause so that restored windows come to the foreground from all workspaces
                    self.is_paused = true;
                    // Bring all windows to the foreground
                    self.restore_all_windows(false)?;

                    // Create a new wm from the config path
                    let mut wm = StaticConfig::preload(
                        config,
                        winevent_listener::event_rx(),
                        self.command_listener.try_clone().ok(),
                    )?;

                    // Initialize the new wm
                    wm.init()?;

                    wm.restore_all_windows(true)?;

                    // This is equivalent to StaticConfig::postload for this use case
                    StaticConfig::reload(config, &mut wm)?;

                    // Set self to the new wm instance
                    *self = wm;

                    // check if there are any bars
                    let mut system = sysinfo::System::new_all();
                    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

                    let has_bar = system
                        .processes_by_name("komorebi-bar.exe".as_ref())
                        .next()
                        .is_some();

                    // stop bar(s)
                    if has_bar {
                        let script = r"
Stop-Process -Name:komorebi-bar -ErrorAction SilentlyContinue
                ";
                        match powershell_script::run(script) {
                            Ok(_) => {
                                println!("{script}");

                                // start new bar(s)
                                let mut config = StaticConfig::read(config)?;
                                if let Some(display_bar_configurations) =
                                    &mut config.bar_configurations
                                {
                                    for config_file_path in &mut *display_bar_configurations {
                                        let script = r#"Start-Process "komorebi-bar" '"--config" "CONFIGFILE"' -WindowStyle hidden"#
                                            .replace("CONFIGFILE", &config_file_path.to_string_lossy());

                                        match powershell_script::run(&script) {
                                            Ok(_) => {
                                                println!("{script}");
                                            }
                                            Err(error) => {
                                                println!("Error: {error}");
                                            }
                                        }
                                    }
                                } else {
                                    let script = r"
if (!(Get-Process komorebi-bar -ErrorAction SilentlyContinue))
{
  Start-Process komorebi-bar -WindowStyle hidden
}
                ";
                                    match powershell_script::run(script) {
                                        Ok(_) => {
                                            println!("{script}");
                                        }
                                        Err(error) => {
                                            println!("Error: {error}");
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                println!("Error: {error}");
                            }
                        }
                    }

                    force_update_borders = true;
                }
            }
            SocketMessage::ReloadStaticConfiguration(ref pathbuf) => {
                self.reload_static_configuration(pathbuf)?;
                force_update_borders = true;
            }
            SocketMessage::CompleteConfiguration => {
                if !INITIAL_CONFIGURATION_LOADED.load(Ordering::SeqCst) {
                    INITIAL_CONFIGURATION_LOADED.store(true, Ordering::SeqCst);
                    self.update_focused_workspace(false, false)?;
                    force_update_borders = true;
                }
            }
            SocketMessage::WatchConfiguration(enable) => {
                self.watch_configuration(enable)?;
            }
            SocketMessage::IdentifyObjectNameChangeApplication(identifier, ref id) => {
                let mut identifiers = OBJECT_NAME_CHANGE_ON_LAUNCH.lock();

                let mut should_push = true;
                for i in &*identifiers {
                    if let MatchingRule::Simple(i) = i
                        && i.id.eq(id)
                    {
                        should_push = false;
                    }
                }

                if should_push {
                    identifiers.push(MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.clone(),
                        matching_strategy: Option::from(MatchingStrategy::Legacy),
                    }));
                }
            }
            SocketMessage::IdentifyTrayApplication(identifier, ref id) => {
                let mut identifiers = TRAY_AND_MULTI_WINDOW_IDENTIFIERS.lock();
                let mut should_push = true;
                for i in &*identifiers {
                    if let MatchingRule::Simple(i) = i
                        && i.id.eq(id)
                    {
                        should_push = false;
                    }
                }

                if should_push {
                    identifiers.push(MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.clone(),
                        matching_strategy: Option::from(MatchingStrategy::Legacy),
                    }));
                }
            }
            SocketMessage::IdentifyLayeredApplication(identifier, ref id) => {
                let mut identifiers = LAYERED_WHITELIST.lock();

                let mut should_push = true;
                for i in &*identifiers {
                    if let MatchingRule::Simple(i) = i
                        && i.id.eq(id)
                    {
                        should_push = false;
                    }
                }

                if should_push {
                    identifiers.push(MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.clone(),
                        matching_strategy: Option::from(MatchingStrategy::Legacy),
                    }));
                }
            }
            SocketMessage::ManageFocusedWindow => {
                self.manage_focused_window()?;
            }
            SocketMessage::UnmanageFocusedWindow => {
                self.unmanage_focused_window()?;
            }
            SocketMessage::InvisibleBorders(_rect) => {}
            SocketMessage::WorkAreaOffset(rect) => {
                self.work_area_offset = Option::from(rect);
                self.retile_all(false)?;
            }
            SocketMessage::MonitorWorkAreaOffset(monitor_idx, rect) => {
                if let Some(monitor) = self.monitors_mut().get_mut(monitor_idx) {
                    monitor.work_area_offset = Option::from(rect);
                }

                // Only the affected monitor's focused workspace needs to be
                // re-rendered; leave all other monitors untouched.
                if let Some(monitor) = self.monitors().get(monitor_idx) {
                    let workspace_idx = monitor.focused_workspace_idx();
                    self.retile_workspace_on_monitor(monitor_idx, workspace_idx, false)?;
                }
            }
            SocketMessage::WorkspaceWorkAreaOffset(monitor_idx, workspace_idx, rect) => {
                if let Some(monitor) = self.monitors_mut().get_mut(monitor_idx)
                    && let Some(workspace) = monitor.workspaces_mut().get_mut(workspace_idx)
                {
                    workspace.work_area_offset = Option::from(rect);
                    self.retile_workspace_on_monitor(monitor_idx, workspace_idx, false)?
                }
            }
            SocketMessage::ToggleWindowBasedWorkAreaOffset => {
                let monitor_idx = self.focused_monitor_idx();
                let workspace = self.focused_workspace_mut()?;
                workspace.apply_window_based_work_area_offset =
                    !workspace.apply_window_based_work_area_offset;

                let workspace_idx = self.focused_workspace_idx()?;
                self.retile_workspace_on_monitor(monitor_idx, workspace_idx, true)?;
            }
            SocketMessage::QuickSave => {
                let workspace = self.focused_workspace()?;
                let resize = &workspace.resize_dimensions;

                let quicksave_json = std::env::temp_dir().join("komorebi.quicksave.json");

                let file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(quicksave_json)?;

                serde_json::to_writer_pretty(&file, &resize)?;
            }
            SocketMessage::QuickLoad => {
                let workspace = self.focused_workspace_mut()?;

                let quicksave_json = std::env::temp_dir().join("komorebi.quicksave.json");

                let file = File::open(&quicksave_json).wrap_err(format!(
                    "no quicksave found at {}",
                    quicksave_json.display()
                ))?;

                let resize: Vec<Option<Rect>> = serde_json::from_reader(file)?;

                workspace.resize_dimensions = resize;
                self.update_focused_workspace(false, false)?;
            }
            SocketMessage::Save(ref path) => {
                let workspace = self.focused_workspace_mut()?;
                let resize = &workspace.resize_dimensions;

                let file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(path)?;

                serde_json::to_writer_pretty(&file, &resize)?;
            }
            SocketMessage::Load(ref path) => {
                let workspace = self.focused_workspace_mut()?;

                let file =
                    File::open(path).wrap_err(format!("no file found at {}", path.display()))?;

                let resize: Vec<Option<Rect>> = serde_json::from_reader(file)?;

                workspace.resize_dimensions = resize;
                self.update_focused_workspace(false, false)?;
            }
            SocketMessage::AddSubscriberSocket(ref socket) => {
                let mut sockets = SUBSCRIPTION_SOCKETS.lock();
                let socket_path = DATA_DIR.join(socket);
                sockets.insert(socket.clone(), socket_path);
            }
            SocketMessage::AddSubscriberSocketWithOptions(ref socket, options) => {
                let mut sockets = SUBSCRIPTION_SOCKETS.lock();
                let socket_path = DATA_DIR.join(socket);
                sockets.insert(socket.clone(), socket_path);

                let mut socket_options = SUBSCRIPTION_SOCKET_OPTIONS.lock();
                socket_options.insert(socket.clone(), options);
            }
            SocketMessage::RemoveSubscriberSocket(ref socket) => {
                let mut sockets = SUBSCRIPTION_SOCKETS.lock();
                sockets.remove(socket);
            }
            SocketMessage::AddSubscriberPipe(ref subscriber) => {
                let mut pipes = SUBSCRIPTION_PIPES.lock();
                let pipe_path = format!(r"\\.\pipe\{subscriber}");
                let pipe = connect(&pipe_path).wrap_err(
                    format!("the named pipe '{}' has not yet been created; please create it before running this command", pipe_path)
                )?;

                pipes.insert(subscriber.clone(), pipe);
            }
            SocketMessage::RemoveSubscriberPipe(ref subscriber) => {
                let mut pipes = SUBSCRIPTION_PIPES.lock();
                pipes.remove(subscriber);
            }
            SocketMessage::MouseFollowsFocus(enable) => {
                self.mouse_follows_focus = enable;
            }
            SocketMessage::ToggleMouseFollowsFocus => {
                self.mouse_follows_focus = !self.mouse_follows_focus;
            }
            SocketMessage::ResizeDelta(delta) => {
                self.resize_delta = delta;
            }
            SocketMessage::ToggleWindowContainerBehaviour => {
                match self.window_management_behaviour.current_behaviour {
                    WindowContainerBehaviour::Create => {
                        self.window_management_behaviour.current_behaviour =
                            WindowContainerBehaviour::Append;
                    }
                    WindowContainerBehaviour::Append => {
                        self.window_management_behaviour.current_behaviour =
                            WindowContainerBehaviour::Create;
                    }
                }
            }
            SocketMessage::ToggleFloatOverride => {
                self.window_management_behaviour.float_override =
                    !self.window_management_behaviour.float_override;
            }
            SocketMessage::ToggleWorkspaceWindowContainerBehaviour => {
                let current_global_behaviour = self.window_management_behaviour.current_behaviour;
                if let Some(behaviour) =
                    &mut self.focused_workspace_mut()?.window_container_behaviour
                {
                    match behaviour {
                        WindowContainerBehaviour::Create => {
                            *behaviour = WindowContainerBehaviour::Append
                        }
                        WindowContainerBehaviour::Append => {
                            *behaviour = WindowContainerBehaviour::Create
                        }
                    }
                } else {
                    self.focused_workspace_mut()?.window_container_behaviour =
                        Some(match current_global_behaviour {
                            WindowContainerBehaviour::Create => WindowContainerBehaviour::Append,
                            WindowContainerBehaviour::Append => WindowContainerBehaviour::Create,
                        });
                };
            }
            SocketMessage::ToggleWorkspaceFloatOverride => {
                let current_global_override = self.window_management_behaviour.float_override;
                if let Some(float_override) = &mut self.focused_workspace_mut()?.float_override {
                    *float_override = !*float_override;
                } else {
                    self.focused_workspace_mut()?.float_override = Some(!current_global_override);
                };
            }
            SocketMessage::WindowHidingBehaviour(behaviour) => {
                let mut hiding_behaviour = HIDING_BEHAVIOUR.lock();
                *hiding_behaviour = behaviour;
            }
            SocketMessage::ToggleCrossMonitorMoveBehaviour => {
                match self.cross_monitor_move_behaviour {
                    MoveBehaviour::Swap => {
                        self.cross_monitor_move_behaviour = MoveBehaviour::Insert;
                    }
                    MoveBehaviour::Insert => {
                        self.cross_monitor_move_behaviour = MoveBehaviour::Swap;
                    }
                    _ => {}
                }
            }
            SocketMessage::CrossMonitorMoveBehaviour(behaviour) => {
                self.cross_monitor_move_behaviour = behaviour;
            }
            SocketMessage::ToggleMonocleFocusBehaviour => {
                self.monocle_focus_behaviour = match self.monocle_focus_behaviour {
                    MonocleFocusBehaviour::Cycle => MonocleFocusBehaviour::NoOp,
                    MonocleFocusBehaviour::NoOp => MonocleFocusBehaviour::Cycle,
                };
            }
            SocketMessage::MonocleFocusBehaviour(behaviour) => {
                self.monocle_focus_behaviour = behaviour;
            }
            SocketMessage::UnmanagedWindowOperationBehaviour(behaviour) => {
                self.unmanaged_window_operation_behaviour = behaviour;
            }
            SocketMessage::Border(enable) => {
                border_manager::BORDER_ENABLED.store(enable, Ordering::SeqCst);
                if !enable {
                    match IMPLEMENTATION.load() {
                        BorderImplementation::Komorebi => {
                            border_manager::destroy_all_borders()?;
                        }
                        BorderImplementation::Windows => {
                            self.remove_all_accents()?;
                        }
                    }
                } else if matches!(IMPLEMENTATION.load(), BorderImplementation::Komorebi) {
                    force_update_borders = true;
                }
            }
            SocketMessage::BorderImplementation(implementation) => {
                if !*WINDOWS_11 && matches!(implementation, BorderImplementation::Windows) {
                    tracing::error!(
                        "BorderImplementation::Windows is only supported on Windows 11 and above"
                    );
                } else {
                    IMPLEMENTATION.store(implementation);
                    match IMPLEMENTATION.load() {
                        BorderImplementation::Komorebi => {
                            self.remove_all_accents()?;
                            force_update_borders = true;
                        }
                        BorderImplementation::Windows => {
                            border_manager::destroy_all_borders()?;
                        }
                    }
                }
            }
            SocketMessage::BorderColour(kind, r, g, b) => {
                match kind {
                    WindowKind::Single => {
                        border_manager::FOCUSED.store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                    WindowKind::Stack => {
                        border_manager::STACK.store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                    WindowKind::Monocle => {
                        border_manager::MONOCLE.store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                    WindowKind::Unfocused => {
                        border_manager::UNFOCUSED.store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                    WindowKind::UnfocusedLocked => {
                        border_manager::UNFOCUSED_LOCKED
                            .store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                    WindowKind::Floating => {
                        border_manager::FLOATING.store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                    WindowKind::Pinned => {
                        border_manager::PINNED.store(Rgb::new(r, g, b).into(), Ordering::SeqCst);
                    }
                }
                force_update_borders = true;
            }
            SocketMessage::BorderStyle(style) => {
                STYLE.store(style);
                force_update_borders = true;
            }
            SocketMessage::BorderWidth(width) => {
                border_manager::BORDER_WIDTH.store(width, Ordering::SeqCst);
                force_update_borders = true;
            }
            SocketMessage::BorderOffset(offset) => {
                border_manager::BORDER_OFFSET.store(offset, Ordering::SeqCst);
                force_update_borders = true;
            }
            SocketMessage::Animation(enable, prefix) => match prefix {
                Some(prefix) => {
                    ANIMATION_ENABLED_PER_ANIMATION
                        .lock()
                        .insert(prefix, enable);
                }
                None => {
                    ANIMATION_ENABLED_GLOBAL.store(enable, Ordering::SeqCst);
                    ANIMATION_ENABLED_PER_ANIMATION.lock().clear();
                }
            },
            SocketMessage::AnimationDuration(duration, prefix) => match prefix {
                Some(prefix) => {
                    ANIMATION_DURATION_PER_ANIMATION
                        .lock()
                        .insert(prefix, duration);
                }
                None => {
                    ANIMATION_DURATION_GLOBAL.store(duration, Ordering::SeqCst);
                    ANIMATION_DURATION_PER_ANIMATION.lock().clear();
                }
            },
            SocketMessage::AnimationFps(fps) => {
                ANIMATION_FPS.store(fps, Ordering::SeqCst);
            }
            SocketMessage::AnimationStyle(style, prefix) => match prefix {
                Some(prefix) => {
                    ANIMATION_STYLE_PER_ANIMATION.lock().insert(prefix, style);
                }
                None => {
                    let mut animation_style = ANIMATION_STYLE_GLOBAL.lock();
                    *animation_style = style;
                    ANIMATION_STYLE_PER_ANIMATION.lock().clear();
                }
            },
            SocketMessage::ToggleTransparency => {
                let current = transparency_manager::TRANSPARENCY_ENABLED.load(Ordering::SeqCst);
                transparency_manager::TRANSPARENCY_ENABLED.store(!current, Ordering::SeqCst);
            }
            SocketMessage::Transparency(enable) => {
                transparency_manager::TRANSPARENCY_ENABLED.store(enable, Ordering::SeqCst);
            }
            SocketMessage::TransparencyAlpha(alpha) => {
                transparency_manager::TRANSPARENCY_ALPHA.store(alpha, Ordering::SeqCst);
            }
            SocketMessage::ToggleTransparencyFloating => {
                let current =
                    transparency_manager::TRANSPARENCY_FLOATING.load(Ordering::SeqCst);
                transparency_manager::TRANSPARENCY_FLOATING.store(!current, Ordering::SeqCst);
            }
            SocketMessage::TransparencyFloating(enable) => {
                transparency_manager::TRANSPARENCY_FLOATING.store(enable, Ordering::SeqCst);
            }
            SocketMessage::ToggleTransparencyMonocle => {
                let current = transparency_manager::TRANSPARENCY_MONOCLE.load(Ordering::SeqCst);
                transparency_manager::TRANSPARENCY_MONOCLE.store(!current, Ordering::SeqCst);
            }
            SocketMessage::TransparencyMonocle(enable) => {
                transparency_manager::TRANSPARENCY_MONOCLE.store(enable, Ordering::SeqCst);
            }
            SocketMessage::StackbarMode(mode) => {
                STACKBAR_MODE.store(mode);
                self.retile_all(true)?;
            }
            SocketMessage::StackbarLabel(label) => {
                STACKBAR_LABEL.store(label);
            }
            SocketMessage::StackbarFocusedTextColour(r, g, b) => {
                let rgb = Rgb::new(r, g, b);
                STACKBAR_FOCUSED_TEXT_COLOUR.store(rgb.into(), Ordering::SeqCst);
            }
            SocketMessage::StackbarUnfocusedTextColour(r, g, b) => {
                let rgb = Rgb::new(r, g, b);
                STACKBAR_UNFOCUSED_TEXT_COLOUR.store(rgb.into(), Ordering::SeqCst);
            }
            SocketMessage::StackbarBackgroundColour(r, g, b) => {
                let rgb = Rgb::new(r, g, b);
                STACKBAR_TAB_BACKGROUND_COLOUR.store(rgb.into(), Ordering::SeqCst);
            }
            SocketMessage::StackbarHeight(height) => {
                STACKBAR_TAB_HEIGHT.store(height, Ordering::SeqCst);
            }
            SocketMessage::StackbarTabWidth(width) => {
                STACKBAR_TAB_WIDTH.store(width, Ordering::SeqCst);
            }
            SocketMessage::StackbarFontSize(size) => {
                STACKBAR_FONT_SIZE.store(size, Ordering::SeqCst);
            }
            #[allow(clippy::assigning_clones)]
            SocketMessage::StackbarFontFamily(ref font_family) => {
                *STACKBAR_FONT_FAMILY.lock() = font_family.clone();
            }
            SocketMessage::ApplicationSpecificConfigurationSchema => {
                #[cfg(feature = "schemars")]
                {
                    let asc = schemars::schema_for!(
                        Vec<crate::core::config_generation::ApplicationConfiguration>
                    );
                    let schema = serde_json::to_string_pretty(&asc)?;

                    reply.write_all(schema.as_bytes())?;
                }
            }
            SocketMessage::NotificationSchema => {
                #[cfg(feature = "schemars")]
                {
                    let notification = schemars::schema_for!(crate::Notification);
                    let schema = serde_json::to_string_pretty(&notification)?;

                    reply.write_all(schema.as_bytes())?;
                }
            }
            SocketMessage::SocketSchema => {
                #[cfg(feature = "schemars")]
                {
                    let socket_message = schemars::schema_for!(SocketMessage);
                    let schema = serde_json::to_string_pretty(&socket_message)?;

                    reply.write_all(schema.as_bytes())?;
                }
            }
            SocketMessage::StaticConfigSchema => {
                #[cfg(feature = "schemars")]
                {
                    let socket_message = schemars::schema_for!(SocketMessage);
                    let schema = serde_json::to_string_pretty(&socket_message)?;

                    reply.write_all(schema.as_bytes())?;
                }
            }
            SocketMessage::GenerateStaticConfig => {
                let config = serde_json::to_string_pretty(&StaticConfig::from(&*self))?;

                reply.write_all(config.as_bytes())?;
            }
            SocketMessage::RemoveTitleBar(identifier, ref id) => {
                let mut identifiers = NO_TITLEBAR.lock();

                let mut should_push = true;
                for i in &*identifiers {
                    if let MatchingRule::Simple(i) = i
                        && i.id.eq(id)
                    {
                        should_push = false;
                    }
                }

                if should_push {
                    identifiers.push(MatchingRule::Simple(IdWithIdentifier {
                        kind: identifier,
                        id: id.clone(),
                        matching_strategy: Option::from(MatchingStrategy::Legacy),
                    }));
                }
            }
            SocketMessage::ToggleTitleBars => {
                let current = REMOVE_TITLEBARS.load(Ordering::SeqCst);
                REMOVE_TITLEBARS.store(!current, Ordering::SeqCst);
                self.update_focused_workspace(false, false)?;
            }
            SocketMessage::DebugWindow(hwnd) => {
                let window = Window::from(hwnd);
                let mut rule_debug = RuleDebug::default();
                let _ = window.should_manage(None, &mut rule_debug);
                let schema = serde_json::to_string_pretty(&rule_debug)?;

                reply.write_all(schema.as_bytes())?;
            }
            SocketMessage::Theme(ref theme) => {
                theme_manager::send_notification(*theme.clone());
            }
            SocketMessage::ApplyState(ref state) => {
                self.apply_state(state.clone());
            }
            // Deprecated commands
            SocketMessage::AltFocusHack(_)
            | SocketMessage::IdentifyBorderOverflowApplication(_, _) => {}
        };

        // Update the list of known_hwnds and their monitor/workspace index pair.
        // This rebuilds the entire hwnd map, so only do it when the command
        // actually changed the state; every topology-touching command (managing,
        // unmanaging, moving or closing a window) also flips the compared state
        // via window membership in workspaces. `state_has_been_modified` is
        // computed once and reused below to skip notifying subscribers.
        let state_has_been_modified = initial_state.has_been_modified(self.as_ref());
        if state_has_been_modified {
            self.update_known_hwnds();
        }

        notify_subscribers(
            NotificationEvent::Socket(message.clone()),
            state_has_been_modified,
            || self.as_ref().into(),
        )?;

        if force_update_borders {
            border_manager::send_force_update();
        } else {
            border_manager::send_notification(None);
        }
        transparency_manager::send_notification();
        stackbar_manager::send_notification();

        tracing::info!("processed");
        Ok(())
    }
}

pub fn read_commands_uds(
    wm: &Arc<Mutex<WindowManager>>,
    mut stream: UnixStream,
) -> eyre::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    // TODO(raggi): while this processes more than one command, if there are
    // replies there is no clearly defined protocol for framing yet - it's
    // perhaps whole-json objects for now, but termination is signalled by
    // socket shutdown.
    for line in reader.lines() {
        let message = SocketMessage::from_str(&line?)?;

        // Wait a bounded time for the window-manager lock instead of dropping
        // the command outright. The lock is only ever held across a single
        // event-loop pass (z-order application happens on the apply worker and
        // blocking window operations are probe-guarded), so a missed acquisition
        // is transient. If the lock is still held after the budget - a path the
        // guards failed to cover - the command is dropped with a clear error
        // rather than stalling the connection forever.
        let Some(mut wm) = acquire_wm_lock(wm, &message) else {
            continue;
        };

        if wm.is_paused {
            return match message {
                SocketMessage::TogglePause
                | SocketMessage::State
                | SocketMessage::GlobalState
                | SocketMessage::Stop => Ok(wm.process_command(message, &mut stream)?),
                _ => {
                    tracing::trace!("ignoring while paused");
                    Ok(())
                }
            };
        }

        wm.process_command(message.clone(), &mut stream)?;
    }

    Ok(())
}

/// Try to acquire the `WindowManager` lock for at most [`LOCK_WAIT_BUDGET`],
/// reporting unusually long acquisitions.
///
/// Returns `None` if the lock is still contended after the budget: the caller
/// drops the command (with a clear log line, never silently) instead of holding
/// the command connection open forever on a wedged window-manager thread.
fn acquire_wm_lock<'a>(
    wm: &'a Arc<Mutex<WindowManager>>,
    message: &SocketMessage,
) -> Option<parking_lot::MutexGuard<'a, WindowManager>> {
    const LOCK_WAIT_BUDGET: Duration = Duration::from_secs(10);
    const CONTENTION_WARN_AT: Duration = Duration::from_secs(2);
    const ATTEMPT_STEP: Duration = Duration::from_millis(100);

    let started = Instant::now();
    let mut contention_warned = false;
    loop {
        match wm.try_lock_for(ATTEMPT_STEP) {
            Some(wm) => return Some(wm),
            None => {
                let elapsed = started.elapsed();
                if !contention_warned && elapsed >= CONTENTION_WARN_AT {
                    tracing::warn!(
                        "window manager lock held for {elapsed:?}, still waiting before processing message: {message}"
                    );
                    contention_warned = true;
                }
                if elapsed >= LOCK_WAIT_BUDGET {
                    tracing::error!(
                        "timed out after {elapsed:?} waiting for the window manager lock, dropping message: {message}"
                    );
                    return None;
                }
            }
        }
    }
}

pub fn read_commands_tcp(
    wm: &Arc<Mutex<WindowManager>>,
    stream: &mut TcpStream,
    addr: &str,
) -> eyre::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    loop {
        let mut buf = vec![0; 1024];
        match reader.read(&mut buf) {
            Err(..) => {
                tracing::warn!("removing disconnected tcp client: {addr}");
                let mut connections = TCP_CONNECTIONS.lock();
                connections.remove(addr);
                break;
            }
            Ok(size) => {
                let Ok(message) = SocketMessage::from_str(&String::from_utf8_lossy(&buf[..size]))
                else {
                    tracing::warn!("client sent an invalid message, disconnecting: {addr}");
                    let mut connections = TCP_CONNECTIONS.lock();
                    connections.remove(addr);
                    break;
                };

                let Some(mut wm) = acquire_wm_lock(wm, &message) else {
                    continue;
                };

                if wm.is_paused {
                    return match message {
                        SocketMessage::TogglePause
                        | SocketMessage::State
                        | SocketMessage::GlobalState
                        | SocketMessage::Stop => Ok(wm.process_command(message, stream)?),
                        _ => {
                            tracing::trace!("ignoring while paused");
                            Ok(())
                        }
                    };
                }

                wm.process_command(message.clone(), &mut *stream)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Rect;
    use crate::SocketMessage;
    use crate::WindowManagerEvent;
    use crate::monitor;
    use crate::window_manager::WindowManager;
    use crossbeam_channel::Receiver;
    use crossbeam_channel::Sender;
    use crossbeam_channel::bounded;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::time::Duration;
    use uds_windows::UnixStream;
    use uuid::Uuid;

    fn send_socket_message(socket: &PathBuf, message: SocketMessage) {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .write_all(serde_json::to_string(&message).unwrap().as_bytes())
            .unwrap();
    }

    #[test]
    fn test_receive_socket_message() {
        let (_sender, receiver): (Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>) =
            bounded(1);
        let socket_name = format!("komorebi-test-{}.sock", Uuid::new_v4());
        let socket_path = PathBuf::from(&socket_name);
        let mut wm = WindowManager::new(receiver, Some(socket_path.clone())).unwrap();
        let m = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        wm.monitors_mut().push_back(m);

        // send a message
        send_socket_message(&socket_path, SocketMessage::FocusWorkspaceNumber(5));

        let (stream, _) = wm.command_listener.accept().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let next = reader.lines().next();

        // read and deserialize the message
        let message_string = next.unwrap().unwrap();
        let message = SocketMessage::from_str(&message_string).unwrap();
        assert!(matches!(message, SocketMessage::FocusWorkspaceNumber(5)));

        // process the message
        wm.process_command(message, stream).unwrap();

        // check the updated window manager state
        assert_eq!(wm.focused_workspace_idx().unwrap(), 5);

        std::fs::remove_file(socket_path).unwrap();
    }
}
