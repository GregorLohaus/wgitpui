//! Window chrome. On macOS the title bar is made transparent with a full-size content view so
//! our own top bar runs edge to edge, and the native traffic lights are re-centered inside it.
//! This talks to AppKit through the raw Objective-C runtime (libobjc), so it needs no crates.

use winit::window::{Window, WindowAttributes};

/// Where the traffic lights end (measured: close/minimize/zoom at x = 20, 43, 66, each 14pt wide).
pub const LEFT_INSET: f32 = if cfg!(target_os = "macos") { 80.0 } else { 0.0 };

pub fn window_attributes(attrs: WindowAttributes) -> WindowAttributes {
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::WindowAttributesExtMacOS;
        attrs.with_titlebar_transparent(true).with_title_hidden(true).with_fullsize_content_view(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        attrs
    }
}

/// Re-centers the traffic lights on a bar of `bar_h` logical points. AppKit resets their position
/// on resize and fullscreen transitions, so this is called on every redraw (it's a few messages).
pub fn place_traffic_lights(window: &Window, bar_h: f32) {
    #[cfg(target_os = "macos")]
    {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if window.fullscreen().is_some() {
            return;
        }
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::AppKit(h) = h.as_raw() {
                // SAFETY: called on the main thread with a live NSView owned by winit.
                unsafe { mac::place(h.ns_view.as_ptr(), 20.0, bar_h as f64 * 0.5) };
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (window, bar_h);
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void, CString};

    type Id = *mut c_void;
    type Sel = *const c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        w: f64,
        h: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Size,
    }

    #[link(name = "objc", kind = "dylib")]
    unsafe extern "C" {
        fn objc_msgSend();
        #[cfg(target_arch = "x86_64")]
        fn objc_msgSend_stret();
        fn sel_registerName(name: *const c_char) -> Sel;
    }

    unsafe fn sel(name: &str) -> Sel {
        let c = CString::new(name).unwrap();
        unsafe { sel_registerName(c.as_ptr()) }
    }

    fn msg() -> *const () {
        objc_msgSend as unsafe extern "C" fn() as *const ()
    }

    unsafe fn send_id(obj: Id, name: &str) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(msg()) };
        unsafe { f(obj, sel(name)) }
    }

    unsafe fn send_id_arg(obj: Id, name: &str, arg: usize) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, usize) -> Id = unsafe { std::mem::transmute(msg()) };
        unsafe { f(obj, sel(name), arg) }
    }

    unsafe fn frame(obj: Id) -> Rect {
        #[cfg(target_arch = "aarch64")]
        {
            let f: unsafe extern "C" fn(Id, Sel) -> Rect = unsafe { std::mem::transmute(msg()) };
            unsafe { f(obj, sel("frame")) }
        }
        #[cfg(target_arch = "x86_64")]
        {
            let mut out = Rect { origin: Point { x: 0.0, y: 0.0 }, size: Size { w: 0.0, h: 0.0 } };
            let f: unsafe extern "C" fn(*mut Rect, Id, Sel) =
                unsafe { std::mem::transmute(objc_msgSend_stret as unsafe extern "C" fn() as *const ()) };
            unsafe { f(&mut out, obj, sel("frame")) };
            out
        }
    }

    unsafe fn set_frame(obj: Id, r: Rect) {
        let f: unsafe extern "C" fn(Id, Sel, Rect) = unsafe { std::mem::transmute(msg()) };
        unsafe { f(obj, sel("setFrame:"), r) }
    }

    unsafe fn set_frame_origin(obj: Id, p: Point) {
        let f: unsafe extern "C" fn(Id, Sel, Point) = unsafe { std::mem::transmute(msg()) };
        unsafe { f(obj, sel("setFrameOrigin:"), p) }
    }

    /// Positions the close/minimize/zoom buttons with the first one at `x` and their centers at
    /// `center_y` points from the top of the window.
    pub unsafe fn place(ns_view: *mut c_void, x: f64, center_y: f64) {
        unsafe {
            let window = send_id(ns_view, "window");
            if window.is_null() {
                return;
            }
            // NSWindowButton: 0 = close, 1 = miniaturize, 2 = zoom.
            let buttons = [0usize, 1, 2].map(|b| send_id_arg(window, "standardWindowButton:", b));
            if buttons.iter().any(|b| b.is_null()) {
                return;
            }
            let container = send_id(send_id(buttons[0], "superview"), "superview");
            if container.is_null() {
                return;
            }
            let bh = frame(buttons[0]).size.h;
            // The container spans the whole bar; buttons are placed explicitly inside it
            // (AppKit keeps its own y offset otherwise). Container coordinates are bottom-up.
            let bar_h = center_y * 2.0;
            let mut cr = frame(container);
            let wr = frame(window);
            cr.size.h = bar_h;
            cr.origin.y = wr.size.h - bar_h;
            set_frame(container, cr);
            let spacing = frame(buttons[1]).origin.x - frame(buttons[0]).origin.x;
            for (i, b) in buttons.iter().enumerate() {
                let mut r = frame(*b);
                r.origin.x = x + i as f64 * spacing;
                r.origin.y = bar_h - center_y - bh * 0.5;
                set_frame_origin(*b, r.origin);
            }
        }
    }
}
