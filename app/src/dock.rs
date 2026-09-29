//! Edge docking of the buddy list (PRD DK-1, DK-5, DK-6).
//!
//! The buddy list is always docked and always on top.
//! Windows: a real AppBar (SHAppBarMessage) that reserves screen space on the
//! chosen monitor, so maximised windows there stay out of the strip — like ICQ.
//! Other platforms: not yet in the prototype (X11 struts and Wayland
//! layer-shell are planned).

/// A display, as far as docking cares.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// Stable-ish id (Windows device name, e.g. `\\.\DISPLAY2`).
    pub id: String,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
    /// Effective DPI (96 = 100 %).
    pub dpi: u32,
}

#[cfg(windows)]
mod imp {
    use super::Monitor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::Shell::{
        SHAppBarMessage, ABE_LEFT, ABE_RIGHT, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, APPBARDATA,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{MONITORINFOF_PRIMARY, WM_USER};

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

    /// All monitors, ordered left to right.
    pub fn monitors() -> Vec<Monitor> {
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

    /// Register as an AppBar on the left or right edge of `mon` and move the
    /// window into the reserved strip. Other windows on that monitor,
    /// including maximised ones, are laid out in the remaining work area.
    pub fn dock(w: &slint::Window, width_logical: f32, left: bool, mon: &Monitor) -> bool {
        let Some(h) = hwnd(w) else { return false };
        let width = (width_logical * mon.dpi as f32 / 96.0).round() as i32;
        unsafe {
            let mut abd = data(h, left);
            SHAppBarMessage(ABM_REMOVE, &mut abd);
            SHAppBarMessage(ABM_NEW, &mut abd);
            abd.rc = if left {
                RECT { left: mon.left, top: mon.top, right: mon.left + width, bottom: mon.bottom }
            } else {
                RECT { left: mon.right - width, top: mon.top, right: mon.right, bottom: mon.bottom }
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
    use super::Monitor;
    pub fn monitors() -> Vec<Monitor> {
        vec![]
    }
    pub fn dock(_w: &slint::Window, _width: f32, _left: bool, _mon: &Monitor) -> bool {
        false
    }
    pub fn undock(_w: &slint::Window) {}
    pub const SUPPORTED: bool = false;
}

pub use imp::{dock, monitors, undock, SUPPORTED};

/// The monitor to dock on: the preferred one if connected, else the primary.
pub fn pick(preferred: &str) -> Option<Monitor> {
    let all = monitors();
    all.iter()
        .find(|m| m.id == preferred)
        .or_else(|| all.iter().find(|m| m.primary))
        .or_else(|| all.first())
        .cloned()
}
