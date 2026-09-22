#![deny(clippy::unwrap_used, clippy::expect_used)]

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::WindowManager;
use crate::core::Rect;
use crate::windows_api::WindowsApi;

pub struct Notification(pub isize, pub Option<Rect>);

static CHANNEL: OnceLock<(Sender<Notification>, Receiver<Notification>)> = OnceLock::new();

pub fn channel() -> &'static (Sender<Notification>, Receiver<Notification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(20))
}

fn event_tx() -> Sender<Notification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<Notification> {
    channel().1.clone()
}

// Currently this should only be used for async focus updates, such as
// when an animation finishes and we need to focus to set the cursor
// position if the user has mouse follows focus enabled. The rect is the
// final position the animated window was commanded to (not a live
// GetWindowRect read): the animation's last position op is posted
// asynchronously, so by the time this thread acts the window may not have
// physically arrived yet, and centering the cursor on a live read would
// land it at the stale start position (typically the top-left spawn spot).
pub fn send_notification(hwnd: isize, rect: Option<Rect>) {
    if event_tx().try_send(Notification(hwnd, rect)).is_err() {
        tracing::warn!("channel is full; dropping notification")
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

    for notification in receiver {
        let mouse_follows_focus = wm.lock().mouse_follows_focus;
        let (hwnd, rect) = (notification.0, notification.1);

        // The notification was sent for a hwnd that was the foreground at the
        // time; by the time this thread acts on it the window may have been
        // destroyed and its hwnd recycled into a completely different window.
        // Acting on the recycled handle would teleport the cursor to and
        // activate an unrelated window, so a dead handle is skipped.
        if !WindowsApi::is_window(hwnd) {
            tracing::debug!(
                hwnd,
                "focus manager skipping notification for destroyed window"
            );
            continue;
        }

        // Only sync the cursor when the animated window is still the OS
        // foreground. The animation it tracked finished under it; if the
        // foreground has since moved to another window (a real user or app
        // focus change), re-raising and re-focusing would steal focus back
        // from the window that was just chosen.
        if !WindowsApi::foreground_window()
            .map(|foreground| foreground == hwnd)
            .unwrap_or(false)
        {
            tracing::debug!(
                hwnd,
                "focus manager skipping notification: animated window is no longer the foreground"
            );
            continue;
        }

        if mouse_follows_focus && let Some(rect) = rect {
            let _ = WindowsApi::center_cursor_in_rect(&rect);
        }
    }

    Ok(())
}
