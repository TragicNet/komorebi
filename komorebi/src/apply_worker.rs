use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::Sender;
use crossbeam_channel::unbounded;

use crate::Window;
use crate::windows_api::WindowsApi;

/// A [`Window`] with a snapshot of the identity (owning process id) taken at
/// enqueue time.
///
/// The apply worker runs on a dedicated thread and can apply an op long after
/// it was queued. Windows recycles hwnds constantly (komorebi's own border,
/// stackbar, ghost and hidden windows churn handles all the time), so a stale
/// capture could otherwise raise, lower or focus a *completely different*
/// window than the one the operation was queued for. Every op re-validates the
/// handle before touching it and silently skips handles that died or were
/// recycled in the meantime.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct CapturedWindow {
    window: Window,
    process_id: u32,
}

impl CapturedWindow {
    pub(crate) fn from_window(window: Window) -> Self {
        let process_id = WindowsApi::window_thread_process_id(window.hwnd).0;
        Self { window, process_id }
    }

    pub(crate) fn hwnd(&self) -> isize {
        self.window.hwnd
    }

    /// Whether the captured hwnd still belongs to the same window it did at
    /// enqueue time. `IsWindow` on its own cannot detect a recycled handle,
    /// so the owning process id is compared against the enqueue-time snapshot.
    pub(crate) fn is_still_owned(&self) -> bool {
        WindowsApi::is_window(self.window.hwnd)
            && WindowsApi::window_thread_process_id(self.window.hwnd).0 == self.process_id
    }
}

/// A synchronous z-order operation applied on a dedicated worker thread, never
/// on the window-manager thread.
///
/// Synchronous win32 z-order calls (`SetWindowPos` without
/// `SWP_ASYNCWINDOWPOS`) marshal to the target window's thread and block until
/// that thread processes the message. A single Not Responding window would
/// otherwise wedge the whole window manager: every event is processed while
/// holding the WM lock, and socket commands time out (`could not acquire
/// window manager lock`). Posting the operations to this worker keeps the
/// blocking calls off the WM lock while preserving the relative z-order of a
/// pass, because a single FIFO worker applies them in submission order.
///
/// Every operation re-validates the window identity it was captured with
/// before applying (see [`CapturedWindow`]), so a queued op can never act on a
/// handle that was recycled into a different window while it waited in the
/// queue. The underlying raise/lower/clear helpers still probe each window's
/// thread (`WindowsApi::is_window_thread_responding`) and skip unresponsive
/// windows, so the worker itself never blocks indefinitely either.
pub enum ApplyOp {
    /// Raise each window above the currently active window via the transient
    /// TopMost-band raise (`HWND_TOPMOST` then `HWND_NOTOPMOST`).
    RaiseAboveActive(Vec<CapturedWindow>),
    /// Raise each window to the top of the normal band (synchronous `HWND_TOP`).
    Raise(Vec<CapturedWindow>),
    /// Lower each window to the bottom of the Z order (synchronous
    /// `HWND_BOTTOM`).
    Lower(Vec<CapturedWindow>),
    /// Lower every window to the bottom of the Z order in ONE deferred
    /// window-pos pass (`BeginDeferWindowPos`/`DeferWindowPos`/
    /// `EndDeferWindowPos`), so the whole batch drops behind the tiling base in
    /// a single screen-refreshing cycle instead of window by window.
    LowerBatch(Vec<CapturedWindow>),
    /// Raise every window to the top of the Z order in ONE deferred window-pos
    /// pass (`BeginDeferWindowPos`/`DeferWindowPos`/`EndDeferWindowPos`), so
    /// the whole batch rises above the band beneath it in a single
    /// screen-refreshing cycle instead of window by window. Entries are applied
    /// in slice order, so later windows land above earlier ones: pass the band
    /// bottom-to-top.
    RaiseBatch(Vec<CapturedWindow>),
    /// Demote each window out of the TopMost band (synchronous `HWND_NOTOPMOST`).
    ClearTopmost(Vec<CapturedWindow>),
    /// Place each window into the persistent TopMost band (synchronous
    /// `HWND_TOPMOST`), so it renders above every normal-band window.
    MakeTopmost(Vec<CapturedWindow>),
    /// Raise a window immediately below another window in the Z order
    /// (synchronous relative insert), so it can never end up above the target
    /// even if the target holds the foreground.
    RaiseBelow {
        window: CapturedWindow,
        target: CapturedWindow,
    },
    /// Bring each window to the foreground (activate it) as the physically
    /// final operation of a pass, after every earlier z-order op has been
    /// applied. Enqueueing the activation on this FIFO guarantees the
    /// foreground change can never race an in-flight raise/lower and get
    /// bounced by a later SetWindowPos.
    ///
    /// The activation is deferred, so the user may have focused another window
    /// while the pass drained. `enqueue_foreground` is the foreground at
    /// enqueue time: if it has moved to a third window by apply time, the
    /// activation was overtaken by an external focus change and is skipped so
    /// the OS foreground is never yanked away from a window the user just
    /// focused. `center_cursor` moves the cursor to the activated window's
    /// rect, on the worker, so the cursor only follows once the activation
    /// actually lands instead of teleporting ahead of the deferred activation.
    ///
    /// Command-initiated activations (layer toggles) pass `authoritative`,
    /// which suppresses the overtaken check while the op is younger than
    /// [`ACTIVATION_SETTLE_BUDGET`]: the toggle deterministically lands focus
    /// and the cursor on its intended window even when an external
    /// focus-follows-mouse daemon (masir, etc.) grabbed the foreground from a
    /// mid-drain cursor move. Once the budget lapses the strict guard applies
    /// again, so a stale activation can never steal focus from a genuine
    /// interaction that happened after the transition settled.
    RaiseAndFocus {
        windows: Vec<CapturedWindow>,
        center_cursor: bool,
        enqueue_foreground: Option<isize>,
        authoritative: bool,
        enqueued_at: Instant,
    },
}

