#![deny(clippy::unwrap_used, clippy::expect_used)]

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_utils::atomic::AtomicConsume;
use parking_lot::Mutex;
use regex::Regex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::time::Duration;
use std::time::Instant;

use crate::REGEX_IDENTIFIERS;
use crate::TRANSPARENCY_BLACKLIST;
use crate::Window;
use crate::WindowManager;
use crate::WindowsApi;
use crate::core::config_generation::MatchingRule;
use crate::should_act;
use crate::workspace::WorkspaceLayer;

pub static TRANSPARENCY_ENABLED: AtomicBool = AtomicBool::new(false);
pub static TRANSPARENCY_ALPHA: AtomicU8 = AtomicU8::new(200);
pub static TRANSPARENCY_MONOCLE: AtomicBool = AtomicBool::new(false);
pub static TRANSPARENCY_FLOATING: AtomicBool = AtomicBool::new(false);

static KNOWN_HWNDS: OnceLock<Mutex<Vec<isize>>> = OnceLock::new();

pub struct Notification;

static CHANNEL: OnceLock<(Sender<Notification>, Receiver<Notification>)> = OnceLock::new();

pub fn known_hwnds() -> Vec<isize> {
    let known = KNOWN_HWNDS.get_or_init(|| Mutex::new(Vec::new())).lock();
    known.iter().copied().collect()
}

pub fn channel() -> &'static (Sender<Notification>, Receiver<Notification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(20))
}

fn event_tx() -> Sender<Notification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<Notification> {
    channel().1.clone()
}

pub fn send_notification() {
    if event_tx().try_send(Notification).is_err() {
        tracing::warn!("channel is full; dropping notification")
    }
}

fn is_transparency_blacklisted(
    window: &Window,
    transparency_blacklist: &[MatchingRule],
    regex_identifiers: &HashMap<String, Regex>,
) -> bool {
    if transparency_blacklist.is_empty() {
        return false;
    }

    if let (Ok(title), Ok(exe_name), Ok(class), Ok(path)) = (
        window.title(),
        window.exe(),
        window.class(),
        window.path(),
    ) {
        should_act(
            &title,
            &exe_name,
            &class,
            &path,
            transparency_blacklist,
            regex_identifiers,
        )
        .is_some()
    } else {
        false
    }
}

pub fn listen_for_notifications(wm: Arc<Mutex<WindowManager>>) {
    std::thread::spawn(move || {
        loop {
            match handle_notifications(wm.clone()) {
                Ok(()) => {
                    tracing::warn!("restarting finished thread");
                }
                Err(error) => {
                    tracing::warn!("restarting failed thread: {}", error);
                }
            }
        }
    });
}

pub fn handle_notifications(wm: Arc<Mutex<WindowManager>>) -> color_eyre::Result<()> {
    tracing::info!("listening");

    let receiver = event_rx();
    event_tx().send(Notification)?;

    // Minimum time between passes; short enough to feel responsive, long enough to absorb rapid
    // focus changes into a single repaint of the final state.
    const SETTLE_DURATION: Duration = Duration::from_millis(50);
    let mut last_pass = Instant::now();

    'receiver: for _ in &receiver {
        // Coalesce notifications that accumulated while the previous pass was running; a single
        // follow-up pass immediately after is enough to pick up any state change.
        while receiver.try_recv().is_ok() {}

        // Settle: if a pass just ran, wait out the remainder of the settle window so a rapid focus
        // chase (clicking/alt-tabbing across containers) paints the final state instead of flipping
        // windows transparent/opaque on every intermediate focus event.
        let elapsed = last_pass.elapsed();
        if elapsed < SETTLE_DURATION {
            std::thread::sleep(SETTLE_DURATION - elapsed);
        }
        last_pass = Instant::now();

        let known_hwnds = KNOWN_HWNDS.get_or_init(|| Mutex::new(Vec::new()));
        if !TRANSPARENCY_ENABLED.load_consume() {
            for hwnd in known_hwnds.lock().iter() {
                if let Err(error) = Window::from(*hwnd).opaque() {
                    tracing::error!("failed to make window {hwnd} opaque: {error}")
                }
            }

            continue 'receiver;
        }

        known_hwnds.lock().clear();

        // Decide phase: compute which windows need their transparency state changed. The
        // WindowManager lock is held only while reading state, never while making the blocking
        // layout WinAPI calls in the apply phase below.
        let (transparent_targets, opaque_targets) = {
            let state = wm.lock();
            decide_targets(&state, known_hwnds)
        };

        // Apply phase: the WM lock is released here. SetWindowLongPtrW / SetLayeredWindowAttributes
        // marshal synchronously to the target window's thread and can block indefinitely on a
        // suspended or hung renderer, so they must never run while the WM event loop is blocked.
        for hwnd in opaque_targets {
            if !WindowsApi::is_window(hwnd) {
                continue;
            }

            if let Err(error) = Window::from(hwnd).opaque() {
                tracing::error!("failed to make window {hwnd} opaque: {error}")
            }
        }

        for hwnd in transparent_targets {
            if !WindowsApi::is_window(hwnd) {
                continue;
            }

            let window = Window::from(hwnd);

            // Minimized / hidden windows belong to threads that may be suspending their renderer
            // (e.g. Chromium occlusion tracking); applying layering to them is pointless and the
            // marshalled SetWindowLongPtrW call can block indefinitely.
            if window.is_minimized() || !window.is_visible() {
                continue;
            }

            match window.transparent() {
                Err(error) => {
                    tracing::error!("failed to make unfocused window {hwnd} transparent: {error}")
                }
                Ok(..) => {
                    known_hwnds.lock().push(hwnd);
                }
            }
        }
    }

    Ok(())
}

