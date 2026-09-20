#![warn(clippy::all)]

use eframe::egui;
use eframe::egui::Color32;
use eframe::egui::ViewportBuilder;
use eframe::egui::color_picker::Alpha;

use komorebi_client::BorderStyle;
use komorebi_client::Colour;
use komorebi_client::DefaultLayout;
use komorebi_client::FocusFollowsMouseImplementation;
use komorebi_client::GlobalState;
use komorebi_client::Layout;
use komorebi_client::Rect;
use komorebi_client::Rgb;
use komorebi_client::RuleDebug;
use komorebi_client::SocketMessage;
use komorebi_client::StackbarLabel;
use komorebi_client::StackbarMode;
use komorebi_client::State;
use komorebi_client::SubscribeOptions;
use komorebi_client::Window;
use komorebi_client::WindowKind;
use regex::Regex;
use std::collections::HashMap;
use std::io::BufReader;
use std::io::Read;
use std::sync::mpsc;
use std::time::Duration;
use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

fn main() {
    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_always_on_top()
            .with_inner_size([560.0, 720.0]),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "komorebi-gui",
        native_options,
        Box::new(|cc| Ok(Box::new(KomorebiGui::new(cc)))),
    );
}

struct BorderColours {
    single: Color32,
    stack: Color32,
    monocle: Color32,
    floating: Color32,
    unfocused: Color32,
    unfocused_locked: Color32,
}

struct BorderConfig {
    border_enabled: bool,
    border_colours: BorderColours,
    border_style: BorderStyle,
    border_offset: i32,
    border_width: i32,
}

struct StackbarConfig {
    mode: StackbarMode,
    label: StackbarLabel,
    height: i32,
    width: i32,
    font_size: i32,
    focused_text_colour: Color32,
    unfocused_text_colour: Color32,
    background_colour: Color32,
}

struct FocusConfig {
    mouse_follows_focus: bool,
    focus_follows_mouse: Option<FocusFollowsMouseImplementation>,
    alt_focus_hack: bool,
}

struct AnimationConfig {
    enabled: bool,
    duration: u64,
    fps: u64,
}

struct TransparencyConfig {
    enabled: bool,
    alpha: u8,
    floating: bool,
    monocle: bool,
}

struct MonitorConfig {
    size: Rect,
    work_area_offset: Rect,
    workspaces: Vec<WorkspaceConfig>,
}

impl From<&komorebi_client::Monitor> for MonitorConfig {
    fn from(value: &komorebi_client::Monitor) -> Self {
        let mut workspaces = vec![];
        for ws in value.workspaces() {
            workspaces.push(WorkspaceConfig::from(ws));
        }

        Self {
            size: value.size,
            work_area_offset: value.work_area_offset.unwrap_or_default(),
            workspaces,
        }
    }
}

struct WorkspaceConfig {
    name: String,
    tile: bool,
    layout: DefaultLayout,
    container_padding: i32,
    workspace_padding: i32,
}

impl From<&komorebi_client::Workspace> for WorkspaceConfig {
    fn from(value: &komorebi_client::Workspace) -> Self {
        let layout = match value.layout {
            Layout::Default(layout) => layout,
            Layout::Custom(_) => DefaultLayout::BSP,
        };

        let name = value
            .name
            .to_owned()
            .unwrap_or_else(|| random_word::get(random_word::Lang::En).to_string());

        Self {
            layout,
            name,
            tile: value.tile,
            workspace_padding: value.workspace_padding.unwrap_or(20),
            container_padding: value.container_padding.unwrap_or(20),
        }
    }
}

enum GuiEvent {
    StateUpdate(Box<State>),
    Connected,
    Disconnected,
}

struct KomorebiGui {
    border_config: BorderConfig,
    stackbar_config: StackbarConfig,
    focus_config: FocusConfig,
    monitors: Vec<MonitorConfig>,
    workspace_names: HashMap<usize, Vec<String>>,
    debug_hwnd: isize,
    debug_windows: Vec<Window>,
    debug_rule: Option<RuleDebug>,
    animation_config: AnimationConfig,
    transparency_config: TransparencyConfig,
    resize_delta: i32,
    invisible_borders: Rect,
    live_updates: bool,
    subscription_connected: bool,
    event_rx: mpsc::Receiver<GuiEvent>,
    regex_pattern: String,
    regex_compiled: Option<Regex>,
    regex_error: Option<String>,
}