pub struct ApplyWorker;

impl ApplyWorker {
    /// How long a command-initiated activation (a layer toggle) stays
    /// authoritative before the strict overtaken check applies again. Longer
    /// than the typical drain of a toggle's re-stack (raises and the final
    /// activation land within a few frames) but short enough that a genuinely
    /// delayed activation can never steal focus from an interaction that
    /// happened noticeably after the toggle. Mirrors the `suppress_layer_flips`
    /// settle window used for komorebi-caused focus changes.
    const ACTIVATION_SETTLE_BUDGET: Duration = Duration::from_millis(300);

    fn sender() -> Sender<ApplyOp> {
        static SENDER: OnceLock<Sender<ApplyOp>> = OnceLock::new();
        SENDER
            .get_or_init(|| {
                let (sender, receiver) = unbounded();
                std::thread::Builder::new()
                    .name("apply-worker".into())
                    .spawn(move || Self::receive(receiver))
                    .expect("could not spawn apply worker thread");
                sender
            })
            .clone()
    }

    fn receive(receiver: crossbeam_channel::Receiver<ApplyOp>) {
        for op in receiver {
            Self::apply(op);
        }
    }

    /// Apply `apply_one` to each still-valid window in the batch, skipping any
    /// window whose handle died or was recycled since it was captured.
    fn apply_on_valid(windows: &[CapturedWindow], apply_one: impl Fn(Window)) {
        for captured in windows {
            if !captured.is_still_owned() {
                tracing::debug!(
                    hwnd = captured.hwnd(),
                    "apply worker skipping operation for recycled or destroyed window"
                );
                continue;
            }
            apply_one(captured.window);
        }
    }

