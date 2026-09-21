#![warn(clippy::all)]

pub mod animation;
pub mod apply_worker;
pub mod border_manager;
pub mod com;
#[macro_use]
pub mod ring;
pub mod container;
pub mod core;
pub mod focus_manager;
pub mod lockable_sequence;
pub mod monitor;
pub mod monitor_reconciliator;
pub mod process_command;
pub mod process_event;
pub mod process_movement;
pub mod reaper;
pub mod set_window_position;
pub mod splash;
pub mod stackbar_manager;
pub mod state;
pub mod static_config;
pub mod styles;
pub mod theme_manager;
pub mod transparency_manager;
pub mod window;
pub mod window_manager;
pub mod window_manager_event;
pub mod windows_api;
pub mod windows_callbacks;
pub mod winevent;
pub mod winevent_listener;
pub mod workspace;

use lazy_static::lazy_static;
use monitor_reconciliator::MonitorNotification;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub use core::*;
pub use komorebi_themes::colour::*;
pub use process_command::*;
pub use process_event::*;
pub use static_config::*;
pub use win32_display_data;
pub use window::*;
pub use window_manager::*;
pub use window_manager_event::*;
pub use windows_api::WindowsApi;
pub use windows_api::*;

use crate::core::config_generation::IdWithIdentifier;
use crate::core::config_generation::MatchingRule;
use crate::core::config_generation::MatchingStrategy;
use crate::core::config_generation::WorkspaceMatchingRule;
use color_eyre::eyre;
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_utils::atomic::AtomicCell;
use os_info::Version;
use parking_lot::Mutex;
use parking_lot::RwLock;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use state::State;
use uds_windows::UnixStream;
use which::which;
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