/// Decides which windows should be transparent and which should be opaque based on the current
/// WM state. Returns `(transparent_targets, opaque_targets)` as hwnd lists; no WinAPI calls are
/// made here so the WM lock is only ever briefly held while reading state.
fn decide_targets(
    state: &WindowManager,
    known_hwnds: &Mutex<Vec<isize>>,
) -> (Vec<isize>, Vec<isize>) {
    let mut transparent_targets = Vec::new();
    let mut opaque_targets = Vec::new();

    let focused_monitor_idx = state.focused_monitor_idx();

    'monitors: for (monitor_idx, m) in state.monitors.elements().iter().enumerate() {
        let focused_workspace_idx = m.focused_workspace_idx();

        'workspaces: for (workspace_idx, ws) in m.workspaces().iter().enumerate() {
            // Only operate on the focused workspace of each monitor
            // Workspaces with tiling disabled don't have transparent windows
            if !ws.tile || workspace_idx != focused_workspace_idx {
                for window in ws.visible_windows().iter().flatten() {
                    opaque_targets.push(window.hwnd);
                }

                continue 'workspaces;
            }

            let transparency_blacklist = TRANSPARENCY_BLACKLIST.lock();
            let regex_identifiers = REGEX_IDENTIFIERS.lock();

            // The monocle container is never transparent unless the toggle is enabled and its
            // monitor isn't focused: a monocle workspace is a fullscreen view of a single window,
            // so it is only dimmed when the user is looking at another monitor.
            if let Some(monocle) = &ws.monocle_container {
                if let Some(window) = monocle.focused_window() {
                    let transparent = TRANSPARENCY_MONOCLE.load_consume()
                        && monitor_idx != focused_monitor_idx
                        && !is_transparency_blacklisted(
                            window,
                            &transparency_blacklist,
                            &regex_identifiers,
                        );

                    if transparent {
                        transparent_targets.push(window.hwnd);
                    } else {
                        opaque_targets.push(window.hwnd);
                    }
                }

                continue 'monitors;
            }

            let foreground_hwnd = WindowsApi::foreground_window().unwrap_or_default();
            let is_maximized = WindowsApi::is_zoomed(foreground_hwnd);

            if is_maximized {
                opaque_targets.push(foreground_hwnd);

                continue 'monitors;
            }

            for (idx, c) in ws.containers().iter().enumerate() {
                // Update the transparency for all containers on this workspace

                // If the window is not focused on the current workspace, or isn't on the focused monitor
                // make it transparent. On a Floating workspace every tiled window is an unfocused
                // background under the raised floating overlay, so none of them is treated as focused.
                #[allow(clippy::collapsible_else_if)]
                if idx != ws.focused_container_idx()
                    || monitor_idx != focused_monitor_idx
                    || ws.layer == WorkspaceLayer::Floating
                {
                    let focused_window_idx = c.focused_window_idx();
                    for (window_idx, window) in c.windows().iter().enumerate() {
                        if window_idx == focused_window_idx {
                            // Never paint the OS foreground window transparent: the container that
                            // WM state considers 'focused' can lag behind the real foreground while
                            // focus events are reconciled, so making the foreground window of an
                            // unreconciled container transparent would flip the window the user is
                            // interacting with to alpha < 255 - a flicker storm while clicking around.
                            let opaque = is_transparency_blacklisted(
                                window,
                                &transparency_blacklist,
                                &regex_identifiers,
                            ) || window.hwnd == foreground_hwnd;

                            if opaque {
                                opaque_targets.push(window.hwnd);
                            } else {
                                transparent_targets.push(window.hwnd);
                            }
                        } else {
                            // just in case, this is useful when people are clicking around
                            // on unfocused stackbar tabs
                            known_hwnds.lock().push(window.hwnd);
                        }
                    }
                // Otherwise, make it opaque
                } else {
                    let focused_window_idx = c.focused_window_idx();
                    for (window_idx, window) in c.windows().iter().enumerate() {
                        if window_idx != focused_window_idx {
                            known_hwnds.lock().push(window.hwnd);
                        } else {
                            opaque_targets.push(window.hwnd);
                        }
                    }
                };
            }

            // Unfocused floating windows are transparent when the toggle is enabled. The focused
            // floating window stays opaque (as does the OS foreground window, see above).
            if TRANSPARENCY_FLOATING.load_consume() {
                let focused_floating_idx = ws.focused_floating_window_idx();

                for (window_idx, window) in ws.floating_windows().iter().enumerate() {
                    let opaque = window_idx == focused_floating_idx
                        || window.hwnd == foreground_hwnd
                        || is_transparency_blacklisted(
                            window,
                            &transparency_blacklist,
                            &regex_identifiers,
                        );

                    if opaque {
                        opaque_targets.push(window.hwnd);
                    } else {
                        transparent_targets.push(window.hwnd);
                    }
                }
            }
        }
    }

    // Deduplicate opaque targets (the maximized foreground window can be pushed once per monitor)
    // and let opaque win over transparent so a window is never flipped both ways within a pass.
    let mut unique_opaque = Vec::with_capacity(opaque_targets.len());
    let mut seen_opaque = HashSet::new();
    for hwnd in opaque_targets {
        if seen_opaque.insert(hwnd) {
            unique_opaque.push(hwnd);
        }
    }

    transparent_targets.retain(|hwnd| !seen_opaque.contains(hwnd));

    (transparent_targets, unique_opaque)
}
