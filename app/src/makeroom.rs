//! macOS cannot reserve screen space for the bar, so "make room" shrinks or moves
//! other apps' windows that reach under it, through the Accessibility API (the
//! way window managers like Rectangle do it). Needs RogueIM under Privacy &
//! Security → Accessibility; without it nothing happens.

use std::ffi::c_void;

use crate::dock::Monitor;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type AXUIElementRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    w: f64,
    h: f64,
}

const AX_POINT: u32 = 1;
const AX_SIZE: u32 = 2;
const UTF8: u32 = 0x0800_0100;
const NUMBER_I32: isize = 3;
const ON_SCREEN_ONLY: u32 = 1;
const EXCLUDE_DESKTOP: u32 = 16;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(el: AXUIElementRef, attr: CFStringRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(el: AXUIElementRef, attr: CFStringRef, value: CFTypeRef) -> i32;
    fn AXValueCreate(kind: u32, value: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(v: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> CFArrayRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: isize, enc: u32, external: bool) -> CFStringRef;
    fn CFArrayGetCount(a: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFArrayRef, i: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> bool;
    fn CFBooleanGetValue(b: CFTypeRef) -> bool;
    fn CFRelease(v: CFTypeRef);
}

/// A CFString that is released when dropped.
struct Str(CFStringRef);

impl Str {
    fn new(s: &str) -> Str {
        // SAFETY: valid UTF-8 bytes with their length.
        Str(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, false) })
    }
}

impl Drop for Str {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created by us, released once.
            unsafe { CFRelease(self.0) }
        }
    }
}

/// Process ids of apps with normal windows on screen, except RogueIM itself.
fn window_pids() -> Vec<i32> {
    let me = std::process::id() as i32;
    let (k_pid, k_layer) = (Str::new("kCGWindowOwnerPID"), Str::new("kCGWindowLayer"));
    let mut pids = vec![];
    // SAFETY: CoreFoundation calls on a list we own and release.
    unsafe {
        let list = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if list.is_null() {
            return pids;
        }
        for i in 0..CFArrayGetCount(list) {
            let d = CFArrayGetValueAtIndex(list, i);
            let (mut pid, mut layer) = (0i32, 1i32);
            let p = CFDictionaryGetValue(d, k_pid.0);
            let l = CFDictionaryGetValue(d, k_layer.0);
            if p.is_null() || l.is_null() {
                continue;
            }
            CFNumberGetValue(p, NUMBER_I32, &mut pid as *mut i32 as *mut c_void);
            CFNumberGetValue(l, NUMBER_I32, &mut layer as *mut i32 as *mut c_void);
            if layer == 0 && pid != me && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        CFRelease(list);
    }
    pids
}

/// Read a point or size attribute.
unsafe fn get<T: Default>(el: AXUIElementRef, attr: &Str, kind: u32) -> Option<T> {
    let mut v: CFTypeRef = std::ptr::null();
    if AXUIElementCopyAttributeValue(el, attr.0, &mut v) != 0 || v.is_null() {
        return None;
    }
    let mut out = T::default();
    let ok = AXValueGetValue(v, kind, &mut out as *mut T as *mut c_void);
    CFRelease(v);
    ok.then_some(out)
}

unsafe fn flag(el: AXUIElementRef, attr: &Str) -> bool {
    let mut v: CFTypeRef = std::ptr::null();
    if AXUIElementCopyAttributeValue(el, attr.0, &mut v) != 0 || v.is_null() {
        return false;
    }
    let b = CFBooleanGetValue(v);
    CFRelease(v);
    b
}

unsafe fn set<T>(el: AXUIElementRef, attr: &Str, kind: u32, value: &T) {
    let v = AXValueCreate(kind, value as *const T as *const c_void);
    if !v.is_null() {
        AXUIElementSetAttributeValue(el, attr.0, v);
        CFRelease(v);
    }
}

pub fn trusted() -> bool {
    // SAFETY: no arguments.
    unsafe { AXIsProcessTrusted() }
}

/// Fit every window on `mon` beside the bar (`bar_px` wide, physical pixels).
pub fn make_room(mon: &Monitor, bar_px: i32, left: bool) {
    if !trusted() {
        return;
    }
    // Accessibility works in points; monitors are in physical pixels.
    let sc = mon.dpi as f64 / 96.0;
    let (ml, mt, mr, mb) = (mon.left as f64 / sc, mon.top as f64 / sc, mon.right as f64 / sc, mon.bottom as f64 / sc);
    let bar = bar_px as f64 / sc;
    let (free_l, free_r) = if left { (ml + bar, mr) } else { (ml, mr - bar) };
    let windows = Str::new("AXWindows");
    let (pos_a, size_a) = (Str::new("AXPosition"), Str::new("AXSize"));
    let (min_a, full_a) = (Str::new("AXMinimized"), Str::new("AXFullScreen"));
    for pid in window_pids() {
        // SAFETY: Accessibility objects are created, used and released here.
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                continue;
            }
            let mut list: CFTypeRef = std::ptr::null();
            if AXUIElementCopyAttributeValue(app, windows.0, &mut list) == 0 && !list.is_null() {
                for i in 0..CFArrayGetCount(list) {
                    let w = CFArrayGetValueAtIndex(list, i);
                    if flag(w, &min_a) || flag(w, &full_a) {
                        continue;
                    }
                    let (Some(p), Some(s)) = (get::<Point>(w, &pos_a, AX_POINT), get::<Size>(w, &size_a, AX_SIZE)) else { continue };
                    // Only windows whose middle is on this monitor.
                    let (cx, cy) = (p.x + s.w / 2.0, p.y + s.h / 2.0);
                    if cx < ml || cx >= mr || cy < mt || cy >= mb {
                        continue;
                    }
                    if p.x >= free_l - 0.5 && p.x + s.w <= free_r + 0.5 {
                        continue;
                    }
                    let room = free_r - free_l;
                    let x = p.x.clamp(free_l, (free_r - s.w).max(free_l));
                    let right = (p.x + s.w).min(free_r).max(x + s.w.min(room));
                    let width = (right - x).min(room);
                    set(w, &pos_a, AX_POINT, &Point { x, y: p.y });
                    if (width - s.w).abs() > 0.5 {
                        set(w, &size_a, AX_SIZE, &Size { w: width, h: s.h });
                    }
                }
                CFRelease(list);
            }
            CFRelease(app);
        }
    }
}
