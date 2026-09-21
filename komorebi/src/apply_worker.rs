use std::sync::OnceLock;

use crossbeam_channel::unbounded;
use crossbeam_channel::Sender;

use crate::Window;

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
/// The underlying raise/lower/clear helpers still probe each window's thread
/// (`WindowsApi::is_window_thread_responding`) and skip unresponsive windows,
/// so the worker itself never blocks indefinitely either.
pub enum ApplyOp {
    /// Raise each window above the currently active window via the transient
    /// TopMost-band raise (`HWND_TOPMOST` then `HWND_NOTOPMOST`).
    RaiseAboveActive(Vec<Window>),
    /// Raise each window to the top of the normal band (synchronous `HWND_TOP`).
    Raise(Vec<Window>),
    /// Lower each window to the bottom of the Z order (synchronous
    /// `HWND_BOTTOM`).
    Lower(Vec<Window>),
    /// Lower every window to the bottom of the Z order in ONE deferred
    /// window-pos pass (`BeginDeferWindowPos`/`DeferWindowPos`/
    /// `EndDeferWindowPos`), so the whole batch drops behind the tiling base in
    /// a single screen-refreshing cycle instead of window by window.
    LowerBatch(Vec<Window>),
    /// Demote each window out of the TopMost band (synchronous `HWND_NOTOPMOST`).
    ClearTopmost(Vec<Window>),
    /// Place each window into the persistent TopMost band (synchronous
    /// `HWND_TOPMOST`), so it renders above every normal-band window.
    MakeTopmost(Vec<Window>),
    /// Raise a window immediately below another window in the Z order
    /// (synchronous relative insert), so it can never end up above the target
    /// even if the target holds the foreground.
    RaiseBelow { window: Window, target: Window },
    /// Bring each window to the foreground (activate it) as the physically
    /// final operation of a pass, after every earlier z-order op has been
    /// applied. Enqueueing the activation on this FIFO guarantees the
    /// foreground change can never race an in-flight raise/lower and get
    /// bounced by a later SetWindowPos.
    RaiseAndFocus(Vec<Window>),
}

pub struct ApplyWorker;

impl ApplyWorker {
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

    fn apply(op: ApplyOp) {
        match op {
            ApplyOp::RaiseAboveActive(windows) => {
                for window in windows {
                    if let Err(error) = window.raise_above_active() {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not raise window above active: {error}"
                        );
                    }
                }
            }
            ApplyOp::Raise(windows) => {
                for window in windows {
                    if let Err(error) = window.raise_sync() {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not raise window: {error}"
                        );
                    }
                }
            }
            ApplyOp::Lower(windows) => {
                for window in windows {
                    if let Err(error) = window.lower_sync() {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not lower window: {error}"
                        );
                    }
                }
            }
            ApplyOp::LowerBatch(windows) => {
                let hwnds = windows.iter().map(|window| window.hwnd).collect::<Vec<_>>();
                if let Err(error) = crate::windows_api::WindowsApi::lower_windows_sync(&hwnds) {
                    tracing::warn!(
                        windows = windows.len(),
                        "could not lower windows in a single pass: {error}"
                    );
                }
            }
            ApplyOp::ClearTopmost(windows) => {
                for window in windows {
                    if let Err(error) = crate::windows_api::WindowsApi::clear_topmost_window(
                        window.hwnd,
                    ) {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not clear topmost state of window: {error}"
                        );
                    }
                }
            }
            ApplyOp::MakeTopmost(windows) => {
                for window in windows {
                    if let Err(error) =
                        crate::windows_api::WindowsApi::make_topmost_window(window.hwnd)
                    {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not make window topmost: {error}"
                        );
                    }
                }
            }
            ApplyOp::RaiseBelow { window, target } => {
                if let Err(error) =
                    crate::windows_api::WindowsApi::raise_window_below(window.hwnd, target.hwnd)
                {
                    tracing::warn!(
                        hwnd = window.hwnd,
                        target = target.hwnd,
                        "could not raise window below target: {error}"
                    );
                }
            }
            ApplyOp::RaiseAndFocus(windows) => {
                for window in windows {
                    match crate::windows_api::WindowsApi::raise_and_focus_window(window.hwnd) {
                        Ok(()) => tracing::info!(
                            hwnd = window.hwnd,
                            "raised and focused window as final pass operation"
                        ),
                        Err(error) => tracing::warn!(
                            hwnd = window.hwnd,
                            "could not raise and focus window: {error}"
                        ),
                    }
                }
            }
        }
    }

    fn enqueue(op: ApplyOp) {
        if Self::sender().send(op).is_err() {
            tracing::warn!("could not enqueue apply operation: worker is gone");
        }
    }

    /// Raise the given windows above the currently active window, in order.
    pub fn raise_above_active(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::RaiseAboveActive(windows));
        }
    }

    /// Raise a window immediately below the given target window so the target
    /// stays on top of it no matter which window currently holds the
    /// foreground.
    pub fn raise_below(window: Window, target: Window) {
        Self::enqueue(ApplyOp::RaiseBelow { window, target });
    }

    /// Bring the given window to the foreground as the final operation of the
    /// current pass, after all previously enqueued z-order work has been
    /// applied. This is the only deterministic way to reset the foreground
    /// once a batch of raises/lowers has been in flight: a synchronous
    /// activation issued from the caller races those async operations and
    /// Windows can hand the foreground to a window whose raise landed last.
    pub fn raise_and_focus_hwnd(hwnd: isize) {
        Self::enqueue(ApplyOp::RaiseAndFocus(vec![Window::from(hwnd)]));
    }

    /// Raise the given windows to the top of the normal band, in order.
    pub fn raise(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::Raise(windows));
        }
    }

    /// Lower the given windows below the managed base, in order.
    pub fn lower(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::Lower(windows));
        }
    }

    /// Lower the given windows in a single atomic deferred window-pos pass so
    /// the whole overlay drops behind the tiling base together, without the
    /// window-by-window stagger of [`Self::lower`].
    pub fn lower_batch(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::LowerBatch(windows));
        }
    }

    /// Demote the given windows out of the TopMost band.
    pub fn clear_topmost(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::ClearTopmost(windows));
        }
    }

    /// Place the given windows into the persistent TopMost band, in order.
    pub fn make_topmost(windows: Vec<Window>) {
        if !windows.is_empty() {
            Self::enqueue(ApplyOp::MakeTopmost(windows));
        }
    }
}