fn colour32(colour: Option<Colour>) -> Color32 {
    match colour {
        Some(Colour::Rgb(rgb)) => Color32::from_rgb(rgb.r as u8, rgb.g as u8, rgb.b as u8),
        Some(Colour::Hex(hex)) => {
            let rgb = Rgb::from(hex);
            Color32::from_rgb(rgb.r as u8, rgb.g as u8, rgb.b as u8)
        }
        None => Color32::from_rgb(0, 0, 0),
    }
}

fn load_global_state() -> Option<GlobalState> {
    komorebi_client::send_query(&SocketMessage::GlobalState)
        .ok()
        .and_then(|json| serde_json::from_str::<GlobalState>(&json).ok())
}

fn load_state() -> Option<State> {
    komorebi_client::send_query(&SocketMessage::State)
        .ok()
        .and_then(|json| serde_json::from_str::<State>(&json).ok())
}

extern "system" fn enum_window(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows_core::BOOL {
    let windows = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };
    let window = Window::from(hwnd.0 as isize);

    if window.is_window()
        && !window.is_miminized()
        && window.is_visible()
        && window.title().is_ok()
        && window.exe().is_ok()
    {
        windows.push(window);
    }

    true.into()
}

fn json_view_ui(ui: &mut egui::Ui, code: &str) {
    let language = "json";
    let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(ui.ctx(), &ui.ctx().style());
    egui_extras::syntax_highlighting::code_view_ui(ui, &theme, code, language);
}

fn send_message(message: SocketMessage) {
    let _ = komorebi_client::send_message(&message);
}

fn send_message_retile(message: SocketMessage) {
    let _ = komorebi_client::send_message(&message);
    let _ = komorebi_client::send_message(&SocketMessage::Retile);
}

impl KomorebiGui {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (event_tx, event_rx) = mpsc::channel();

        let global_state = load_global_state();
        let state = load_state();

        let border_colours = BorderColours {
            single: colour32(global_state.as_ref().and_then(|gs| gs.border_colours.single)),
            stack: colour32(global_state.as_ref().and_then(|gs| gs.border_colours.stack)),
            monocle: colour32(global_state.as_ref().and_then(|gs| gs.border_colours.monocle)),
            floating: colour32(global_state.as_ref().and_then(|gs| gs.border_colours.floating)),
            unfocused: colour32(global_state.as_ref().and_then(|gs| gs.border_colours.unfocused)),
            unfocused_locked: colour32(
                global_state.as_ref().and_then(|gs| gs.border_colours.unfocused_locked),
            ),
        };

        let border_config = BorderConfig {
            border_enabled: global_state
                .as_ref()
                .map_or(false, |gs| gs.border_enabled),
            border_colours,
            border_style: global_state
                .as_ref()
                .map_or(BorderStyle::System, |gs| gs.border_style),
            border_offset: global_state.as_ref().map_or(0, |gs| gs.border_offset),
            border_width: global_state.as_ref().map_or(0, |gs| gs.border_width),
        };

        let focus_config = FocusConfig {
            mouse_follows_focus: state.as_ref().map_or(false, |s| s.mouse_follows_focus),
            focus_follows_mouse: state
                .as_ref()
                .and_then(|s| s.focus_follows_mouse),
            alt_focus_hack: global_state
                .as_ref()
                .map_or(false, |gs| gs.custom_ffm),
        };

        let animation_config = AnimationConfig {
            enabled: false,
            duration: 200,
            fps: 60,
        };

        let transparency_config = TransparencyConfig {
            enabled: global_state
                .as_ref()
                .is_some_and(|gs| gs.transparency_enabled),
            alpha: global_state.as_ref().map_or(0, |gs| gs.transparency_alpha),
            floating: global_state
                .as_ref()
                .is_some_and(|gs| gs.transparency_floating),
            monocle: global_state
                .as_ref()
                .is_some_and(|gs| gs.transparency_monocle),
        };

        let resize_delta = state.as_ref().map_or(0, |s| s.resize_delta);

        let invisible_borders = Rect::default();

        let mut monitors = vec![];
        if let Some(ref s) = state {
            for m in s.monitors.elements() {
                monitors.push(MonitorConfig::from(m));
            }
        }

        let mut workspace_names = HashMap::new();
        for (monitor_idx, m) in monitors.iter().enumerate() {
            for ws in &m.workspaces {
                let names = workspace_names.entry(monitor_idx).or_insert_with(Vec::new);
                names.push(ws.name.clone());
            }
        }

