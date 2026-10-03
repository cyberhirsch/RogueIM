//! Edge docking of the buddy list (PRD DK-1..6).
//!
//! The buddy list is always docked and always on top.
//! * Windows: a real AppBar (SHAppBarMessage) that reserves screen space on the
//!   chosen monitor, so maximised windows there stay out of the strip.
//! * Linux/X11 (also XWayland — the app prefers the X11 backend when both are
//!   available): a dock-type window with `_NET_WM_STRUT_PARTIAL`, which window
//!   managers (KWin, Mutter, Xfwm, Openbox, i3, …) honour.
//! * macOS: no public API reserves screen space; the bar snaps to the edge,
//!   floats above other windows and shows on every Space.
//! `reserve = false` places the bar as an overlay (used while auto-hidden).

/// A display, as far as docking cares.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// Stable-ish id (Windows device name, monitor name elsewhere).
    pub id: String,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
    /// Effective DPI (96 = 100 %).
    pub dpi: u32,
}

fn place(w: &slint::Window, x: i32, y: i32, width: u32, height: u32) {
    w.set_position(slint::PhysicalPosition::new(x, y));
    w.set_size(slint::PhysicalSize::new(width, height));
}

#[cfg(windows)]
mod imp {
    use super::Monitor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::cell::Cell;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::Shell::{
        SHAppBarMessage, ABE_LEFT, ABE_RIGHT, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{MONITORINFOF_PRIMARY, WM_USER};

    thread_local! {
        /// Whether our window is currently registered as an AppBar.
        static REGISTERED: Cell<bool> = const { Cell::new(false) };
    }

    fn hwnd(w: &slint::Window) -> Option<HWND> {
        match w.window_handle().window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
            _ => None,
        }
    }

    unsafe extern "system" fn collect(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
        let out = unsafe { &mut *(data as *mut Vec<Monitor>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info as *mut _ as *mut _) } != 0 {
            let r = info.monitorInfo.rcMonitor;
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            let (mut dx, mut dy) = (96u32, 96u32);
            unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            out.push(Monitor {
                id: String::from_utf16_lossy(&info.szDevice[..len]),
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                dpi: dx.max(96),
            });
        }
        TRUE
    }

