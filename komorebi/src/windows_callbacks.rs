use std::collections::VecDeque;
use std::time::Duration;

use crate::border_manager;
use crate::container::Container;
use crate::window::RuleDebug;
use crate::window::Window;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;
use crate::winevent::WinEvent;
use crate::winevent_listener;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::WPARAM;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
use windows::Win32::UI::WindowsAndMessaging::GWL_STYLE;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongW;
use windows::Win32::UI::WindowsAndMessaging::OBJID_WINDOW;
use windows::Win32::UI::WindowsAndMessaging::SendNotifyMessageW;
use windows::Win32::UI::WindowsAndMessaging::WS_CHILD;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_NOACTIVATE;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use windows_core::BOOL;

pub extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let containers = unsafe { &mut *(lparam.0 as *mut VecDeque<Container>) };

    let is_visible = WindowsApi::is_window_visible(hwnd.0 as isize);
    let is_window = WindowsApi::is_window(hwnd.0 as isize);
    let is_minimized = WindowsApi::is_iconic(hwnd.0 as isize);
    let is_maximized = WindowsApi::is_zoomed(hwnd.0 as isize);

    if is_visible && is_window && !is_minimized {
        let window = Window::from(hwnd);

        if let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
            && should_manage
        {
            if is_maximized {
                WindowsApi::restore_window(window.hwnd);
            }

            let mut container = Container::default();
            container.windows_mut().push_back(window);
            containers.push_back(container);
        }
    }

    true.into()
}

pub extern "system" fn enum_minimized_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let hwnds = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };

    let is_window = WindowsApi::is_window(hwnd.0 as isize);
    let is_minimized = WindowsApi::is_iconic(hwnd.0 as isize);

    if is_window && is_minimized {
        let window = Window::from(hwnd);

        if let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
            && should_manage
        {
            hwnds.push(hwnd.0 as isize);
        }
    }

    true.into()
}

pub extern "system" fn enum_ignored_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };

    let window = Window::from(hwnd);
    let is_visible = WindowsApi::is_window_visible(window.hwnd);
    let is_window = WindowsApi::is_window(window.hwnd);
    let is_minimized = WindowsApi::is_iconic(window.hwnd);

    if is_visible
        && is_window
        && !is_minimized
        && let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
        && !should_manage
    {
        windows.push(window);
    }

    true.into()
}

/// Records every visible top-level window handle, in the order EnumWindows
/// reports them (top-to-bottom Z order). Diagnostic helper for inspecting
/// the layer stack after a transition.
pub extern "system" fn enum_all_visible_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let hwnds = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };

    if WindowsApi::is_window(hwnd.0 as isize) && WindowsApi::is_window_visible(hwnd.0 as isize) {
        hwnds.push(hwnd.0 as isize);
    }

    true.into()
}

pub extern "system" fn alt_tab_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };

    let is_visible = WindowsApi::is_window_visible(hwnd.0 as isize);
    let is_window = WindowsApi::is_window(hwnd.0 as isize);
    let is_minimized = WindowsApi::is_iconic(hwnd.0 as isize);

    if is_visible && is_window && !is_minimized {
        let window = Window::from(hwnd);

        if let Ok(should_manage) = window.should_manage(None, &mut RuleDebug::default())
            && should_manage
        {
            windows.push(window);
        }
    }

    true.into()
}

fn has_filtered_style(hwnd: HWND) -> bool {
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) as u32 };
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 };

    style & WS_CHILD.0 != 0
        || ex_style & WS_EX_TOOLWINDOW.0 != 0
        || ex_style & WS_EX_NOACTIVATE.0 != 0
}

pub extern "system" fn win_event_hook(
    _h_win_event_hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if id_object != OBJID_WINDOW.0 {
        return;
    }

    let window = Window::from(hwnd);

    let winevent = match WinEvent::try_from(event) {
        Ok(event) => event,
        Err(_) => return,
    };

    // this forwards the message to the window's border when it moves or is destroyed
    // see border_manager/border.rs
    if matches!(
        winevent,
        WinEvent::ObjectLocationChange | WinEvent::ObjectDestroy
    ) && !has_filtered_style(hwnd)
    {
        let border_info = border_manager::window_border(hwnd.0 as isize);

        if let Some(border_info) = border_info {
            unsafe {
                let _ = SendNotifyMessageW(
                    border_info.hwnd(),
                    event,
                    WPARAM(0),
                    LPARAM(hwnd.0 as isize),
                );
            }
        }
    }

    // sometimes the border focus state and colors don't get updated because this event comes too
    // slow for the value of GetForegroundWindow to be up to date by the time it is inspected in
    // the border manager to determine if a window show have its border show as "focused"
    //
    // so here we can just fire another event at the border manager when the system has finally
    // registered the new foreground window and this time the correct border colors will be applied
    if matches!(winevent, WinEvent::SystemForeground) && !has_filtered_style(hwnd) {
        border_manager::send_notification(Some(hwnd.0 as isize));
    }

    let event_type = match WindowManagerEvent::from_win_event(winevent, window) {
        None => {
            tracing::trace!(
                "Unhandled WinEvent: {winevent} (hwnd: {}, exe: {}, title: {}, class: {})",
                window.hwnd,
                window.exe().unwrap_or_default(),
                window.title().unwrap_or_default(),
                window.class().unwrap_or_default()
            );

            return;
        }
        Some(event) => event,
    };

    // Under an event storm the bounded queue can saturate. Never drop the
    // events the window manager cannot recover from Win32 probes (foreground
    // tracking, geometry/visibility changes): those block the hook until the
    // window manager drains the queue, preserving losslessness. Only the noisy
    // positional/title events are dropped, so the buffer clamp alone bounds
    // memory and the hook always returns promptly in practice. WinEvent hooks
    // run on the thread that placed them for out-of-context hooks.
    let event_tx = winevent_listener::event_tx();
    if event_type.drop_allowed() {
        if event_tx
            .send_timeout(event_type, Duration::from_millis(50))
            .is_err()
        {
            tracing::debug!(event = ?event_type, "dropped winevent under overload");
        }
    } else if event_tx.send(event_type).is_err() {
        tracing::warn!(event = ?event_type, "failed to enqueue critical winevent");
    }
}