        let stackbar_config = StackbarConfig {
            mode: global_state
                .as_ref()
                .map_or(StackbarMode::Never, |gs| gs.stackbar_mode),
            height: global_state.as_ref().map_or(0, |gs| gs.stackbar_height),
            width: global_state.as_ref().map_or(0, |gs| gs.stackbar_tab_width),
            font_size: 0,
            label: global_state
                .as_ref()
                .map_or(StackbarLabel::Process, |gs| gs.stackbar_label),
            focused_text_colour: colour32(
                global_state.as_ref().map(|gs| gs.stackbar_focused_text_colour.clone()),
            ),
            unfocused_text_colour: colour32(
                global_state
                    .as_ref()
                    .map(|gs| gs.stackbar_unfocused_text_colour.clone()),
            ),
            background_colour: colour32(
                global_state
                    .as_ref()
                    .map(|gs| gs.stackbar_tab_background_colour.clone()),
            ),
        };

        let event_tx_thread = event_tx.clone();
        std::thread::spawn(move || {
            let subscriber_name =
                format!("komorebi-gui-{}", random_word::get(random_word::Lang::En));

            loop {
                let listener = match komorebi_client::subscribe_with_options(
                    &subscriber_name,
                    SubscribeOptions {
                        filter_state_changes: true,
                    },
                ) {
                    Ok(l) => l,
                    Err(_) => {
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };

                let _ = event_tx_thread.send(GuiEvent::Connected);

                for stream in listener.incoming() {
                    match stream {
                        Ok(mut subscription) => {
                            let mut buffer = Vec::new();
                            let mut reader = BufReader::new(&mut subscription);
                            if let Ok(0) = reader.read_to_end(&mut buffer) {
                                break;
                            }

                            if let Ok(notification_string) = String::from_utf8(buffer) {
                                if let Ok(notification) =
                                    serde_json::from_str::<komorebi_client::Notification>(
                                        &notification_string,
                                    )
                                {
                                    let _ = event_tx_thread.send(GuiEvent::StateUpdate(Box::new(
                                        notification.state,
                                    )));
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }

                let _ = event_tx_thread.send(GuiEvent::Disconnected);
                std::thread::sleep(Duration::from_secs(1));
            }
        });

        let mut debug_windows = vec![];
        unsafe {
            EnumWindows(
                Some(enum_window),
                windows::Win32::Foundation::LPARAM(
                    &mut debug_windows as *mut Vec<Window> as isize,
                ),
            )
            .unwrap();
        };

        Self {
            border_config,
            focus_config,
            monitors,
            workspace_names,
            debug_hwnd: 0,
            debug_windows,
            stackbar_config,
            debug_rule: None,
            animation_config,
            transparency_config,
            resize_delta,
            invisible_borders,
            live_updates: true,
            subscription_connected: false,
            event_rx,
            regex_pattern: String::new(),
            regex_compiled: None,
            regex_error: None,
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                GuiEvent::StateUpdate(state) => {
                    if self.live_updates {
                        self.apply_state(*state);
                        self.refresh_debug_windows();
                    }
                }
                GuiEvent::Connected => {
                    self.subscription_connected = true;
                }
                GuiEvent::Disconnected => {
                    self.subscription_connected = false;
                }
            }
        }
    }

    fn refresh_debug_windows(&mut self) {
        let mut debug_windows = vec![];
        unsafe {
            EnumWindows(
                Some(enum_window),
                windows::Win32::Foundation::LPARAM(
                    &mut debug_windows as *mut Vec<Window> as isize,
                ),
            )
            .unwrap();
        };
        self.debug_windows = debug_windows;
    }

    fn apply_state(&mut self, state: State) {
        let mut monitors = vec![];
        for m in state.monitors.elements() {
            monitors.push(MonitorConfig::from(m));
        }

        let mut workspace_names = HashMap::new();
        for (monitor_idx, m) in monitors.iter().enumerate() {
            for ws in &m.workspaces {
                let names = workspace_names.entry(monitor_idx).or_insert_with(Vec::new);
                names.push(ws.name.clone());
            }
        }

        self.monitors = monitors;
        self.workspace_names = workspace_names;
        self.focus_config.mouse_follows_focus = state.mouse_follows_focus;
        self.focus_config.focus_follows_mouse = state.focus_follows_mouse;
        self.resize_delta = state.resize_delta;
    }
}

impl eframe::App for KomorebiGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events();

        egui::CentralPanel::default().show(ctx, |ui| {
            ctx.set_pixels_per_point(2.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.set_width(ctx.content_rect().width());

                egui::CollapsingHeader::new("Debugging")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .toggle_value(&mut self.live_updates, "Live Updates")
                                .changed()
                            {
                                if self.live_updates {
                                    if let Some(state) = load_state() {
                                        self.apply_state(state);
                                    }
                                    if let Some(gs) = load_global_state() {
                                        self.border_config.border_enabled = gs.border_enabled;
                                        self.border_config.border_style = gs.border_style;
                                        self.border_config.border_offset = gs.border_offset;
                                        self.border_config.border_width = gs.border_width;
                                        self.stackbar_config.mode = gs.stackbar_mode;
                                        self.stackbar_config.label = gs.stackbar_label;
                                        self.stackbar_config.height = gs.stackbar_height;
                                        self.stackbar_config.width = gs.stackbar_tab_width;
                                        self.transparency_config.enabled = gs.transparency_enabled;
                                        self.transparency_config.alpha = gs.transparency_alpha;
                                        self.transparency_config.floating = gs.transparency_floating;
                                        self.transparency_config.monocle = gs.transparency_monocle;
                                    }
                                    self.refresh_debug_windows();
                                }
                            }

                            if ui.button("Refresh Windows").clicked() {
                                self.refresh_debug_windows();
                            }
                        });

                        let total = self.debug_windows.len();
                        let invalid = self
                            .debug_windows
                            .iter()
                            .filter(|w| w.title().is_err())
                            .count();
                        ui.label(format!(
                            "Visible windows: {} ({:.0}%)",
                            total,
                            (total.saturating_sub(invalid)) as f64 / total.max(1) as f64 * 100.0
                        ));

                        ui.collapsing("Window Rules", |ui| {
                            let window = Window::from(self.debug_hwnd);

                            let label =
                                if let (Ok(title), Ok(exe)) = (window.title(), window.exe()) {
                                    format!("{title} ({exe})")
                                } else {
                                    String::from("Select a Window")
                                };

                            egui::ComboBox::from_label("Select a Window")
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    for w in &self.debug_windows {
                                        let Ok(title) = w.title() else { continue };
                                        let Ok(exe) = w.exe() else { continue };
                                        if ui
                                            .selectable_value(
                                                &mut self.debug_hwnd,
                                                w.hwnd,
                                                format!("{title} ({exe})"),
                                            )
                                            .changed()
                                        {
                                            let debug_rule: RuleDebug = serde_json::from_str(
                                                &komorebi_client::send_query(
                                                    &SocketMessage::DebugWindow(
                                                        self.debug_hwnd,
                                                    ),
                                                )
                                                .unwrap(),
                                            )
                                            .unwrap();

                                            self.debug_rule = Some(debug_rule)
                                        }
                                    }
                                });

                            if let Some(debug_rule) = &self.debug_rule {
                                json_view_ui(
                                    ui,
                                    &serde_json::to_string_pretty(debug_rule).unwrap(),
                                )
                            }
                        });

                        ui.collapsing("Regex Match", |ui| {
                            if ui
                                .text_edit_singleline(&mut self.regex_pattern)
                                .changed()
                            {
                                if self.regex_pattern.is_empty() {
                                    self.regex_compiled = None;
                                    self.regex_error = None;
                                } else {
                                    match Regex::new(&self.regex_pattern) {
                                        Ok(re) => {
                                            self.regex_compiled = Some(re);
                                            self.regex_error = None;
                                        }
                                        Err(e) => {
                                            self.regex_compiled = None;
                                            self.regex_error = Some(e.to_string());
                                        }
                                    }
                                }
                            }

                            if let Some(ref error) = self.regex_error {
                                ui.colored_label(Color32::RED, error);
                            }

                            if let Some(ref re) = self.regex_compiled {
                                ui.label(format!(
                                    "Windows matching /{}/:",
                                    self.regex_pattern
                                ));

                                egui::ScrollArea::vertical()
                                    .max_height(200.0)
                                    .show(ui, |ui| {
                                        for w in &self.debug_windows {
                                            let Ok(title) = w.title() else { continue };
                                            let Ok(exe) = w.exe() else { continue };
                                            let class = w.class().unwrap_or_default();
                                            let path = w.path().unwrap_or_default();

                                            let matches = re.is_match(&title)
                                                || re.is_match(&exe)
                                                || re.is_match(&class)
                                                || re.is_match(&path);

                                            if matches {
                                                ui.label(format!(
                                                    "{} | exe: {} | cls: {}",
                                                    title, exe, class
                                                ));
                                            }
                                        }
                                    });
                            } else if self.regex_pattern.is_empty() {
                                ui.label("Type a regex pattern to test against visible windows");
                            }
                        });
                    });
                ui.separator();

                ui.collapsing("Focus", |ui| {
                    if ui
                        .toggle_value(
                            &mut self.focus_config.mouse_follows_focus,
                            "Mouse Follows Focus",
                        )
                        .changed()
                    {
                        send_message(SocketMessage::MouseFollowsFocus(
                            self.focus_config.mouse_follows_focus,
                        ));
                    }

                    let ffm = &mut self.focus_config.focus_follows_mouse;
                    let mut enabled = ffm.is_some();
                    if ui
                        .toggle_value(&mut enabled, "Focus Follows Mouse")
                        .changed()
                    {
                        if enabled {
                            let impl_type =
                                ffm.unwrap_or(FocusFollowsMouseImplementation::Komorebi);
                            *ffm = Some(impl_type);
                            send_message(SocketMessage::FocusFollowsMouse(impl_type, true));
                        } else {
                            if let Some(impl_type) = ffm {
                                send_message(SocketMessage::FocusFollowsMouse(*impl_type, false));
                            }
                            *ffm = None;
                        }
                    }

                    if ffm.is_some() {
                        ui.indent("ffm_impl", |ui| {
                            for option in [
                                FocusFollowsMouseImplementation::Komorebi,
                                FocusFollowsMouseImplementation::Windows,
                            ] {
                                let current = ffm.unwrap();
                                if ui
                                    .add(egui::Button::selectable(current == option, option.to_string()))
                                    .clicked()
                                {
                                    *ffm = Some(option);
                                    send_message(SocketMessage::FocusFollowsMouse(option, true));
                                }
                            }
                        });
                    }

                    if ui
                        .toggle_value(
                            &mut self.focus_config.alt_focus_hack,
                            "Alt Focus Hack (Custom FFM)",
                        )
                        .changed()
                    {
                        send_message(SocketMessage::AltFocusHack(
                            self.focus_config.alt_focus_hack,
                        ));
                    }
                });
                ui.separator();

                ui.collapsing("Border", |ui| {
                    if ui
                        .toggle_value(&mut self.border_config.border_enabled, "Border")
                        .changed()
                    {
                        send_message(SocketMessage::Border(self.border_config.border_enabled));
                    }

                    ui.collapsing("Colours", |ui| {
                        let mut col = self.border_config.border_colours.single;
                        ui.collapsing("Single", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::BorderColour(
                                    WindowKind::Single,
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.border_config.border_colours.single = col;

                        let mut col = self.border_config.border_colours.stack;
                        ui.collapsing("Stack", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::BorderColour(
                                    WindowKind::Stack,
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.border_config.border_colours.stack = col;

                        let mut col = self.border_config.border_colours.monocle;
                        ui.collapsing("Monocle", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::BorderColour(
                                    WindowKind::Monocle,
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.border_config.border_colours.monocle = col;

                        let mut col = self.border_config.border_colours.floating;
                        ui.collapsing("Floating", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::BorderColour(
                                    WindowKind::Floating,
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.border_config.border_colours.floating = col;

                        let mut col = self.border_config.border_colours.unfocused;
                        ui.collapsing("Unfocused", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::BorderColour(
                                    WindowKind::Unfocused,
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.border_config.border_colours.unfocused = col;

                        let mut col = self.border_config.border_colours.unfocused_locked;
                        ui.collapsing("Unfocused Locked", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::BorderColour(
                                    WindowKind::UnfocusedLocked,
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.border_config.border_colours.unfocused_locked = col;
                    });

                    ui.collapsing("Style", |ui| {
                        let style = self.border_config.border_style;
                        for option in [
                            BorderStyle::System,
                            BorderStyle::Rounded,
                            BorderStyle::Square,
                        ] {
                            if ui
                                .add(egui::Button::selectable(style == option, option.to_string()))
                                .clicked()
                            {
                                self.border_config.border_style = option;
                                send_message(SocketMessage::BorderStyle(option));
                                std::thread::sleep(Duration::from_secs(1));
                                send_message(SocketMessage::Retile);
                            }
                        }
                    });

                    let mut width = self.border_config.border_width;
                    ui.collapsing("Width", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut width, -50..=50))
                            .drag_stopped()
                        {
                            send_message(SocketMessage::BorderWidth(width));
                        }
                    });
                    self.border_config.border_width = width;

                    let mut offset = self.border_config.border_offset;
                    ui.collapsing("Offset", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut offset, -50..=50))
                            .drag_stopped()
                        {
                            send_message(SocketMessage::BorderOffset(offset));
                        }
                    });
                    self.border_config.border_offset = offset;
                });
                ui.separator();

                ui.collapsing("Stackbar", |ui| {
                    let mode = self.stackbar_config.mode;
                    for option in [
                        StackbarMode::Never,
                        StackbarMode::OnStack,
                        StackbarMode::Always,
                    ] {
                        if ui
                            .add(egui::Button::selectable(mode == option, option.to_string()))
                            .clicked()
                        {
                            self.stackbar_config.mode = option;
                            send_message_retile(SocketMessage::StackbarMode(option));
                        }
                    }

                    ui.collapsing("Label", |ui| {
                        let label = self.stackbar_config.label;
                        for option in [StackbarLabel::Process, StackbarLabel::Title] {
                            if ui
                                .add(egui::Button::selectable(
                                    label == option,
                                    option.to_string(),
                                ))
                                .clicked()
                            {
                                self.stackbar_config.label = option;
                                send_message(SocketMessage::StackbarLabel(option));
                            }
                        }
                    });

                    ui.collapsing("Colours", |ui| {
                        let mut col = self.stackbar_config.focused_text_colour;
                        ui.collapsing("Focused Text", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::StackbarFocusedTextColour(
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.stackbar_config.focused_text_colour = col;

                        let mut col = self.stackbar_config.unfocused_text_colour;
                        ui.collapsing("Unfocused Text", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::StackbarUnfocusedTextColour(
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.stackbar_config.unfocused_text_colour = col;

                        let mut col = self.stackbar_config.background_colour;
                        ui.collapsing("Background", |ui| {
                            if egui::color_picker::color_picker_color32(ui, &mut col, Alpha::Opaque) {
                                send_message(SocketMessage::StackbarBackgroundColour(
                                    col.r() as u32,
                                    col.g() as u32,
                                    col.b() as u32,
                                ));
                            }
                        });
                        self.stackbar_config.background_colour = col;
                    });

                    let mut width = self.stackbar_config.width;
                    ui.collapsing("Width", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut width, 0..=500))
                            .drag_stopped()
                        {
                            send_message_retile(SocketMessage::StackbarTabWidth(width));
                        }
                    });
                    self.stackbar_config.width = width;

                    let mut height = self.stackbar_config.height;
                    ui.collapsing("Height", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut height, 0..=100))
                            .drag_stopped()
                        {
                            send_message_retile(SocketMessage::StackbarHeight(height));
                        }
                    });
                    self.stackbar_config.height = height;

                    let mut font_size = self.stackbar_config.font_size;
                    ui.collapsing("Font Size", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut font_size, 0..=100))
                            .drag_stopped()
                        {
                            send_message(SocketMessage::StackbarFontSize(font_size));
                        }
                    });
                    self.stackbar_config.font_size = font_size;
                });
                ui.separator();

                ui.collapsing("Animation", |ui| {
                    if ui
                        .toggle_value(&mut self.animation_config.enabled, "Animation Enabled")
                        .changed()
                    {
                        send_message(SocketMessage::Animation(
                            self.animation_config.enabled,
                            None,
                        ));
                    }

                    let mut duration = self.animation_config.duration;
                    ui.collapsing("Duration (ms)", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut duration, 0u64..=2000u64))
                            .drag_stopped()
                        {
                            send_message(SocketMessage::AnimationDuration(duration, None));
                        }
                    });
                    self.animation_config.duration = duration;

                    let mut fps = self.animation_config.fps;
                    ui.collapsing("FPS", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut fps, 1u64..=240u64))
                            .drag_stopped()
                        {
                            send_message(SocketMessage::AnimationFps(fps));
                        }
                    });
                    self.animation_config.fps = fps;

                    ui.collapsing("Style (send only)", |ui| {
                        ui.label("See docs for all easing options");
                    });
                });
                ui.separator();

                ui.collapsing("Transparency", |ui| {
                    if ui
                        .toggle_value(
                            &mut self.transparency_config.enabled,
                            "Transparency Enabled",
                        )
                        .changed()
                    {
                        send_message(SocketMessage::Transparency(
                            self.transparency_config.enabled,
                        ));
                    }

                    if ui
                        .toggle_value(
                            &mut self.transparency_config.floating,
                            "Floating Window Transparency",
                        )
                        .changed()
                    {
                        send_message(SocketMessage::TransparencyFloating(
                            self.transparency_config.floating,
                        ));
                    }

                    if ui
                        .toggle_value(
                            &mut self.transparency_config.monocle,
                            "Monocle Window Transparency",
                        )
                        .changed()
                    {
                        send_message(SocketMessage::TransparencyMonocle(
                            self.transparency_config.monocle,
                        ));
                    }

                    let mut alpha = self.transparency_config.alpha;
                    ui.collapsing("Alpha", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut alpha, 0u8..=255u8))
                            .drag_stopped()
                        {
                            send_message(SocketMessage::TransparencyAlpha(alpha));
                        }
                    });
                    self.transparency_config.alpha = alpha;
                });
                ui.separator();

                ui.collapsing("Invisible Borders", |ui| {
                    let mut left = self.invisible_borders.left;
                    ui.collapsing("Left", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut left, 0..=100))
                            .drag_stopped()
                        {
                            let mut ib = self.invisible_borders;
                            ib.left = left;
                            send_message(SocketMessage::InvisibleBorders(ib));
                        }
                    });
                    self.invisible_borders.left = left;

                    let mut top = self.invisible_borders.top;
                    ui.collapsing("Top", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut top, 0..=100))
                            .drag_stopped()
                        {
                            let mut ib = self.invisible_borders;
                            ib.top = top;
                            send_message(SocketMessage::InvisibleBorders(ib));
                        }
                    });
                    self.invisible_borders.top = top;

                    let mut right = self.invisible_borders.right;
                    ui.collapsing("Right", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut right, 0..=100))
                            .drag_stopped()
                        {
                            let mut ib = self.invisible_borders;
                            ib.right = right;
                            send_message(SocketMessage::InvisibleBorders(ib));
                        }
                    });
                    self.invisible_borders.right = right;

                    let mut bottom = self.invisible_borders.bottom;
                    ui.collapsing("Bottom", |ui| {
                        if ui
                            .add(egui::Slider::new(&mut bottom, 0..=100))
                            .drag_stopped()
                        {
                            let mut ib = self.invisible_borders;
                            ib.bottom = bottom;
                            send_message(SocketMessage::InvisibleBorders(ib));
                        }
                    });
                    self.invisible_borders.bottom = bottom;
                });
                ui.separator();

                let mut resize_delta = self.resize_delta;
                ui.collapsing("Resize Delta", |ui| {
                    if ui
                        .add(egui::Slider::new(&mut resize_delta, 0..=100))
                        .drag_stopped()
                    {
                        send_message(SocketMessage::ResizeDelta(resize_delta));
                    }
                });
                self.resize_delta = resize_delta;
                ui.separator();

                ui.collapsing("Window Operations", |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Toggle Float").clicked() {
                            send_message(SocketMessage::ToggleFloat);
                        }
                        if ui.button("Toggle Monocle").clicked() {
                            send_message(SocketMessage::ToggleMonocle);
                        }
                        if ui.button("Toggle Maximize").clicked() {
                            send_message(SocketMessage::ToggleMaximize);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            send_message(SocketMessage::Close);
                        }
                        if ui.button("Minimize").clicked() {
                            send_message(SocketMessage::Minimize);
                        }
                        if ui.button("Force Focus").clicked() {
                            send_message(SocketMessage::ForceFocus);
                        }
                    });
                });
                ui.separator();

                ui.collapsing("Workspace Management", |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("New Workspace").clicked() {
                            send_message(SocketMessage::NewWorkspace);
                        }
                        if ui.button("Close Workspace").clicked() {
                            send_message(SocketMessage::CloseWorkspace);
                        }
                        if ui.button("Focus Last").clicked() {
                            send_message(SocketMessage::FocusLastWorkspace);
                        }
                    });
                });
                ui.separator();

                for (monitor_idx, monitor) in self.monitors.iter_mut().enumerate() {
                    ui.collapsing(
                        format!(
                            "Monitor {monitor_idx} ({}x{})",
                            monitor.size.right, monitor.size.bottom
                        ),
                        |ui| {
                            ui.collapsing("Work Area Offset", |ui| {
                                if ui
                                    .add(
                                        egui::Slider::new(&mut monitor.work_area_offset.left, 0..=500)
                                            .text("Left"),
                                    )
                                    .drag_stopped()
                                {
                                    send_message(SocketMessage::MonitorWorkAreaOffset(
                                        monitor_idx,
                                        monitor.work_area_offset,
                                    ));
                                };

                                if ui
                                    .add(
                                        egui::Slider::new(&mut monitor.work_area_offset.top, 0..=500)
                                            .text("Top"),
                                    )
                                    .drag_stopped()
                                {
                                    send_message(SocketMessage::MonitorWorkAreaOffset(
                                        monitor_idx,
                                        monitor.work_area_offset,
                                    ));
                                };

                                if ui
                                    .add(
                                        egui::Slider::new(
                                            &mut monitor.work_area_offset.right,
                                            0..=500,
                                        )
                                        .text("Right"),
                                    )
                                    .drag_stopped()
                                {
                                    send_message(SocketMessage::MonitorWorkAreaOffset(
                                        monitor_idx,
                                        monitor.work_area_offset,
                                    ));
                                };

                                if ui
                                    .add(
                                        egui::Slider::new(
                                            &mut monitor.work_area_offset.bottom,
                                            0..=500,
                                        )
                                        .text("Bottom"),
                                    )
                                    .drag_stopped()
                                {
                                    send_message(SocketMessage::MonitorWorkAreaOffset(
                                        monitor_idx,
                                        monitor.work_area_offset,
                                    ));
                                };
                            });

                            ui.collapsing("Workspaces", |ui| {
                                for (workspace_idx, workspace) in
                                    monitor.workspaces.iter_mut().enumerate()
                                {
                                    ui.collapsing(
                                        format!(
                                            "Workspace {workspace_idx} ({})",
                                            workspace.name
                                        ),
                                        |ui| {
                                            if ui.button("Focus").clicked() {
                                                send_message(SocketMessage::MouseFollowsFocus(
                                                    false,
                                                ));
                                                send_message(
                                                    SocketMessage::FocusMonitorWorkspaceNumber(
                                                        monitor_idx,
                                                        workspace_idx,
                                                    ),
                                                );
                                                send_message(SocketMessage::MouseFollowsFocus(
                                                    self.focus_config.mouse_follows_focus,
                                                ));
                                            }

                                            if ui
                                                .toggle_value(&mut workspace.tile, "Tiling")
                                                .changed()
                                            {
                                                send_message(SocketMessage::WorkspaceTiling(
                                                    monitor_idx,
                                                    workspace_idx,
                                                    workspace.tile,
                                                ));
                                            }

                                            ui.collapsing("Name", |ui| {
                                                let monitor_workspaces = self
                                                    .workspace_names
                                                    .get_mut(&monitor_idx)
                                                    .unwrap();
                                                let workspace_name =
                                                    &mut monitor_workspaces[workspace_idx];
                                                if ui
                                                    .text_edit_singleline(workspace_name)
                                                    .lost_focus()
                                                {
                                                    workspace.name.clone_from(workspace_name);
                                                    send_message(SocketMessage::WorkspaceName(
                                                        monitor_idx,
                                                        workspace_idx,
                                                        workspace.name.clone(),
                                                    ));
                                                }
                                            });

                                            ui.collapsing("Layout", |ui| {
                                                for option in [
                                                    DefaultLayout::BSP,
                                                    DefaultLayout::Columns,
                                                    DefaultLayout::Rows,
                                                    DefaultLayout::VerticalStack,
                                                    DefaultLayout::HorizontalStack,
                                                    DefaultLayout::UltrawideVerticalStack,
                                                    DefaultLayout::Grid,
                                                ] {
                                                    if ui
                                                        .add(egui::Button::selectable(
                                                            workspace.layout == option,
                                                            option.to_string(),
                                                        ))
                                                        .clicked()
                                                    {
                                                        workspace.layout = option;
                                                        send_message(
                                                            SocketMessage::WorkspaceLayout(
                                                                monitor_idx,
                                                                workspace_idx,
                                                                workspace.layout,
                                                            ),
                                                        );
                                                    }
                                                }
                                            });

                                            let mut cp = workspace.container_padding;
                                            ui.collapsing("Container Padding", |ui| {
                                                if ui
                                                    .add(egui::Slider::new(&mut cp, 0..=100))
                                                    .drag_stopped()
                                                {
                                                    send_message(
                                                        SocketMessage::ContainerPadding(
                                                            monitor_idx,
                                                            workspace_idx,
                                                            cp,
                                                        ),
                                                    );
                                                }
                                            });
                                            workspace.container_padding = cp;

                                            let mut wp = workspace.workspace_padding;
                                            ui.collapsing("Workspace Padding", |ui| {
                                                if ui
                                                    .add(egui::Slider::new(&mut wp, 0..=100))
                                                    .drag_stopped()
                                                {
                                                    send_message(
                                                        SocketMessage::WorkspacePadding(
                                                            monitor_idx,
                                                            workspace_idx,
                                                            wp,
                                                        ),
                                                    );
                                                }
                                            });
                                            workspace.workspace_padding = wp;
                                        },
                                    );
                                }
                            });
                        },
                    );
                }
            });
        });
    }
}