    pub fn monitors(_w: &slint::Window) -> Vec<Monitor> {
        let mut v: Vec<Monitor> = vec![];
        unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(collect), &mut v as *mut _ as LPARAM) };
        v.sort_by_key(|m| (m.left, m.top));
        v
    }

    fn data(h: HWND, left: bool) -> APPBARDATA {
        APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: h,
            uCallbackMessage: WM_USER + 0x52,
            uEdge: if left { ABE_LEFT } else { ABE_RIGHT },
            rc: RECT { left: 0, top: 0, right: 0, bottom: 0 },
            lParam: 0,
        }
    }

    /// No taskbar button (and no Alt+Tab entry) for the bar: it is always
    /// visible, and the tray icon and hotkey reach it. A tool window has none.
    pub fn hide_from_taskbar(w: &slint::Window) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, ShowWindow, GWL_EXSTYLE, SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW};
        let Some(h) = hwnd(w) else { return };
        unsafe {
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            let want = (ex | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize);
            if ex != want {
                // The taskbar only notices the new style when the window reappears.
                ShowWindow(h, SW_HIDE);
                SetWindowLongPtrW(h, GWL_EXSTYLE, want);
                ShowWindow(h, SW_SHOWNOACTIVATE);
            }
        }
    }

    /// Let mouse clicks pass through a window (the share frame).
    pub fn click_through(w: &slint::Window) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT};
        let Some(h) = hwnd(w) else { return };
        unsafe {
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex | (WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_NOACTIVATE) as isize);
            // A layered window needs its opacity set to show at all.
            windows_sys::Win32::UI::WindowsAndMessaging::SetLayeredWindowAttributes(h, 0, 255, windows_sys::Win32::UI::WindowsAndMessaging::LWA_ALPHA);
        }
    }

    pub fn dock(w: &slint::Window, width_logical: f32, left: bool, mon: &Monitor, reserve: bool) -> bool {
        let Some(h) = hwnd(w) else { return false };
        let width = (width_logical * mon.dpi as f32 / 96.0).round().max(4.0) as i32;
        if !reserve {
            undock(w);
            let x = if left { mon.left } else { mon.right - width };
            super::place(w, x, mon.top, width as u32, (mon.bottom - mon.top) as u32);
            return true;
        }
        unsafe {
            let mut abd = data(h, left);
            // Register once. Re-registering (REMOVE + NEW) before every move
            // races with the shell: QUERYPOS still sees our old strip and
            // pushes the bar inward by its own width.
            if !REGISTERED.get() {
                SHAppBarMessage(ABM_NEW, &mut abd);
                REGISTERED.set(true);
            }
            abd.rc = if left {
                RECT { left: mon.left, top: mon.top, right: mon.left + width, bottom: mon.bottom }
            } else {
                RECT { left: mon.right - width, top: mon.top, right: mon.right, bottom: mon.bottom }
            };
            SHAppBarMessage(ABM_QUERYPOS, &mut abd);
            if left {
                abd.rc.right = abd.rc.left + width;
            } else {
                abd.rc.left = abd.rc.right - width;
            }
            SHAppBarMessage(ABM_SETPOS, &mut abd);
            super::place(w, abd.rc.left, abd.rc.top, width as u32, (abd.rc.bottom - abd.rc.top) as u32);
        }
        true
    }

    pub fn undock(w: &slint::Window) {
        let Some(h) = hwnd(w) else { return };
        if REGISTERED.get() {
            unsafe {
                let mut abd = data(h, false);
                SHAppBarMessage(ABM_REMOVE, &mut abd);
            }
            REGISTERED.set(false);
        }
    }

    /// Mouse position in physical screen pixels.
    pub fn cursor(_w: &slint::Window) -> Option<(i32, i32)> {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT { x: 0, y: 0 };
        (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Monitor;
    use slint::winit_030::WinitWindowAccessor;

    pub fn monitors(w: &slint::Window) -> Vec<Monitor> {
        let mut v = w
            .with_winit_window(|ww| {
                let primary = ww.primary_monitor();
                ww.available_monitors()
                    .map(|m| {
                        let p = m.position();
                        let s = m.size();
                        Monitor {
                            id: m.name().unwrap_or_else(|| format!("{}x{}@{},{}", s.width, s.height, p.x, p.y)),
                            left: p.x,
                            top: p.y,
                            right: p.x + s.width as i32,
                            bottom: p.y + s.height as i32,
                            primary: primary.as_ref() == Some(&m),
                            dpi: (m.scale_factor() * 96.0).round() as u32,
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        v.sort_by_key(|m| (m.left, m.top));
        v
    }

    #[cfg(target_os = "linux")]
    fn x11_strut(w: &slint::Window, left: bool, width: i32, mon: &Monitor, reserve: bool) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
        use x11rb::wrapper::ConnectionExt as _;
        let wh = w.window_handle();
        let Ok(h) = wh.window_handle() else { return };
        let win: u32 = match h.as_raw() {
            RawWindowHandle::Xlib(x) => x.window as u32,
            RawWindowHandle::Xcb(x) => x.window.get(),
            _ => return,
        };
        let Ok((conn, screen)) = x11rb::connect(None) else { return };
        let root = &conn.setup().roots[screen];
        let (rw, rh) = (root.width_in_pixels as i32, root.height_in_pixels as i32);
        let atom = |n: &str| conn.intern_atom(false, n.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
        let (Some(wt), Some(dock), Some(strut), Some(partial)) = (atom("_NET_WM_WINDOW_TYPE"), atom("_NET_WM_WINDOW_TYPE_DOCK"), atom("_NET_WM_STRUT"), atom("_NET_WM_STRUT_PARTIAL")) else { return };
        let _ = conn.change_property32(PropMode::REPLACE, win, wt, AtomEnum::ATOM, &[dock]);
        let (l, r) = if !reserve { (0, 0) } else if left { (mon.left + width, 0) } else { (0, rw - mon.right + width) };
        let (ly0, ly1, ry0, ry1) = (mon.top, mon.bottom - 1, mon.top, mon.bottom - 1);
        let _ = rh;
        let _ = conn.change_property32(PropMode::REPLACE, win, strut, AtomEnum::CARDINAL, &[l as u32, r as u32, 0, 0]);
        let _ = conn.change_property32(
            PropMode::REPLACE,
            win,
            partial,
            AtomEnum::CARDINAL,
            &[l as u32, r as u32, 0, 0, ly0 as u32, ly1 as u32, ry0 as u32, ry1 as u32, 0, 0, 0, 0],
        );
        let _ = conn.flush();
    }

    /// Show the bar on every Space (winit has no API for it).
    #[cfg(target_os = "macos")]
    fn mac_all_spaces(w: &slint::Window) {
        use objc2_app_kit::{NSView, NSWindowCollectionBehavior};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let wh = w.window_handle();
        let Ok(h) = wh.window_handle() else { return };
        let RawWindowHandle::AppKit(a) = h.as_raw() else { return };
        // SAFETY: winit's NSView lives as long as the window; Slint calls us on the main thread.
        let view: &NSView = unsafe { a.ns_view.cast::<NSView>().as_ref() };
        if let Some(win) = view.window() {
            win.setCollectionBehavior(NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::Stationary);
        }
    }

    pub fn dock(w: &slint::Window, width_logical: f32, left: bool, mon: &Monitor, reserve: bool) -> bool {
        let width = (width_logical * mon.dpi as f32 / 96.0).round().max(4.0) as i32;
        let x = if left { mon.left } else { mon.right - width };
        super::place(w, x, mon.top, width as u32, (mon.bottom - mon.top) as u32);
        #[cfg(target_os = "macos")]
        mac_all_spaces(w);
        #[cfg(target_os = "linux")]
        x11_strut(w, left, width, mon, reserve);
        let _ = reserve;
        true
    }

    /// Linux: the bar is a dock-type window, which taskbars already skip.
    /// macOS: the Dock shows apps, not windows; hiding the app icon would also
    /// hide the chat windows, so it stays.
    pub fn hide_from_taskbar(_w: &slint::Window) {}

    /// Elsewhere the share frame is only 4 px wide; clicks on it are rare.
    pub fn click_through(_w: &slint::Window) {}

    pub fn undock(w: &slint::Window) {
        #[cfg(target_os = "linux")]
        {
            let m = Monitor { id: String::new(), left: 0, top: 0, right: 0, bottom: 0, primary: true, dpi: 96 };
            x11_strut(w, false, 0, &m, false);
        }
        let _ = w;
    }

    /// Mouse position in physical screen pixels.
    #[cfg(target_os = "linux")]
    pub fn cursor(_w: &slint::Window) -> Option<(i32, i32)> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;
        thread_local! {
            static CONN: Option<(x11rb::rust_connection::RustConnection, usize)> = x11rb::connect(None).ok();
        }
        CONN.with(|c| {
            let (conn, screen) = c.as_ref()?;
            let root = conn.setup().roots.get(*screen)?.root;
            let r = conn.query_pointer(root).ok()?.reply().ok()?;
            Some((r.root_x as i32, r.root_y as i32))
        })
    }

    /// Mouse position in physical screen pixels. Quartz reports points; each
    /// monitor has its own scale, so convert with the scale of the monitor the
    /// mouse is on (winit places monitors at point origin x scale).
    #[cfg(target_os = "macos")]
    pub fn cursor(w: &slint::Window) -> Option<(i32, i32)> {
        use objc2_core_graphics::CGEvent;
        let ev = CGEvent::new(None)?;
        let p = CGEvent::location(Some(&ev));
        let mons = monitors(w);
        let scale = mons
            .iter()
            .map(|m| (m, m.dpi as f64 / 96.0))
            .find(|(m, sc)| {
                let (l, t) = (m.left as f64 / sc, m.top as f64 / sc);
                let (r, b) = (m.right as f64 / sc, m.bottom as f64 / sc);
                p.x >= l && p.x < r && p.y >= t && p.y < b
            })
            .map(|(_, sc)| sc)
            .unwrap_or(1.0);
        Some(((p.x * scale) as i32, (p.y * scale) as i32))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn cursor(_w: &slint::Window) -> Option<(i32, i32)> {
        None
    }
}

pub use imp::{click_through, cursor, dock, hide_from_taskbar, monitors, undock};

/// The monitor to dock on: the preferred one if connected, else the primary.
pub fn pick(w: &slint::Window, preferred: &str) -> Option<Monitor> {
    let all = monitors(w);
    all.iter()
        .find(|m| m.id == preferred)
        .or_else(|| all.iter().find(|m| m.primary))
        .or_else(|| all.first())
        .cloned()
}
