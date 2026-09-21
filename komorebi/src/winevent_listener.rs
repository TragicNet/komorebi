use std::sync::OnceLock;
use std::time::Duration;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use windows::Win32::UI::Accessibility::SetWinEventHook;
use windows::Win32::UI::WindowsAndMessaging::DispatchMessageW;
use windows::Win32::UI::WindowsAndMessaging::EVENT_MAX;
use windows::Win32::UI::WindowsAndMessaging::EVENT_MIN;
use windows::Win32::UI::WindowsAndMessaging::GetMessageW;
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::Win32::UI::WindowsAndMessaging::TranslateMessage;
use windows::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT;
use windows::Win32::UI::WindowsAndMessaging::WINEVENT_SKIPOWNPROCESS;

use crate::window_manager_event::WindowManagerEvent;
use crate::windows_callbacks;

/// Maximum number of window events buffered before the emitting thread (the
/// WinEvent hook) applies backpressure. Bounding the queue guarantees a window
/// event storm can never grow memory unboundedly; under sustained overload the
/// hook drops events instead, which is safe because the window manager rebuilds
/// its view of the world from source-of-truth Win32 probes on the next event.
const WINEVENT_CHANNEL_CAPACITY: usize = 1024;

static CHANNEL: OnceLock<(Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>)> =
    OnceLock::new();

static EVENT_PUMP: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

pub fn start() {
    EVENT_PUMP.get_or_init(|| {
        std::thread::spawn(move || {
            unsafe {
                SetWinEventHook(
                    EVENT_MIN,
                    EVENT_MAX,
                    None,
                    Some(windows_callbacks::win_event_hook),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };

            let mut msg: MSG = MSG::default();

            loop {
                unsafe {
                    if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        tracing::debug!("windows event processing thread shutdown");
                        break;
                    };
                    // TODO: error handling
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                std::thread::sleep(Duration::from_millis(10))
            }
        })
    });
}

fn channel() -> &'static (Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(WINEVENT_CHANNEL_CAPACITY))
}

pub fn event_tx() -> Sender<WindowManagerEvent> {
    channel().0.clone()
}

pub fn event_rx() -> Receiver<WindowManagerEvent> {
    channel().1.clone()
}
