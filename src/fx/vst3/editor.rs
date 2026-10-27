//! The plug-in's own window (`IPlugView`). The order is strict, and plug-ins
//! crash when it is broken: the frame before `attached`, `removed` before
//! the window goes.

#![allow(non_snake_case)]

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use vst3::Steinberg::Vst::IEditControllerTrait;
use vst3::Steinberg::{
    IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, ViewRect, kResultOk,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::controller::Controller;
use crate::fx::Result;
use crate::fx::graph::PluginEditor;
use crate::fx::window::{self, PLATFORM_TYPE, PluginWindow};

pub fn new(controller: Rc<Controller>) -> Box<dyn PluginEditor> {
    Box::new(Vst3Editor {
        controller,
        view: None,
        window: None,
        frame: None,
    })
}

struct Vst3Editor {
    /// Keeps the module, which must outlive the view.
    controller: Rc<Controller>,
    view: Option<ComPtr<IPlugView>>,
    window: Option<PluginWindow>,
    /// Lives while the view is attached to it.
    frame: Option<ComWrapper<HostFrame>>,
}

/// What a plug-in asks to resize its window through. Set before `attached`:
/// editors without one think they are not embedded.
struct HostFrame {
    window: *mut c_void,
    /// The size the view knows of; a different one means the user resized.
    size: Cell<(i32, i32)>,
}

impl Class for HostFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for HostFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, newSize: *mut ViewRect) -> i32 {
        if newSize.is_null() {
            return kResultOk;
        }
        // SAFETY: the plug-in's rectangle.
        let rect = unsafe { *newSize };
        window::resize_handle(self.window, rect.right - rect.left, rect.bottom - rect.top);
        self.size.set(window::client_size(self.window));
        // The host resizes, then tells the view.
        // SAFETY: the plug-in's own view, alive for the call.
        if let Some(view) = unsafe { ComRef::from_raw(view) } {
            unsafe { view.onSize(newSize) };
        }
        kResultOk
    }
}

fn size_of(rect: &ViewRect) -> (i32, i32) {
    (rect.right - rect.left, rect.bottom - rect.top)
}

impl PluginEditor for Vst3Editor {
    fn open(&mut self, title: &str) -> Result<()> {
        if let Some(window) = &self.window {
            window.show();
            return Ok(());
        }
        // SAFETY: a live controller; "editor" is the spec's name.
        let view =
            unsafe { ComPtr::from_raw(self.controller.get().createView(c"editor".as_ptr())) }
                .ok_or("the plug-in has no window of its own")?;
        // SAFETY: a live view, a NUL-terminated type.
        if unsafe { view.isPlatformTypeSupported(PLATFORM_TYPE.as_ptr().cast()) } != kResultOk {
            return Err("the plug-in's window does not work on this system".into());
        }
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        // SAFETY: an output rectangle.
        unsafe { view.getSize(&mut rect) };
        let (width, height) = size_of(&rect);
        let window = PluginWindow::open(title, width, height)?;
        let frame = ComWrapper::new(HostFrame {
            window: window.handle(),
            size: Cell::new(window::client_size(window.handle())),
        });
        if let Some(ptr) = frame.to_com_ptr::<IPlugFrame>() {
            // SAFETY: the frame lives as long as the editor.
            unsafe { view.setFrame(ptr.as_ptr()) };
        }
        // SAFETY: the window outlives the view's attachment, see `close`.
        let attached = unsafe { view.attached(window.handle(), PLATFORM_TYPE.as_ptr().cast()) };
        if attached != kResultOk {
            // SAFETY: the plug-in keeps the frame as a bare pointer.
            unsafe { view.setFrame(std::ptr::null_mut()) };
            return Err(format!("the plug-in's window did not open ({attached})"));
        }
        // Some editors know their size only once attached, and do not call
        // `resizeView` for it.
        let mut settled = rect;
        // SAFETY: an attached view, an output rectangle.
        let asked = unsafe { view.getSize(&mut settled) } == kResultOk;
        let (mut width, mut height) = (width, height);
        let (settled_width, settled_height) = size_of(&settled);
        if asked
            && settled_width > 0
            && settled_height > 0
            && (settled_width, settled_height) != (width, height)
        {
            (width, height) = (settled_width, settled_height);
            window.resize(width, height);
            frame.size.set(window::client_size(window.handle()));
        }
        // Some editors lay themselves out only on `onSize`.
        let mut actual = ViewRect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        // SAFETY: an attached view.
        unsafe { view.onSize(&mut actual) };
        window.show();
        self.view = Some(view);
        self.window = Some(window);
        self.frame = Some(frame);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(view) = self.view.take() {
            // SAFETY: pairs with `attached`; the frame goes with the window
            // below, and plug-ins keep it as a bare pointer.
            unsafe {
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
        }
        self.window = None;
        self.frame = None;
    }

    fn is_open(&self) -> bool {
        self.window.is_some()
    }

    fn poll(&mut self) {
        let Some(window) = &self.window else { return };
        if window.closed_by_user() {
            return self.close();
        }
        self.follow_user_resize();
    }
}

impl Vst3Editor {
    /// Tells the view the size the user dragged the window to, or puts the
    /// window back when the view does not resize.
    fn follow_user_resize(&mut self) {
        let (Some(view), Some(window), Some(frame)) = (&self.view, &self.window, &self.frame)
        else {
            return;
        };
        let actual = window::client_size(window.handle());
        let known = frame.size.get();
        if actual == known || actual.0 <= 0 || actual.1 <= 0 {
            return;
        }
        // SAFETY: an attached view.
        if unsafe { view.canResize() } != kResultOk {
            window.resize(known.0, known.1);
            frame.size.set(window::client_size(window.handle()));
            return;
        }
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: actual.0,
            bottom: actual.1,
        };
        // SAFETY: an attached view, an in-out rectangle.
        unsafe { view.checkSizeConstraint(&mut rect) };
        if size_of(&rect) != actual {
            window.resize(size_of(&rect).0, size_of(&rect).1);
        }
        frame.size.set(window::client_size(window.handle()));
        // SAFETY: as above.
        unsafe { view.onSize(&mut rect) };
    }
}

impl Drop for Vst3Editor {
    fn drop(&mut self) {
        self.close();
    }
}