lazy_static! {
    static ref HIDDEN_HWNDS: Arc<Mutex<Vec<isize>>> = Arc::new(Mutex::new(vec![]));
    static ref LAYERED_WHITELIST: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Exe,
            id: String::from("steam.exe"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        }),
    ]));
    static ref TRAY_AND_MULTI_WINDOW_IDENTIFIERS: Arc<Mutex<Vec<MatchingRule>>> =
        Arc::new(Mutex::new(vec![
            MatchingRule::Simple(IdWithIdentifier {
                kind: ApplicationIdentifier::Exe,
                id: String::from("explorer.exe"),
                matching_strategy: Option::from(MatchingStrategy::Equals),
            }),
            MatchingRule::Simple(IdWithIdentifier {
                kind: ApplicationIdentifier::Exe,
                id: String::from("firefox.exe"),
                matching_strategy: Option::from(MatchingStrategy::Equals),
            }),
            MatchingRule::Simple(IdWithIdentifier {
                kind: ApplicationIdentifier::Exe,
                id: String::from("chrome.exe"),
                matching_strategy: Option::from(MatchingStrategy::Equals),
            }),
            MatchingRule::Simple(IdWithIdentifier {
                kind: ApplicationIdentifier::Exe,
                id: String::from("idea64.exe"),
                matching_strategy: Option::from(MatchingStrategy::Equals),
            }),
            MatchingRule::Simple(IdWithIdentifier {
                kind: ApplicationIdentifier::Exe,
                id: String::from("ApplicationFrameHost.exe"),
                matching_strategy: Option::from(MatchingStrategy::Equals),
            }),
            MatchingRule::Simple(IdWithIdentifier {
                kind: ApplicationIdentifier::Exe,
                id: String::from("steam.exe"),
                matching_strategy: Option::from(MatchingStrategy::Equals),
            })
        ]));
    static ref OBJECT_NAME_CHANGE_ON_LAUNCH: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Exe,
            id: String::from("firefox.exe"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        }),
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Exe,
            id: String::from("idea64.exe"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        }),
    ]));
    static ref OBJECT_NAME_CHANGE_TITLE_IGNORE_LIST: Arc<Mutex<Vec<Regex>>> = Arc::new(Mutex::new(Vec::new()));
    static ref TRANSPARENCY_BLACKLIST: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(Vec::new()));
    static ref MONITOR_INDEX_PREFERENCES: Arc<Mutex<HashMap<usize, Rect>>> =
        Arc::new(Mutex::new(HashMap::new()));
    static ref DISPLAY_INDEX_PREFERENCES: Arc<RwLock<HashMap<usize, String>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref WORKSPACE_MATCHING_RULES: Arc<Mutex<Vec<WorkspaceMatchingRule>>> =
        Arc::new(Mutex::new(Vec::new()));
    static ref REGEX_IDENTIFIERS: Arc<Mutex<HashMap<String, Regex>>> =
        Arc::new(Mutex::new(HashMap::new()));
    static ref MANAGE_IDENTIFIERS: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![]));
    static ref IGNORE_IDENTIFIERS: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![
        // mstsc.exe creates these on Windows 11 when a WSL process is launched
        // https://github.com/LGUG2Z/komorebi/issues/74
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Class,
            id: String::from("OPContainerClass"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        }),
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Class,
            id: String::from("IHWindowClass"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        }),
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Exe,
            id: String::from("komorebi-bar.exe"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        })
    ]));
    static ref SESSION_FLOATING_APPLICATIONS: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(Vec::new()));
    static ref FLOATING_APPLICATIONS: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Exe,
            id: String::from("komorebi-shortcuts.exe"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        })

    ]));
    static ref PINNED_FLOATING_APPLICATIONS: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(Vec::new()));
    static ref PERMAIGNORE_CLASSES: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![
        "Chrome_RenderWidgetHostHWND".to_string(),
    ]));
    static ref WSL2_UI_PROCESSES: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![
        "X410.exe".to_string(),
        "vcxsrv.exe".to_string(),
    ]));
    static ref SLOW_APPLICATION_IDENTIFIERS: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![
        MatchingRule::Simple(IdWithIdentifier {
            kind: ApplicationIdentifier::Exe,
            id: String::from("firefox.exe"),
            matching_strategy: Option::from(MatchingStrategy::Equals),
        }),
    ]));
    static ref DUPLICATE_MONITOR_SERIAL_IDS: Arc<RwLock<Vec<String>>> =
        Arc::new(RwLock::new(Vec::new()));
    static ref SUBSCRIPTION_PIPES: Arc<Mutex<HashMap<String, File>>> =
        Arc::new(Mutex::new(HashMap::new()));
    pub static ref SUBSCRIPTION_SOCKETS: Arc<Mutex<HashMap<String, PathBuf>>> =
        Arc::new(Mutex::new(HashMap::new()));
    pub static ref SUBSCRIPTION_SOCKET_OPTIONS: Arc<Mutex<HashMap<String, SubscribeOptions>>> =
        Arc::new(Mutex::new(HashMap::new()));
    static ref TCP_CONNECTIONS: Arc<Mutex<HashMap<String, TcpStream>>> =
        Arc::new(Mutex::new(HashMap::new()));
    static ref HIDING_BEHAVIOUR: Arc<Mutex<HidingBehaviour>> =
        Arc::new(Mutex::new(HidingBehaviour::Cloak));
    pub static ref HOME_DIR: PathBuf = {
        std::env::var("KOMOREBI_CONFIG_HOME").map_or_else(|_| dirs::home_dir().expect("there is no home directory"), |home_path| {
            let home = home_path.replace_env();

            assert!(
                home.is_dir(),
                "$Env:KOMOREBI_CONFIG_HOME is set to '{home_path}', which is not a valid directory"
            );


            home
        })
    };
    pub static ref DATA_DIR: PathBuf = dirs::data_local_dir().expect("there is no local data directory").join("komorebi");
    pub static ref AHK_EXE: String = {
        let mut ahk: String = String::from("autohotkey.exe");

        if let Ok(komorebi_ahk_exe) = std::env::var("KOMOREBI_AHK_EXE")
            && which(&komorebi_ahk_exe).is_ok() {
                ahk = komorebi_ahk_exe;
            }

        ahk
    };
    static ref WINDOWS_11: bool = {
        matches!(
            os_info::get().version(),
            Version::Semantic(_, _, x) if x >= &22000
        )
    };

    // Use app-specific titlebar removal options where possible
    // eg. Windows Terminal, IntelliJ IDEA, Firefox
    static ref NO_TITLEBAR: Arc<Mutex<Vec<MatchingRule>>> = Arc::new(Mutex::new(vec![]));

    static ref WINDOWS_BY_BAR_HWNDS: Arc<Mutex<HashMap<isize, VecDeque<isize>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    static ref FLOATING_WINDOW_TOGGLE_ASPECT_RATIO: Arc<Mutex<AspectRatio>> = Arc::new(Mutex::new(AspectRatio::Predefined(PredefinedAspectRatio::Widescreen)));

    static ref CURRENT_VIRTUAL_DESKTOP: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));

    pub static ref LAYOUT_DEFAULTS: Arc<Mutex<HashMap<DefaultLayout, LayoutDefaultEntry>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

