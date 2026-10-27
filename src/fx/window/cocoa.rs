//! Plug-in windows on macOS: an `NSWindow` whose content view the plug-in
//! fills. AppKit runs on the main thread, where the window's event loop
//! already handles these windows' events too.

use std::ffi::{CString, c_void};

use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, msg_send};

use crate::fx::Result;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

// SAFETY: matches `CGPoint` on 64-bit systems.
unsafe impl Encode for Point {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

// SAFETY: matches `CGSize` on 64-bit systems.
unsafe impl Encode for Size {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

// SAFETY: matches `CGRect` on 64-bit systems.
unsafe impl Encode for Rect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[Point::ENCODING, Size::ENCODING]);
}

/// Titled, closable, miniaturizable, resizable.
const STYLE: usize = 1 | 2 | 4 | 8;
/// `NSBackingStoreBuffered`.
const BUFFERED: usize = 2;

pub struct PluginWindow {
    window: Retained<AnyObject>,
    content: Retained<AnyObject>,
}

fn size_of(width: i32, height: i32) -> Size {
    Size {
        width: f64::from(width.max(160)),
        height: f64::from(height.max(120)),
    }
}

impl PluginWindow {
    pub fn open(title: &str, width: i32, height: i32) -> Result<Self> {
        if MainThreadMarker::new().is_none() {
            return Err("plug-in windows open on the main thread".into());
        }
        let class = AnyClass::get(c"NSWindow").ok_or("no AppKit")?;
        let rect = Rect {
            origin: Point::default(),
            size: size_of(width, height),
        };
        let title = CString::new(title.replace('\0', "")).unwrap_or_default();
        // SAFETY: documented AppKit calls on the main thread.
        unsafe {
            let allocated: Allocated<AnyObject> = msg_send![class, alloc];
            let window: Option<Retained<AnyObject>> = msg_send![
                allocated,
                initWithContentRect: rect,
                styleMask: STYLE,
                backing: BUFFERED,
                defer: false
            ];
            let window = window.ok_or("the window did not open")?;
            // Closing hides it; it goes when we drop it.
            let _: () = msg_send![&*window, setReleasedWhenClosed: false];
            let string = AnyClass::get(c"NSString").ok_or("no Foundation")?;
            let title: Option<Retained<AnyObject>> =
                msg_send![string, stringWithUTF8String: title.as_ptr()];
            if let Some(title) = title {
                let _: () = msg_send![&*window, setTitle: &*title];
            }
            let _: () = msg_send![&*window, center];
            let content: Option<Retained<AnyObject>> = msg_send![&*window, contentView];
            let content = content.ok_or("the window has no content")?;
            Ok(Self { window, content })
        }
    }

    /// A window for a view the plug-in made itself (Audio Units), sized to
    /// it.
    pub fn open_with(title: &str, view: &AnyObject) -> Result<Self> {
        // SAFETY: a view, on the main thread (checked by `open`).
        let frame: Rect = unsafe { msg_send![view, frame] };
        let window = Self::open(title, frame.size.width as i32, frame.size.height as i32)?;
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![view, setFrameOrigin: Point::default()];
            let _: () = msg_send![&*window.content, addSubview: view];
        }
        window.fit(view);
        Ok(window)
    }

    /// Follows the size of a view the plug-in resizes itself.
    pub fn fit(&self, view: &AnyObject) {
        // SAFETY: views of this window, on the main thread.
        unsafe {
            let wanted: Rect = msg_send![view, frame];
            let current: Rect = msg_send![&*self.content, frame];
            if wanted.size != current.size && wanted.size.width > 0.0 {
                let _: () = msg_send![&*self.window, setContentSize: wanted.size];
            }
        }
    }

    pub fn show(&self) {
        // SAFETY: documented AppKit calls on the main thread.
        unsafe {
            let _: () =
                msg_send![&*self.window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
            if let Some(app) = AnyClass::get(c"NSApplication") {
                let app: Option<Retained<AnyObject>> = msg_send![app, sharedApplication];
                if let Some(app) = app {
                    let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
                }
            }
        }
    }

    pub fn resize(&self, width: i32, height: i32) {
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![&*self.window, setContentSize: size_of(width, height)];
        }
    }

    /// The content view, for the plug-in.
    pub fn handle(&self) -> *mut c_void {
        Retained::as_ptr(&self.content).cast_mut().cast()
    }

    /// Closed from its title bar: hidden, and not just in the Dock.
    pub fn closed_by_user(&self) -> bool {
        // SAFETY: as above.
        unsafe {
            let visible: bool = msg_send![&*self.window, isVisible];
            let minimized: bool = msg_send![&*self.window, isMiniaturized];
            !visible && !minimized
        }
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        // The plug-in has let go of the view by now.
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![&*self.window, close];
        }
    }
}

/// The size of a plug-in's view by its handle (an `NSView`).
pub fn client_size(handle: *mut c_void) -> (i32, i32) {
    let Some(view) = (unsafe { handle.cast::<AnyObject>().as_ref() }) else {
        return (0, 0);
    };
    // SAFETY: the content view of one of our windows.
    let frame: Rect = unsafe { msg_send![view, frame] };
    (frame.size.width as i32, frame.size.height as i32)
}

/// Resizes the window around a view by the view's handle.
pub fn resize_handle(handle: *mut c_void, width: i32, height: i32) {
    // SAFETY: the content view of one of our windows, or null.
    let Some(view) = (unsafe { handle.cast::<AnyObject>().as_ref() }) else {
        return;
    };
    // SAFETY: as above.
    unsafe {
        let window: Option<Retained<AnyObject>> = msg_send![view, window];
        if let Some(window) = window {
            let _: () = msg_send![&*window, setContentSize: size_of(width, height)];
        }
    }
}
