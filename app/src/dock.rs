//! Edge docking of the buddy list (PRD DK-1).
//!
//! The buddy list is always docked and always on top.
//! Windows: a real AppBar (SHAppBarMessage) that reserves screen space, so
//! maximised windows shrink — like ICQ. Other platforms: not yet in the
//! prototype (X11 struts and Wayland layer-shell are planned).

#[cfg(windows)]
mod imp {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::UI::Shell::{
        SHAppBarMessage, ABE_LEFT, ABE_RIGHT, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, WM_USER};

    fn hwnd(w: &slint::Window) -> Option<HWND> {
        match w.window_handle().window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
            _ => None,
        }
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

    /// Register as an AppBar on the left or right edge of the primary screen
    /// and move the window into the reserved strip. Other windows, including
    /// maximised ones, are laid out in the remaining work area.
    pub fn dock(w: &slint::Window, width_logical: f32, left: bool) -> bool {
        let Some(h) = hwnd(w) else { return false };
        let width = (width_logical * w.scale_factor()).round() as i32;
        unsafe {
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let mut abd = data(h, left);
            SHAppBarMessage(ABM_NEW, &mut abd);
            abd.rc = if left {
                RECT { left: 0, top: 0, right: width, bottom: sh }
            } else {
                RECT { left: sw - width, top: 0, right: sw, bottom: sh }
            };
            SHAppBarMessage(ABM_QUERYPOS, &mut abd);
            // QUERYPOS may shift the rect (e.g. a side taskbar); keep our width.
            if left {
                abd.rc.right = abd.rc.left + width;
            } else {
                abd.rc.left = abd.rc.right - width;
            }
            SHAppBarMessage(ABM_SETPOS, &mut abd);
            w.set_position(slint::PhysicalPosition::new(abd.rc.left, abd.rc.top));
            w.set_size(slint::PhysicalSize::new(width as u32, (abd.rc.bottom - abd.rc.top) as u32));
        }
        true
    }

    pub fn undock(w: &slint::Window) {
        let Some(h) = hwnd(w) else { return };
        unsafe {
            let mut abd = data(h, false);
            SHAppBarMessage(ABM_REMOVE, &mut abd);
        }
    }

    pub const SUPPORTED: bool = true;
}

#[cfg(not(windows))]
mod imp {
    pub fn dock(_w: &slint::Window, _width: f32, _left: bool) -> bool {
        false
    }
    pub fn undock(_w: &slint::Window) {}
    pub const SUPPORTED: bool = false;
}

pub use imp::{dock, undock, SUPPORTED};
