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

// KNOWN_HWNDS is a persistent set of the hwnds komorebi currently keeps transparent, so that
// every window it dimmed - including windows on non-focused (hidden) workspaces - can be
// restored to opaque when transparency is toggled off or the WM exits.
static KNOWN_HWNDS: OnceLock<Mutex<Vec<isize>>> = OnceLock::new();

pub struct Notification;

static CHANNEL: OnceLock<(Sender<Notification>, Receiver<Notification>)> = OnceLock::new();

pub fn known_hwnds() -> Vec<isize> {
    let known = KNOWN_HWNDS.get_or_init(|| Mutex::new(Vec::new())).lock();
    known.iter().copied().collect()
}

/// Record that `hwnd` is transparent so it can be restored on disable; idempotent.
fn track_transparent_hwnd(known: &mut Vec<isize>, hwnd: isize) {
    if !known.contains(&hwnd) {
        known.push(hwnd);
    }
}

/// Drop `hwnd` from the restore set once it has been made opaque.
fn untrack_opaque_hwnd(known: &mut Vec<isize>, hwnd: isize) {
    known.retain(|&h| h != hwnd);
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

    if let (Ok(title), Ok(exe_name), Ok(class), Ok(path)) =
        (window.title(), window.exe(), window.class(), window.path())
    {
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

        let known_hwnds = KNOWN_HWNDS.get_or_init(|| Mutex::new(Vec::new()));

        // Handle the disabled state before the settle sleep: when transparency is off there is
        // nothing to coalesce, so restoring any leftover dimmed windows and returning straight
        // away avoids waking up and sleeping on every event. A restore is also not the kind of
        // visual flip-flopping the settle window exists to coalesce.
        // Transparency stays active even when the global toggle is off if any monitor or
        // workspace has a runtime override explicitly enabling it (the focused-scope toggles
        // work independently of the global switch). The monitor/workspace state is read under
        // the lock for this brief scan only; the restore path below never holds the WM lock.
        let transparency_active = TRANSPARENCY_ENABLED.load_consume() || {
            let state = wm.lock();
            state.monitors.elements().iter().any(|m| {
                m.transparency == Some(true)
                    || m.workspaces()
                        .iter()
                        .any(|ws| ws.transparency == Some(true))
            })
        };
        if !transparency_active {
            // Restore every window komorebi made transparent, including windows on non-focused
            // workspaces that were dimmed while their workspace was focused: KNOWN_HWNDS persists
            // across passes, so this is the full set rather than last pass's focused-workspace set.
            let hwnds = known_hwnds.lock().clone();
            for hwnd in hwnds {
                if !WindowsApi::is_window(hwnd) {
                    continue;
                }

                if let Err(error) = Window::from(hwnd).opaque() {
                    tracing::error!("failed to make window {hwnd} opaque: {error}")
                }
            }

            known_hwnds.lock().clear();

            continue 'receiver;
        }

        // Coalesce a monitor/workspace switch burst into a single final pass. During a switch the
        // OS foreground is transient and bounces across the workspaces being shown/hidden, so every
        // intermediate decide+apply repaints against a transient foreground: windows that will end
        // dimmed are restored opaque by a middle pass and then dimmed only when the foreground
        // finally settles, which is the all-windows-opaque flash on every switch (even a single one).
        // While any monitor is inside its (bounded, self-clearing) switch-stabilization window, keep
        // draining and wait out the rest of that window, then let exactly one decide+apply run
        // against the settled foreground.
        let mut switch_settling = { wm.lock().any_monitor_switch_settling() };
        if switch_settling {
            // Bounded wait for the stabilization grace window to clear. The flag always clears
            // (grace is time-based, 400 ms), so this loop terminates; the deadline only guards
            // against a pathological event storm that keeps re-arming the window.
            const SWITCH_COALESCE_DEADLINE_MS: u64 = 600;
            let deadline = Instant::now() + Duration::from_millis(SWITCH_COALESCE_DEADLINE_MS);

            while switch_settling && Instant::now() < deadline {
                std::thread::sleep(SETTLE_DURATION);
                switch_settling = wm.lock().any_monitor_switch_settling();
            }

            last_pass = Instant::now();
        } else {
            // Settle: if a pass just ran, wait out the remainder of the settle window so a rapid focus
            // chase (clicking/alt-tabbing across containers) paints the final state instead of flipping
            // windows transparent/opaque on every intermediate focus event.
            let elapsed = last_pass.elapsed();
            if elapsed < SETTLE_DURATION {
                std::thread::sleep(SETTLE_DURATION - elapsed);
            }
            last_pass = Instant::now();
        }

        // Decide phase: compute which windows need their transparency state changed. The
        // WindowManager lock is held only while reading state; the OS foreground window and
        // maximized state are read before locking, and the blocking layout WinAPI calls are
        // made only in the apply phase below.
        let foreground_hwnd = WindowsApi::foreground_window().unwrap_or_default();
        let is_maximized = WindowsApi::is_zoomed(foreground_hwnd);

        let (transparent_targets, opaque_targets) = {
            let state = wm.lock();
            decide_targets(&state, known_hwnds, foreground_hwnd, is_maximized)
        };

        tracing::debug!(
            transparency_enabled = TRANSPARENCY_ENABLED.load_consume(),
            transparency_floating = TRANSPARENCY_FLOATING.load_consume(),
            transparency_monocle = TRANSPARENCY_MONOCLE.load_consume(),
            ?transparent_targets,
            ?opaque_targets,
            "decided window transparency targets",
        );

        // Apply phase: the WM lock is released here. SetWindowLongPtrW / SetLayeredWindowAttributes
        // marshal synchronously to the target window's thread and can block indefinitely on a
        // suspended or hung renderer, so they must never run while the WM event loop is blocked.
        for hwnd in opaque_targets {
            if !WindowsApi::is_window(hwnd) {
                continue;
            }

            // Probe the window's thread before the marshalled SetWindowLongPtrW so one Not
            // Responding renderer can't stall the rest of the pass; a hung window simply keeps
            // its current opacity and will be retried on a later pass.
            if !WindowsApi::is_window_thread_responding(
                hwnd,
                WindowsApi::WINDOW_THREAD_RESPONSE_TIMEOUT_MS,
            ) {
                tracing::debug!(hwnd, "skipping opacity update for unresponsive window");
                continue;
            }

            if let Err(error) = Window::from(hwnd).opaque() {
                tracing::error!("failed to make window {hwnd} opaque: {error}")
            } else {
                untrack_opaque_hwnd(&mut known_hwnds.lock(), hwnd);
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

            // Same responsiveness probe as the opaque loop: a hung thread would otherwise block
            // the marshalled SetWindowLongPtrW / SetLayeredWindowAttributes calls.
            if !WindowsApi::is_window_thread_responding(
                hwnd,
                WindowsApi::WINDOW_THREAD_RESPONSE_TIMEOUT_MS,
            ) {
                tracing::debug!(hwnd, "skipping transparency update for unresponsive window");
                continue;
            }

            match window.transparent() {
                Err(error) => {
                    tracing::error!("failed to make unfocused window {hwnd} transparent: {error}")
                }
                Ok(..) => {
                    track_transparent_hwnd(&mut known_hwnds.lock(), hwnd);
                }
            }
        }

        // Prune windows destroyed since they were dimmed: decide_targets never re-emits destroyed
        // windows as targets, so without this the restore set would grow for the session lifetime.
        known_hwnds
            .lock()
            .retain(|&hwnd| WindowsApi::is_window(hwnd));
    }

    Ok(())
}

/// Decides which windows should be transparent and which should be opaque based on the current
/// WM state. Returns `(transparent_targets, opaque_targets)` as hwnd lists; no WinAPI calls are
/// made here so the WM lock is only ever briefly held while reading state.
fn decide_targets(
    state: &WindowManager,
    known_hwnds: &Mutex<Vec<isize>>,
    foreground_hwnd: isize,
    is_maximized: bool,
) -> (Vec<isize>, Vec<isize>) {
    let mut transparent_targets = Vec::new();
    let mut opaque_targets = Vec::new();

    // Hold the rule-list and restore-set locks once for the whole decision instead of re-locking
    // per workspace / per window; nothing here re-enters `should_manage()` or re-locks these, so a
    // single scoped hold is safe under the WM lock.
    let transparency_blacklist = TRANSPARENCY_BLACKLIST.lock();
    let regex_identifiers = REGEX_IDENTIFIERS.lock();
    let mut known = known_hwnds.lock();

    // A workspace/monitor switch has just happened on one or more monitors. During the
    // switch's event storm the OS foreground is transient: komorebi activates the new
    // focused window asynchronously and hiding the previous workspace auto-promotes other
    // windows in between, so the foreground bounces across the workspaces being restored.
    // Trusting the raw foreground here makes those transient windows stay opaque, pass
    // after pass, until the foreground finally settles - the sequential opaque flash
    // observed on `focus-workspaces`. While settling, base the "don't dim" rule purely on
    // WM focus state (which the workspace restore already set) and ignore the foreground.
    let switch_settling = state.any_monitor_switch_settling();

    let focused_monitor_idx = state.focused_monitor_idx();

    let global_enabled = TRANSPARENCY_ENABLED.load_consume();

    'monitors: for (monitor_idx, m) in state.monitors.elements().iter().enumerate() {
        let focused_workspace_idx = m.focused_workspace_idx();

        // Per-scope effective transparency: a runtime override on the monitor wins over the
        // global toggle, and an override on the workspace wins over both.
        let monitor_transparency = m.transparency.unwrap_or(global_enabled);

        // Pinned floating windows stay visible across all workspaces on the
        // monitor, so they are always kept opaque.
        for window in m.pinned_windows() {
            opaque_targets.push(window.hwnd);
        }

        'workspaces: for (workspace_idx, ws) in m.workspaces().iter().enumerate() {
            let workspace_transparency = ws.transparency.unwrap_or(monitor_transparency);

            // Non-focused workspaces are hidden; leave their windows at the transparency computed
            // when they were last focused. Force-opaquing them here would reveal them fully opaque
            // for one pass when the user switches to them, because the workspace is restored
            // before the next async pass runs.
            if workspace_idx != focused_workspace_idx {
                continue 'workspaces;
            }

            // Tiling-disabled workspaces don't have transparent windows
            if !ws.tile {
                for window in ws.visible_windows().iter().flatten() {
                    opaque_targets.push(window.hwnd);
                }

                continue 'workspaces;
            }

            // The monocle container is never transparent unless the toggle is enabled and its
            // monitor isn't focused: a monocle workspace is a fullscreen view of a single window,
            // so it is only dimmed when the user is looking at another monitor.
            if let Some(monocle) = &ws.monocle_container {
                if let Some(window) = monocle.focused_window() {
                    let transparent = workspace_transparency
                        && TRANSPARENCY_MONOCLE.load_consume()
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

            // A workspace with transparency disabled never dims its windows: restore the visible
            // ones to opaque (they were dimmed while the scope was enabled) and move on.
            if !workspace_transparency {
                for window in ws.visible_windows().iter().flatten() {
                    opaque_targets.push(window.hwnd);
                }

                continue 'workspaces;
            }

            // A maximized foreground window covers its whole monitor, so skip the container pass
            // for THAT monitor only; other monitors are still visible and their windows must keep
            // being dimmed/restored. The maximized window itself is forced opaque.
            if is_maximized && !switch_settling && monitor_idx == focused_monitor_idx {
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
                            ) || (!switch_settling && window.hwnd == foreground_hwnd);

                            if opaque {
                                opaque_targets.push(window.hwnd);
                            } else {
                                transparent_targets.push(window.hwnd);
                            }
                        } else {
                            // just in case, this is useful when people are clicking around
                            // on unfocused stackbar tabs
                            track_transparent_hwnd(&mut known, window.hwnd);
                        }
                    }
                // Otherwise, make it opaque
                } else {
                    let focused_window_idx = c.focused_window_idx();
                    for (window_idx, window) in c.windows().iter().enumerate() {
                        if window_idx != focused_window_idx {
                            track_transparent_hwnd(&mut known, window.hwnd);
                        } else {
                            opaque_targets.push(window.hwnd);
                        }
                    }
                };
            }

            // Floating windows are dimmed when the toggle is enabled, and only the OS foreground window
            // is ever exempt. On a tiling workspace only the raised floating window is visible, so
            // WM ring focus (focused_floating_window_idx) tracks that same window; keeping it
            // crisp matters more than honoring ring focus, and ring focus can lag behind the real
            // foreground the same way container focus does (see the container comment above).
            // Floats are always emitted as a target: when the toggle is off they must be actively
            // restored to opaque, otherwise a float dimmed by a previous pass would stay dimmed
            // (the `visible_windows()` opaque path only runs for non-focused workspaces).
            let floating_transparency = TRANSPARENCY_FLOATING.load_consume();

            for window in ws.floating_windows() {
                let opaque = !floating_transparency
                    || (!switch_settling && window.hwnd == foreground_hwnd)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::core::Rect;
    use crate::monitor;
    use crate::workspace::Workspace;
    use parking_lot::MutexGuard;
    use std::sync::atomic::Ordering;

    // The transparency statics are process-global, so tests that touch them must run one at a
    // time; otherwise the toggles set by one test leak into another running in parallel.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct StateGuard {
        _state_lock: MutexGuard<'static, ()>,
        previous_enabled: bool,
        previous_floating: bool,
        previous_monocle: bool,
    }

    impl StateGuard {
        fn lock() -> Self {
            Self {
                _state_lock: TEST_LOCK.lock(),
                previous_enabled: TRANSPARENCY_ENABLED.load_consume(),
                previous_floating: TRANSPARENCY_FLOATING.load_consume(),
                previous_monocle: TRANSPARENCY_MONOCLE.load_consume(),
            }
        }

        fn enable() -> Self {
            let guard = Self::lock();

            TRANSPARENCY_ENABLED.store(true, Ordering::SeqCst);
            TRANSPARENCY_FLOATING.store(true, Ordering::SeqCst);
            TRANSPARENCY_MONOCLE.store(false, Ordering::SeqCst);

            guard
        }

        fn disable_floating(self) -> Self {
            TRANSPARENCY_FLOATING.store(false, Ordering::SeqCst);
            self
        }
    }

    impl Drop for StateGuard {
        fn drop(&mut self) {
            TRANSPARENCY_ENABLED.store(self.previous_enabled, Ordering::SeqCst);
            TRANSPARENCY_FLOATING.store(self.previous_floating, Ordering::SeqCst);
            TRANSPARENCY_MONOCLE.store(self.previous_monocle, Ordering::SeqCst);
        }
    }

    fn window_manager_with_floats(floats: &[&[isize]]) -> WindowManager {
        let (_tx, rx) = crossbeam_channel::bounded(1);

        let mut wm = WindowManager::new(rx, None).unwrap();

        let mut m = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        for workspace_floats in floats {
            let workspace = m.workspaces_mut().back_mut().unwrap();

            for hwnd in *workspace_floats {
                workspace
                    .floating_windows_mut()
                    .push_back(Window::from(*hwnd));
            }

            m.workspaces_mut().push_back(Workspace::default());
        }

        wm.monitors_mut().push_back(m);

        wm
    }

    fn window_manager_with_two_monitors(floats: &[&[isize]]) -> WindowManager {
        let (_tx, rx) = crossbeam_channel::bounded(1);

        let mut wm = WindowManager::new(rx, None).unwrap();

        for (monitor_idx, workspace_floats) in floats.iter().enumerate() {
            let mut m = monitor::new(
                monitor_idx as isize,
                Rect::default(),
                Rect::default(),
                format!("TestMonitor{monitor_idx}"),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            let workspace = m.workspaces_mut().back_mut().unwrap();

            for hwnd in *workspace_floats {
                workspace
                    .floating_windows_mut()
                    .push_back(Window::from(*hwnd));
            }

            wm.monitors_mut().push_back(m);
        }

        wm
    }

    fn now_epoch_ms() -> u64 {
        use std::time::SystemTime;
        use std::time::UNIX_EPOCH;
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }

    #[test]
    fn test_unfocused_floating_windows_are_dimmed() {
        let _guard = StateGuard::enable();
        let wm = window_manager_with_floats(&[&[10, 20]]);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert_eq!(transparent, vec![10, 20]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_foreground_floating_window_stays_opaque() {
        let _guard = StateGuard::enable();
        let wm = window_manager_with_floats(&[&[10, 20]]);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 20, false);

        assert_eq!(transparent, vec![10]);
        assert_eq!(opaque, vec![20]);
    }

    #[test]
    fn test_ring_focused_floating_window_is_dimmed_when_not_foreground() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_floats(&[&[10, 20]]);

        let workspace = wm.focused_workspace_mut().unwrap();
        assert!(workspace.focus_floating_window(0));

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert_eq!(transparent, vec![10, 20]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_floating_toggle_disabled_restores_opaque() {
        let _guard = StateGuard::enable().disable_floating();
        let wm = window_manager_with_floats(&[&[10, 20]]);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert!(transparent.is_empty());
        assert_eq!(opaque, vec![10, 20]);
    }

    #[test]
    fn test_non_focused_workspace_windows_are_not_opaqued() {
        let _guard = StateGuard::enable();
        // ws0 is focused and holds float 10; ws1 is hidden and holds float 20.
        let wm = window_manager_with_floats(&[&[10], &[20]]);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        // Only the focused workspace's float is touched; the hidden workspace is left alone so
        // its windows keep their alpha when the user switches to it.
        assert_eq!(transparent, vec![10]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_pinned_floating_window_stays_opaque() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_floats(&[&[10], &[]]);

        // Pin float 10 on the monitor, then move focus to ws1 so ws0 is hidden but 10 stays visible.
        wm.monitors_mut()[0].pin_floating_window(10);

        wm.focused_monitor_mut()
            .unwrap()
            .focus_workspace(1)
            .unwrap();

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert!(transparent.is_empty());
        assert_eq!(opaque, vec![10]);
    }

    #[test]
    fn test_maximized_foreground_skips_monitor_transparency() {
        let _guard = StateGuard::enable();
        let wm = window_manager_with_floats(&[&[10]]);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, true);

        assert!(transparent.is_empty());
        assert_eq!(opaque, vec![999]);
    }

    #[test]
    fn test_maximized_foreground_skips_only_its_own_monitor() {
        let _guard = StateGuard::enable();
        // Monitor 0 is focused and its float 10 is maximized as the foreground; monitor 1 is
        // still visible and its float 20 must keep being dimmed rather than frozen by the
        // maximized short-circuit, which historically skipped every monitor.
        let wm = window_manager_with_two_monitors(&[&[10], &[20]]);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 10, true);

        assert_eq!(transparent, vec![20]);
        assert!(opaque.contains(&10));
    }

    #[test]
    fn test_maximized_foreground_on_unfocused_monitor_dimmed() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_two_monitors(&[&[10], &[20]]);
        wm.focus_monitor(1).unwrap();

        // The foreground (20) is maximized on the focused monitor; monitor 0's float 10 is on
        // an unfocused monitor and must be dimmed even while the maximized short-circuit holds
        // for monitor 1.
        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 20, true);

        assert!(transparent.contains(&10));
        assert!(!transparent.contains(&20));
        assert!(opaque.contains(&20));
    }

    #[test]
    fn test_transient_foreground_is_dimmed_while_switch_settling() {
        let _guard = StateGuard::enable();
        // Monitor 1 is unfocused and was just switched to (settling); its float 20 is the
        // transient OS foreground auto-promoted during the switch's event storm.
        let mut wm = window_manager_with_two_monitors(&[&[10], &[20]]);
        wm.monitors_mut()[1].last_switch_at = Some(now_epoch_ms());

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 20, false);

        // While settling, the raw foreground must not keep window 20 opaque: WM focus state
        // is authoritative, so on the unfocused monitor 20 is dimmed like everything else.
        assert!(transparent.contains(&20));
        assert!(transparent.contains(&10));
        assert!(!opaque.contains(&20));
    }

    #[test]
    fn test_foreground_exemption_restored_after_switch_settle() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_two_monitors(&[&[10], &[20]]);
        // The switch finished more than the grace window ago, so 20 is the settled,
        // trustworthy foreground again.
        wm.monitors_mut()[1].last_switch_at = Some(now_epoch_ms().saturating_sub(500));

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 20, false);

        assert!(opaque.contains(&20));
        assert!(!transparent.contains(&20));
        assert!(transparent.contains(&10));
    }

    #[test]
    fn test_maximized_short_circuit_ignored_while_switch_settling() {
        let _guard = StateGuard::enable();
        // Both monitors were just switched to (a `focus-workspaces` on all monitors), and the
        // transient foreground is reported as maximized. The short-circuit must not skip the
        // settling monitors, otherwise their restored windows flash opaque until it ends.
        let mut wm = window_manager_with_two_monitors(&[&[10], &[20]]);
        wm.monitors_mut()[0].last_switch_at = Some(now_epoch_ms());
        wm.monitors_mut()[1].last_switch_at = Some(now_epoch_ms());

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 20, true);

        assert!(transparent.contains(&10));
        assert!(transparent.contains(&20));
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_track_transparent_hwnd_is_idempotent() {
        let mut known = Vec::new();

        track_transparent_hwnd(&mut known, 10);
        track_transparent_hwnd(&mut known, 10);
        track_transparent_hwnd(&mut known, 20);

        assert_eq!(known, vec![10, 20]);
    }

    #[test]
    fn test_untrack_opaque_hwnd_removes_only_target() {
        let mut known = vec![10, 20, 30];

        untrack_opaque_hwnd(&mut known, 20);

        assert_eq!(known, vec![10, 30]);
    }

    #[test]
    fn test_hidden_workspace_window_remains_restorable() {
        let _guard = StateGuard::enable();
        // ws0 is focused and holds float 10; ws1 is hidden and holds float 20, which was dimmed
        // while ws1 was focused earlier and so is already in the restore set.
        let wm = window_manager_with_floats(&[&[10], &[20]]);
        let known_hwnds = Mutex::new(vec![20]);

        let (transparent, opaque) = decide_targets(&wm, &known_hwnds, 999, false);

        // Reconcile the restore set with the pass's decisions, as the apply phase does.
        let mut known = known_hwnds.into_inner();
        for hwnd in opaque {
            untrack_opaque_hwnd(&mut known, hwnd);
        }
        for hwnd in transparent {
            track_transparent_hwnd(&mut known, hwnd);
        }

        // The hidden workspace's dimmed window must still be tracked so that toggling
        // transparency off restores it to opaque; previously the per-pass clear dropped it.
        assert!(known.contains(&20));
    }

    #[test]
    fn test_toggle_monitor_transparency() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_floats(&[&[10]]);

        // The global toggle is on and the focused monitor carries no override yet, so toggling
        // flips the monitor override to off; its windows are restored to opaque.
        wm.toggle_monitor_transparency().unwrap();
        assert_eq!(wm.monitors_mut()[0].transparency, Some(false));
        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);
        assert!(transparent.is_empty());
        assert_eq!(opaque, vec![10]);

        // Toggling again flips the override back to on, which wins over the now-off state being
        // applied to the focused workspace (the global toggle is still on).
        wm.toggle_monitor_transparency().unwrap();
        assert_eq!(wm.monitors_mut()[0].transparency, Some(true));
        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);
        assert_eq!(transparent, vec![10]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_toggle_workspace_transparency_precedence() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_floats(&[&[10]]);

        // The focused monitor and workspace carry no overrides; the effective value falls back to
        // the global toggle (on), so toggling the workspace flips it to off.
        wm.toggle_workspace_transparency().unwrap();
        let workspace = wm.focused_workspace_mut().unwrap();
        assert_eq!(workspace.transparency, Some(false));

        // A monitor override is introduced; with the workspace override still unset the effective
        // value falls back to it, and the workspace toggle flips from that override.
        wm.monitors_mut()[0].transparency = Some(false);
        wm.toggle_workspace_transparency().unwrap();
        assert_eq!(wm.focused_workspace_mut().unwrap().transparency, Some(true));
    }

    #[test]
    fn test_workspace_override_disables_dimming_with_global_enabled() {
        let _guard = StateGuard::enable();
        let mut wm = window_manager_with_floats(&[&[10]]);
        wm.focused_workspace_mut().unwrap().transparency = Some(false);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert!(transparent.is_empty());
        assert_eq!(opaque, vec![10]);
    }

    #[test]
    fn test_workspace_override_enables_dimming_with_global_disabled() {
        let _guard = StateGuard::lock();
        TRANSPARENCY_ENABLED.store(false, Ordering::SeqCst);
        TRANSPARENCY_FLOATING.store(true, Ordering::SeqCst);
        let mut wm = window_manager_with_floats(&[&[10]]);
        wm.focused_workspace_mut().unwrap().transparency = Some(true);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert_eq!(transparent, vec![10]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_workspace_override_beats_monitor_override() {
        let _guard = StateGuard::lock();
        TRANSPARENCY_ENABLED.store(false, Ordering::SeqCst);
        TRANSPARENCY_FLOATING.store(true, Ordering::SeqCst);
        let mut wm = window_manager_with_floats(&[&[10]]);
        wm.focused_workspace_mut().unwrap().transparency = Some(true);
        wm.monitors_mut()[0].transparency = Some(false);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert_eq!(transparent, vec![10]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_monitor_override_enables_dimming_with_global_disabled() {
        let _guard = StateGuard::lock();
        TRANSPARENCY_ENABLED.store(false, Ordering::SeqCst);
        TRANSPARENCY_FLOATING.store(true, Ordering::SeqCst);
        let mut wm = window_manager_with_floats(&[&[10]]);
        wm.monitors_mut()[0].transparency = Some(true);

        let (transparent, opaque) = decide_targets(&wm, &Mutex::new(vec![]), 999, false);

        assert_eq!(transparent, vec![10]);
        assert!(opaque.is_empty());
    }

    #[test]
    fn test_static_config_transparency_legacy_boolean_applies() {
        let _guard = StateGuard::lock();
        let mut config =
            serde_json::from_str::<crate::StaticConfig>(r#"{ "transparency": true }"#).unwrap();
        config.apply_globals().unwrap();

        assert!(TRANSPARENCY_ENABLED.load_consume());
        assert!(!TRANSPARENCY_FLOATING.load_consume());
        assert!(!TRANSPARENCY_MONOCLE.load_consume());
        assert_eq!(TRANSPARENCY_ALPHA.load_consume(), 200);
    }

    #[test]
    fn test_static_config_transparency_nested_settings_apply() {
        let _guard = StateGuard::lock();
        let mut config = serde_json::from_str::<crate::StaticConfig>(
            r#"
            {
                "transparency": {
                    "enabled": true,
                    "alpha": 120,
                    "floating": true,
                    "monocle": true
                }
            }
            "#,
        )
        .unwrap();
        config.apply_globals().unwrap();

        assert!(TRANSPARENCY_ENABLED.load_consume());
        assert!(TRANSPARENCY_FLOATING.load_consume());
        assert!(TRANSPARENCY_MONOCLE.load_consume());
        assert_eq!(TRANSPARENCY_ALPHA.load_consume(), 120);
    }

    #[test]
    fn test_static_config_transparency_nested_settings_take_precedence() {
        let _guard = StateGuard::lock();
        let mut config = serde_json::from_str::<crate::StaticConfig>(
            r#"
            {
                "transparency": { "alpha": 90, "floating": true },
                "transparency_alpha": 150,
                "transparency_floating": false
            }
            "#,
        )
        .unwrap();
        config.apply_globals().unwrap();

        assert_eq!(TRANSPARENCY_ALPHA.load_consume(), 90);
        assert!(TRANSPARENCY_FLOATING.load_consume());
    }
}