pub static DEFAULT_WORKSPACE_PADDING: AtomicI32 = AtomicI32::new(10);
pub static DEFAULT_CONTAINER_PADDING: AtomicI32 = AtomicI32::new(10);
pub static DEFAULT_RESIZE_DELTA: i32 = 50;

pub static DEFAULT_MOUSE_FOLLOWS_FOCUS: bool = true;
pub static DEFAULT_FOCUS_NEW_WINDOWS: bool = false;
pub static DEFAULT_CYCLE_FOCUS_ACROSS_MONITORS: bool = false;
pub static DEFAULT_KEEP_MONOCLE_ON_WINDOW_CLOSE: bool = true;
pub static INITIAL_CONFIGURATION_LOADED: AtomicBool = AtomicBool::new(false);
pub static CUSTOM_FFM: AtomicBool = AtomicBool::new(false);
pub static SESSION_ID: AtomicU32 = AtomicU32::new(0);

pub static REMOVE_TITLEBARS: AtomicBool = AtomicBool::new(false);

pub static LOWER_IGNORED_WINDOWS_ON_FOCUS: AtomicBool = AtomicBool::new(false);

pub static HIDE_PINNED_ON_EMPTY_WORKSPACES: AtomicBool = AtomicBool::new(false);

pub static SLOW_APPLICATION_COMPENSATION_TIME: AtomicU64 = AtomicU64::new(20);

pub static WINDOW_HANDLING_BEHAVIOUR: AtomicCell<WindowHandlingBehaviour> =
    AtomicCell::new(WindowHandlingBehaviour::Sync);

shadow_rs::shadow!(build);

pub const PUBLIC_KEY: [u8; 32] = [
    0x5a, 0x69, 0x4a, 0xe1, 0x3c, 0x4b, 0xc8, 0x4e, 0xc3, 0x79, 0x0f, 0xab, 0x27, 0x6b, 0x7e, 0xdd,
    0x6b, 0x39, 0x6f, 0xa2, 0xc3, 0x9f, 0x3d, 0x48, 0xf2, 0x72, 0x56, 0x41, 0x1b, 0xc8, 0x08, 0xdb,
];

#[derive(Default, Debug, Clone, PartialEq, Deserialize)]
pub struct License {
    #[serde(rename = "hasValidSubscription")]
    pub has_valid_subscription: bool,
    pub timestamp: i64,
    #[serde(rename = "currentEndPeriod")]
    pub current_end_period: Option<i64>,
    pub signature: String,
}

/// A trait for types that can be marked as locked or unlocked.
pub trait Lockable {
    /// Returns `true` if the item is locked.
    fn locked(&self) -> bool;
    /// Sets the locked state of the item.
    fn set_locked(&mut self, locked: bool) -> &mut Self;
}

