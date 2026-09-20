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
    /// Demote each window out of the TopMost band (synchronous `HWND_NOTOPMOST`).
    ClearTopmost(Vec<Window>),
    /// Place each window into the persistent TopMost band (synchronous
    /// `HWND_TOPMOST`), so it renders above every normal-band window.
    MakeTopmost(Vec<Window>),
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
                    if let Err(error) = crate::windows_api::WindowsApi::make_topmost_window(
                        window.hwnd,
                    ) {
                        tracing::warn!(
                            hwnd = window.hwnd,
                            "could not make window topmost: {error}"
                        );
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
