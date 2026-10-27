//! The `IEditController`, shared by everything on the window's thread (the
//! parameter list, the editor, the state) and taken down once, after all of
//! them. Taking it down matters: a controller this host made and connected
//! holds its component and is held by it until disconnected.

use std::rc::Rc;
use std::sync::Arc;

use vst3::ComPtr;
use vst3::Steinberg::Vst::{
    IComponent, IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
    IEditControllerTrait,
};
use vst3::Steinberg::{IBStream, IPluginBaseTrait, kResultOk};

use super::host::MemoryStream;
use super::module::Module;
use crate::fx::graph::PluginState;

pub struct Controller {
    controller: ComPtr<IEditController>,
    component: ComPtr<IComponent>,
    /// A separate object rather than the component itself: this host made
    /// and connected it, so this host takes it down.
    separate: bool,
    /// Outlives both pointers: fields drop in order.
    _module: Arc<Module>,
}

impl Controller {
    pub fn new(
        controller: ComPtr<IEditController>,
        component: ComPtr<IComponent>,
        separate: bool,
        module: Arc<Module>,
    ) -> Rc<Self> {
        Rc::new(Self {
            controller,
            component,
            separate,
            _module: module,
        })
    }

    pub fn get(&self) -> &ComPtr<IEditController> {
        &self.controller
    }
}

/// Connects (or disconnects) the component's and controller's message
/// channels, which carry meters and anything else parameters do not.
pub fn link(component: &ComPtr<IComponent>, controller: &ComPtr<IEditController>, on: bool) {
    let (Some(from), Some(to)) = (
        component.cast::<IConnectionPoint>(),
        controller.cast::<IConnectionPoint>(),
    ) else {
        return;
    };
    // SAFETY: both points belong to live objects of one plug-in.
    unsafe {
        if on {
            from.connect(to.as_ptr());
            to.connect(from.as_ptr());
        } else {
            from.disconnect(to.as_ptr());
            to.disconnect(from.as_ptr());
        }
    }
}

/// Tells a separate controller the component's state, which it has no
/// other way to learn.
pub fn share_state(component: &ComPtr<IComponent>, controller: &ComPtr<IEditController>) {
    let stream = MemoryStream::new(Vec::new());
    let Some(ptr) = stream.to_com_ptr::<IBStream>() else {
        return;
    };
    // SAFETY: the stream lives through both calls.
    unsafe {
        if component.getState(ptr.as_ptr()) != kResultOk || stream.is_empty() {
            return;
        }
        stream.rewind();
        controller.setComponentState(ptr.as_ptr());
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        // SAFETY: the last reference: the editor and the formatter are gone;
        // undoes what `instance::create` did, in reverse.
        unsafe {
            self.controller.setComponentHandler(std::ptr::null_mut());
            if self.separate {
                link(&self.component, &self.controller, false);
                self.controller.terminate();
            }
        }
    }
}

/// The component's state, where the sound's settings live; read and written
/// from the window's thread, as hosts do while playing.
pub struct Vst3State {
    pub component: ComPtr<IComponent>,
    /// Learns of a restored state after the component.
    pub controller: Option<Rc<Controller>>,
    pub _module: Arc<Module>,
}

impl PluginState for Vst3State {
    fn save(&self) -> Option<Vec<u8>> {
        let stream = MemoryStream::new(Vec::new());
        let ptr = stream.to_com_ptr::<IBStream>()?;
        // SAFETY: the stream lives through the call.
        let ok = unsafe { self.component.getState(ptr.as_ptr()) } == kResultOk;
        (ok && !stream.is_empty()).then(|| stream.bytes())
    }

    fn restore(&self, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        let stream = MemoryStream::new(data.to_vec());
        let Some(ptr) = stream.to_com_ptr::<IBStream>() else {
            return false;
        };
        // SAFETY: the stream lives through the calls.
        unsafe {
            if self.component.setState(ptr.as_ptr()) != kResultOk {
                return false;
            }
            if let Some(controller) = self.controller.as_ref().filter(|c| c.separate) {
                stream.rewind();
                controller.controller.setComponentState(ptr.as_ptr());
            }
        }
        true
    }
}