    fn apply(op: ApplyOp) {
        match op {
            ApplyOp::RaiseAboveActive(windows) => {
                Self::apply_on_valid(&windows, |window| {
                    if let Err(error) = window.raise_above_active() {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not raise window above active: {error}"
                        );
                    }
                });
            }
            ApplyOp::Raise(windows) => {
                Self::apply_on_valid(&windows, |window| {
                    if let Err(error) = window.raise_sync() {
                        tracing::warn!(hwnd = window.hwnd, "could not raise window: {error}");
                    }
                });
            }
            ApplyOp::Lower(windows) => {
                Self::apply_on_valid(&windows, |window| {
                    if let Err(error) = window.lower_sync() {
                        tracing::warn!(hwnd = window.hwnd, "could not lower window: {error}");
                    }
                });
            }
            ApplyOp::LowerBatch(windows) => {
                let hwnds = windows
                    .iter()
                    .filter(|captured| {
                        if captured.is_still_owned() {
                            true
                        } else {
                            tracing::debug!(
                                hwnd = captured.hwnd(),
                                "apply worker skipping lower batch entry for recycled or \
                                 destroyed window"
                            );
                            false
                        }
                    })
                    .map(|captured| captured.hwnd())
                    .collect::<Vec<_>>();
                if !hwnds.is_empty()
                    && let Err(error) = crate::windows_api::WindowsApi::lower_windows_sync(&hwnds)
                {
                    tracing::warn!(
                        windows = hwnds.len(),
                        "could not lower windows in a single pass: {error}"
                    );
                }
            }
            ApplyOp::RaiseBatch(windows) => {
                let hwnds = windows
                    .iter()
                    .filter(|captured| {
                        if captured.is_still_owned() {
                            true
                        } else {
                            tracing::debug!(
                                hwnd = captured.hwnd(),
                                "apply worker skipping raise batch entry for recycled or \
                                 destroyed window"
                            );
                            false
                        }
                    })
                    .map(|captured| captured.hwnd())
                    .collect::<Vec<_>>();
                if !hwnds.is_empty()
                    && let Err(error) = crate::windows_api::WindowsApi::raise_windows_sync(&hwnds)
                {
                    tracing::warn!(
                        windows = hwnds.len(),
                        "could not raise windows in a single pass: {error}"
                    );
                }
            }
            ApplyOp::ClearTopmost(windows) => {
                Self::apply_on_valid(&windows, |window| {
                    if let Err(error) =
                        crate::windows_api::WindowsApi::clear_topmost_window(window.hwnd)
                    {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not clear topmost state of window: {error}"
                        );
                    }
                });
            }
            ApplyOp::MakeTopmost(windows) => {
                Self::apply_on_valid(&windows, |window| {
                    if let Err(error) =
                        crate::windows_api::WindowsApi::make_topmost_window(window.hwnd)
                    {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not make window topmost: {error}"
                        );
                    }
                });
            }
            ApplyOp::RaiseBelow { window, target } => {
                if window.is_still_owned() && target.is_still_owned() {
                    if let Err(error) = crate::windows_api::WindowsApi::raise_window_below(
                        window.hwnd(),
                        target.hwnd(),
                    ) {
                        tracing::warn!(
                            hwnd = window.hwnd(),
                            target = target.hwnd(),
                            "could not raise window below target: {error}"
                        );
                    }
                } else {
                    tracing::debug!(
                        hwnd = window.hwnd(),
                        target = target.hwnd(),
                        "apply worker skipping raise-below for recycled or destroyed window"
                    );
                }
            }
            ApplyOp::RaiseAndFocus {
                windows,
                center_cursor,
                enqueue_foreground,
                authoritative,
                enqueued_at,
            } => {
                let mut kept_any = false;
                // An authoritative activation is honest for the settle budget:
                // the toggle it belongs to is still transitioning, so a third
                // window grabbing the foreground during the drain (an external
                // focus-follows-mouse move) must not silence the toggle's
                // intended focus and cursor transfer. Past the budget the
                // strict overtaken check applies so a delayed activation can
                // never steal focus from a genuine later interaction.
                let authoritative =
                    authoritative && enqueued_at.elapsed() < Self::ACTIVATION_SETTLE_BUDGET;
                for captured in &windows {
                    if !captured.is_still_owned() {
                        tracing::debug!(
                            hwnd = captured.hwnd(),
                            "apply worker skipping deferred activation for recycled or \
                             destroyed window"
                        );
                        continue;
                    }
                    kept_any = true;

                    let hwnd = captured.hwnd();
                    // If the foreground has moved to a third window since the
                    // activation was enqueued, the operation was overtaken by an
                    // external focus change (e.g. the user clicked another tile
                    // while the pass drained). Forcing the foreground without
                    // releasing it first would steal focus back from a window
                    // the user just chose, so the stale activation is skipped.
                    let current_foreground = WindowsApi::foreground_window().unwrap_or_default();
                    let overtaken = !authoritative
                        && current_foreground != hwnd
                        && enqueue_foreground
                            .is_some_and(|foreground| current_foreground != foreground);
                    if overtaken {
                        tracing::info!(
                            hwnd,
                            current_foreground,
                            enqueue_foreground = enqueue_foreground.unwrap_or_default(),
                            "skipping deferred activation: foreground changed since enqueue"
                        );
                        continue;
                    }

                    match WindowsApi::raise_and_focus_window(hwnd) {
                        Ok(()) => {
                            tracing::info!(
                                hwnd,
                                "raised and focused window as final pass operation"
                            );
                            if center_cursor && let Ok(rect) = WindowsApi::window_rect(hwnd) {
                                let _ = WindowsApi::center_cursor_in_rect(&rect);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(hwnd, "could not raise and focus window: {error}")
                        }
                    }
                }

                if !kept_any {
                    tracing::debug!(
                        "apply worker dropped a deferred activation batch with no valid windows"
                    );
                }
            }
        }
    }

    fn enqueue(op: ApplyOp) {
        if Self::sender().send(op).is_err() {
            tracing::warn!("could not enqueue apply operation: worker is gone");
        }
    }

    fn capture(windows: Vec<Window>) -> Vec<CapturedWindow> {
        windows
            .into_iter()
            .map(CapturedWindow::from_window)
            .collect()
    }

    /// Raise the given windows above the currently active window, in order.
    pub fn raise_above_active(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::RaiseAboveActive(Self::capture(windows)));
        }
    }

    /// Raise a window immediately below the given target window so the target
    /// stays on top of it no matter which window currently holds the
    /// foreground.
    pub fn raise_below(window: Window, target: Window) {
        Self::enqueue(ApplyOp::RaiseBelow {
            window: CapturedWindow::from_window(window),
            target: CapturedWindow::from_window(target),
        });
    }

    /// Bring the given window to the foreground as the final operation of the
    /// current pass, after all previously enqueued z-order work has been
    /// applied.
    ///
    /// `center_cursor` additionally moves the cursor to the window's current
    /// rect, applied on the worker only once the activation actually lands, so
    /// a mouse-follows-focus cursor never teleports ahead of the deferred
    /// activation. The activation is skipped entirely if the foreground moved
    /// to a third window while the pass drained, so a stale activation can
    /// never yank focus away from a window the user focused in the meantime.
    ///
    /// `authoritative` marks command-initiated activations (layer toggles): for
    /// [`ACTIVATION_SETTLE_BUDGET`] the overtaken check is suppressed so the
    /// toggle's intended focus and cursor transfer land deterministically even
    /// when an external focus-follows-mouse daemon grabbed the foreground from
    /// a mid-drain cursor move. After the budget the strict guard applies again.
    pub fn raise_and_focus_hwnd(hwnd: isize, center_cursor: bool, authoritative: bool) {
        let enqueue_foreground = WindowsApi::foreground_window().ok();
        Self::enqueue(ApplyOp::RaiseAndFocus {
            windows: Self::capture(vec![Window::from(hwnd)]),
            center_cursor,
            enqueue_foreground,
            authoritative,
            enqueued_at: Instant::now(),
        });
    }

    /// Raise the given windows to the top of the normal band, in order.
    pub fn raise(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::Raise(Self::capture(windows)));
        }
    }

    /// Lower the given windows below the managed base, in order.
    pub fn lower(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::Lower(Self::capture(windows)));
        }
    }

    /// Lower the given windows in a single atomic deferred window-pos pass so
    /// the whole overlay drops behind the tiling base together, without the
    /// window-by-window stagger of [`Self::lower`].
    pub fn lower_batch(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::LowerBatch(Self::capture(windows)));
        }
    }

    /// Raise the given windows in a single atomic deferred window-pos pass so
    /// the whole band rises together, without the window-by-window stagger of
    /// [`Self::raise`]. Entries are applied in slice order, so later windows
    /// land above earlier ones: pass the band bottom-to-top.
    pub fn raise_batch(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::RaiseBatch(Self::capture(windows)));
        }
    }

    /// Demote the given windows out of the TopMost band.
    pub fn clear_topmost(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::ClearTopmost(Self::capture(windows)));
        }
    }

    /// Place the given windows into the persistent TopMost band, in order.
    pub fn make_topmost(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::MakeTopmost(Self::capture(windows)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CapturedWindow;
    use crate::Window;

    #[test]
    fn test_captured_window_rejects_invalid_hwnd() {
        // A handle that was never allocated can never pass the liveness check,
        // so an op captured for it must be skipped at apply time.
        let captured = CapturedWindow::from_window(Window::from(0xbeef));
        assert!(!captured.is_still_owned());
    }
}