#[must_use]
pub fn current_virtual_desktop() -> Option<Vec<u8>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // This is the path on Windows 10
    let mut current = hkcu
        .open_subkey(format!(
            r#"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SessionInfo\{}\VirtualDesktops"#,
            SESSION_ID.load(Ordering::SeqCst)
        ))
        .ok()
        .and_then(
            |desktops| match desktops.get_raw_value("CurrentVirtualDesktop") {
                Ok(current) => Option::from(current.bytes),
                Err(_) => None,
            },
        );

    // This is the path on Windows 11
    if current.is_none() {
        current = hkcu
            .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops")
            .ok()
            .and_then(
                |desktops| match desktops.get_raw_value("CurrentVirtualDesktop") {
                    Ok(current) => Option::from(current.bytes),
                    Err(_) => None,
                },
            );
    }

    // For Win10 users that do not use virtual desktops, the CurrentVirtualDesktop value will not
    // exist until one has been created in the task view

    // The registry value will also not exist on user login if virtual desktops have been created
    // but the task view has not been initiated

    // In both of these cases, we return None, and the virtual desktop validation will never run. In
    // the latter case, if the user desires this validation after initiating the task view, komorebi
    // should be restarted, and then when this // fn runs again for the first time, it will pick up
    // the value of CurrentVirtualDesktop and validate against it accordingly
    current.map(|current| current.to_vec())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum NotificationEvent {
    WindowManager(WindowManagerEvent),
    Socket(SocketMessage),
    Monitor(MonitorNotification),
    VirtualDesktop(VirtualDesktopNotification),
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum VirtualDesktopNotification {
    EnteredAssociatedVirtualDesktop,
    LeftAssociatedVirtualDesktop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Notification {
    pub event: NotificationEvent,
    pub state: State,
}

/// Returns `true` if at least one subscriber (socket or pipe) is registered.
pub fn has_subscribers() -> bool {
    !SUBSCRIPTION_SOCKETS.lock().is_empty() || !SUBSCRIPTION_PIPES.lock().is_empty()
}

/// Maximum number of notifications buffered for delivery before new ones are
/// dropped under extreme overload. Bounding the queue (every entry carries a
/// full `State` payload) keeps memory usage bounded and guarantees a
/// wedged/slow subscriber can never stall the window manager event loop.
const NOTIFY_CHANNEL_CAPACITY: usize = 256;

/// Longest time the publisher will wait to write to a single subscriber socket
/// before treating that subscriber as stale and pruning it.
const NOTIFY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A fully-formed notification destined for (at least one) subscriber,
/// serialized and delivered on a dedicated thread rather than on whichever
/// thread just mutated the window manager.
struct NotifyEnvelope {
    notification: Notification,
    state_has_been_modified: bool,
    is_override: bool,
}

struct NotifyPipeline {
    tx: Sender<NotifyEnvelope>,
    /// Holds the newest state-change notification still waiting to be
    /// delivered when the only subscribers opted into `filter_state_changes`.
    /// Such subscribers ignore the per-event payload and only react to "the
    /// state changed", so latest-wins collapses a burst of state dumps into a
    /// single delivery instead of queuing every single one of them.
    latest_state: Arc<Mutex<Option<NotifyEnvelope>>>,
}

static NOTIFY_PIPELINE: LazyLock<NotifyPipeline> = LazyLock::new(|| {
    let (tx, rx) = crossbeam_channel::bounded(NOTIFY_CHANNEL_CAPACITY);
    let latest_state = Arc::new(Mutex::new(None::<NotifyEnvelope>));

    std::thread::Builder::new()
        .name("komorebi-notifier".to_string())
        .spawn({
            let latest_state = Arc::clone(&latest_state);
            move || publisher_loop(rx, latest_state)
        })
        .expect("failed to spawn the subscriber notification publisher thread");

    NotifyPipeline { tx, latest_state }
});

/// Consumes notifications off the queue and delivers them to subscribers. All
/// serialization and socket I/O happens on this thread, so producers (the
/// window manager event/command threads) never block on a subscriber. When the
/// queue runs dry it still polls the latest-state cell once in a while so a
/// coalesced notification is delivered even if no further event follows it.
fn publisher_loop(rx: Receiver<NotifyEnvelope>, latest_state: Arc<Mutex<Option<NotifyEnvelope>>>) {
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(envelope) => deliver_notification(envelope),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(envelope) = latest_state.lock().take() {
            deliver_notification(envelope);
        }
    }
}

/// Serializes `envelope` and writes it to all subscribing sockets and pipes.
/// The subscriber lists are only briefly locked to snapshot them; the actual
/// connect/write I/O happens without holding the subscription locks, so a
/// wedged subscriber can only stall this thread, never a producer.
fn deliver_notification(envelope: NotifyEnvelope) {
    let NotifyEnvelope {
        notification,
        state_has_been_modified,
        is_override,
    } = envelope;

    let notification = match serde_json::to_string(&notification) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!("could not serialise notification for subscribers: {error}");
            return;
        }
    };

    let sockets = {
        let sockets = SUBSCRIPTION_SOCKETS.lock();
        let options = SUBSCRIPTION_SOCKET_OPTIONS.lock();
        sockets
            .iter()
            .map(|(socket, path)| {
                (
                    socket.clone(),
                    path.clone(),
                    options.get(socket).copied().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
    };

    let mut stale_sockets = vec![];

    for (socket, path, subscribe_options) in &sockets {
        let apply_state_filter = subscribe_options.filter_state_changes;

        if !apply_state_filter || state_has_been_modified || is_override {
            match UnixStream::connect(path) {
                Ok(mut stream) => {
                    let _ = stream.set_write_timeout(Some(NOTIFY_WRITE_TIMEOUT));
                    if stream.write_all(notification.as_bytes()).is_ok() {
                        tracing::debug!("pushed notification to subscriber: {socket}");
                    } else {
                        tracing::debug!("failed to push notification to subscriber: {socket}");
                        stale_sockets.push(socket.clone());
                    }
                }
                Err(_) => {
                    stale_sockets.push(socket.clone());
                }
            }
        }
    }

    if !stale_sockets.is_empty() {
        let mut active = SUBSCRIPTION_SOCKETS.lock();
        for socket in &stale_sockets {
            tracing::warn!("removing stale subscription: {socket}");
            active.remove(socket);
            let socket_path = DATA_DIR.join(socket);
            if let Err(error) = std::fs::remove_file(&socket_path) {
                tracing::error!(
                    "could not remove stale subscriber socket file at {}: {error}",
                    socket_path.display()
                )
            }
        }
    }

    let mut stale_pipes = vec![];
    let mut pipes = SUBSCRIPTION_PIPES.lock();
    for (subscriber, pipe) in &mut *pipes {
        match writeln!(pipe, "{notification}") {
            Ok(()) => {
                tracing::debug!("pushed notification to subscriber: {subscriber}");
            }
            Err(error) => {
                // ERROR_FILE_NOT_FOUND
                // 2 (0x2)
                // The system cannot find the file specified.

                // ERROR_NO_DATA
                // 232 (0xE8)
                // The pipe is being closed.

                // Remove the subscription; the process will have to subscribe again
                if let Some(2 | 232) = error.raw_os_error() {
                    stale_pipes.push(subscriber.clone());
                }
            }
        }
    }

    for subscriber in stale_pipes {
        tracing::warn!("removing stale subscription: {}", subscriber);
        pipes.remove(&subscriber);
    }
}

pub fn notify_subscribers(
    notification: Notification,
    state_has_been_modified: bool,
) -> eyre::Result<()> {
    let is_override_event = matches!(
        notification.event,
        NotificationEvent::Socket(SocketMessage::AddSubscriberSocket(_))
            | NotificationEvent::Socket(SocketMessage::AddSubscriberSocketWithOptions(_, _))
            | NotificationEvent::Socket(SocketMessage::Theme(_))
            | NotificationEvent::Socket(SocketMessage::ReloadStaticConfiguration(_))
            | NotificationEvent::WindowManager(WindowManagerEvent::TitleUpdate(_, _))
            | NotificationEvent::WindowManager(WindowManagerEvent::Show(_, _))
            | NotificationEvent::WindowManager(WindowManagerEvent::Uncloak(_, _))
    );

    // Fast path: nobody is listening. This also lets the callers skip building
    // the payload `State` entirely.
    if SUBSCRIPTION_SOCKETS.lock().is_empty() && SUBSCRIPTION_PIPES.lock().is_empty() {
        return Ok(());
    }

    let sockets = SUBSCRIPTION_SOCKETS.lock();
    let all_filter_state_changes = !sockets.is_empty() && SUBSCRIPTION_PIPES.lock().is_empty() && {
        let options = SUBSCRIPTION_SOCKET_OPTIONS.lock();
        sockets.keys().all(|socket| {
            options
                .get(socket)
                .copied()
                .unwrap_or_default()
                .filter_state_changes
        })
    };

    if !state_has_been_modified && !is_override_event && all_filter_state_changes {
        // Every subscriber opted into the state-change filter and the state
        // has not changed; nothing would be delivered, so skip entirely.
        return Ok(());
    }

    let pipeline = &*NOTIFY_PIPELINE;

    if is_override_event {
        // An override is always delivered immediately with a fresh full state,
        // so any coalesced snapshot still waiting is older and must not be
        // delivered after it.
        *pipeline.latest_state.lock() = None;
    }

    if !is_override_event && state_has_been_modified && all_filter_state_changes {
        // Coalesce: keep only the newest state dump waiting for the filter,
        // replacing the previously pending one instead of growing the queue.
        *pipeline.latest_state.lock() = Some(NotifyEnvelope {
            notification,
            state_has_been_modified,
            is_override: false,
        });
        return Ok(());
    }

    let envelope = NotifyEnvelope {
        notification,
        state_has_been_modified,
        is_override: is_override_event,
    };

    if let Err(error) = pipeline.tx.try_send(envelope) {
        // The bounded queue is full and a subscriber is not draining it fast
        // enough. Drop the notification instead of blocking the event loop;
        // once the slow/stale subscriber is pruned the queue will drain again.
        tracing::warn!("notification dropped, subscriber notify queue full: {error}");
    }

    Ok(())
}

/// Writes the list of managed window HWNDs (consumed by e.g. `masir`) to a temp
/// file and renames it into place, so readers of `komorebi.hwnd.json` never
/// observe a truncated or empty file mid-write.
pub(crate) fn write_known_hwnds(hwnds: &[isize]) -> eyre::Result<()> {
    let hwnd_json = DATA_DIR.join("komorebi.hwnd.json");
    let tmp_json = DATA_DIR.join("komorebi.hwnd.json.tmp");

    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&tmp_json)?;
        serde_json::to_writer_pretty(&file, hwnds)?;
        file.sync_all().ok();
    }

    std::fs::rename(&tmp_json, &hwnd_json)?;

    Ok(())
}

pub fn load_configuration() -> eyre::Result<()> {
    let config_pwsh = HOME_DIR.join("komorebi.ps1");
    let config_ahk = HOME_DIR.join("komorebi.ahk");

    if config_pwsh.exists() {
        let powershell_exe = if which("pwsh.exe").is_ok() {
            "pwsh.exe"
        } else {
            "powershell.exe"
        };

        tracing::info!("loading configuration file: {}", config_pwsh.display());

        Command::new(powershell_exe)
            .arg(config_pwsh.as_os_str())
            .output()?;
    } else if config_ahk.exists() && which(&*AHK_EXE).is_ok() {
        tracing::info!("loading configuration file: {}", config_ahk.display());

        Command::new(&*AHK_EXE)
            .arg(config_ahk.as_os_str())
            .output()?;
    }

    Ok(())
}
