//! The objects a VST3 host hands its plug-ins: the host application (which
//! also makes messages for them), attribute lists, a stream in memory for
//! states, and the component handler that hears edits in the plug-in's own
//! window.

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use vst3::Steinberg::Vst::{
    IAttributeList, IAttributeList_iid, IAttributeListTrait, IComponentHandler,
    IComponentHandlerTrait, IHostApplication, IHostApplicationTrait, IMessage, IMessage_iid,
    IMessageTrait, ParamID, ParamValue, RestartFlags_, String128,
};
use vst3::Steinberg::{
    FIDString, IBStream, IBStreamTrait, TUID, char16, kInvalidArgument, kNotImplemented,
    kResultFalse, kResultOk, tresult,
};
use vst3::{Class, ComPtr, ComWrapper};

use crate::fx::graph::{EditNotifications, ParamBridge, ParamChange, ParamInbox};

/// Given to plug-ins in `initialize`; without it some refuse to start, and
/// without its messages a component cannot talk to its controller (meters
/// stand still).
pub struct HostApplication {
    name: Vec<u16>,
}

impl HostApplication {
    pub fn new(name: &str) -> ComWrapper<Self> {
        let mut name: Vec<u16> = name.encode_utf16().take(127).collect();
        name.push(0);
        ComWrapper::new(Self { name })
    }
}

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

/// Hands a new object over to the plug-in, which owns the reference.
///
/// # Safety
/// `obj` must be valid for writing.
unsafe fn hand_over<I: vst3::Interface>(ptr: Option<ComPtr<I>>, obj: *mut *mut c_void) -> tresult {
    let Some(ptr) = ptr else {
        return kNotImplemented;
    };
    // SAFETY: passed on to the caller.
    unsafe { *obj = ptr.as_ptr().cast() };
    std::mem::forget(ptr);
    kResultOk
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the caller's 128 UTF-16 units.
        let name = unsafe { &mut *name };
        name.fill(0);
        for (to, &from) in name.iter_mut().zip(&self.name) {
            *to = from as _;
        }
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        _iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked above; a TUID is 16 bytes.
        let requested = unsafe { *cid };
        if requested == IMessage_iid {
            // SAFETY: `obj` checked above.
            unsafe { hand_over(Message::new().to_com_ptr::<IMessage>(), obj) }
        } else if requested == IAttributeList_iid {
            // SAFETY: as above.
            unsafe { hand_over(AttributeList::new().to_com_ptr::<IAttributeList>(), obj) }
        } else {
            kNotImplemented
        }
    }
}

enum Attribute {
    Int(i64),
    Float(f64),
    /// UTF-16 without the terminating zero.
    Text(Vec<u16>),
    Binary(Vec<u8>),
}

#[derive(Default)]
pub struct AttributeList {
    values: RefCell<HashMap<String, Attribute>>,
}

impl AttributeList {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self::default())
    }

    fn set(&self, id: FIDString, value: Attribute) -> tresult {
        match key(id) {
            Some(key) => {
                self.values.borrow_mut().insert(key, value);
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

/// An attribute's key, copied: it lives only for the call.
fn key(id: FIDString) -> Option<String> {
    if id.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated string, as the interface promises.
    unsafe { CStr::from_ptr(id) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: FIDString, value: i64) -> tresult {
        self.set(id, Attribute::Int(value))
    }

    unsafe fn getInt(&self, id: FIDString, value: *mut i64) -> tresult {
        let (Some(k), false) = (key(id), value.is_null()) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&k) {
            // SAFETY: checked above.
            Some(Attribute::Int(v)) => unsafe { *value = *v },
            _ => return kResultFalse,
        }
        kResultOk
    }

    unsafe fn setFloat(&self, id: FIDString, value: f64) -> tresult {
        self.set(id, Attribute::Float(value))
    }

    unsafe fn getFloat(&self, id: FIDString, value: *mut f64) -> tresult {
        let (Some(k), false) = (key(id), value.is_null()) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&k) {
            // SAFETY: checked above.
            Some(Attribute::Float(v)) => unsafe { *value = *v },
            _ => return kResultFalse,
        }
        kResultOk
    }

    unsafe fn setString(&self, id: FIDString, string: *const char16) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a NUL-terminated UTF-16 string from the plug-in.
        let text = unsafe {
            let mut length = 0;
            while *string.add(length) != 0 {
                length += 1;
            }
            std::slice::from_raw_parts(string.cast::<u16>(), length).to_vec()
        };
        self.set(id, Attribute::Text(text))
    }

    unsafe fn getString(&self, id: FIDString, string: *mut char16, sizeInBytes: u32) -> tresult {
        let (Some(k), false) = (key(id), string.is_null()) else {
            return kInvalidArgument;
        };
        let values = self.values.borrow();
        let Some(Attribute::Text(text)) = values.get(&k) else {
            return kResultFalse;
        };
        // Room for the terminating zero.
        let count = text.len().min((sizeInBytes as usize / 2).saturating_sub(1));
        // SAFETY: the plug-in's buffer holds `sizeInBytes` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), string.cast::<u16>(), count);
            *string.add(count) = 0;
        }
        kResultOk
    }

    unsafe fn setBinary(&self, id: FIDString, data: *const c_void, sizeInBytes: u32) -> tresult {
        if data.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the plug-in promises `sizeInBytes` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), sizeInBytes as usize) };
        self.set(id, Attribute::Binary(bytes.to_vec()))
    }

    unsafe fn getBinary(
        &self,
        id: FIDString,
        data: *mut *const c_void,
        sizeInBytes: *mut u32,
    ) -> tresult {
        let (Some(k), false, false) = (key(id), data.is_null(), sizeInBytes.is_null()) else {
            return kInvalidArgument;
        };
        let values = self.values.borrow();
        let Some(Attribute::Binary(bytes)) = values.get(&k) else {
            return kResultFalse;
        };
        // The list keeps the bytes, as the interface says.
        // SAFETY: checked above.
        unsafe {
            *data = bytes.as_ptr().cast();
            *sizeInBytes = bytes.len() as u32;
        }
        kResultOk
    }
}

/// A message between a component and its controller.
pub struct Message {
    id: RefCell<CString>,
    /// `getAttributes` lends the list without a reference.
    attributes: Option<ComPtr<IAttributeList>>,
}

impl Message {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            id: RefCell::new(CString::default()),
            attributes: AttributeList::new().to_com_ptr::<IAttributeList>(),
        })
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> *const c_char {
        self.id.borrow().as_ptr()
    }

    unsafe fn setMessageID(&self, id: *const c_char) {
        if !id.is_null() {
            // SAFETY: a NUL-terminated string from the plug-in.
            *self.id.borrow_mut() = unsafe { CStr::from_ptr(id) }.to_owned();
        }
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes
            .as_ref()
            .map_or(std::ptr::null_mut(), ComPtr::as_ptr)
    }
}

/// A stream in memory, for states.
pub struct MemoryStream {
    data: RefCell<Vec<u8>>,
    position: RefCell<i64>,
}

impl MemoryStream {
    pub fn new(data: Vec<u8>) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            data: RefCell::new(data),
            position: RefCell::new(0),
        })
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.data.borrow().clone()
    }

    /// Between writing and reading.
    pub fn rewind(&self) {
        *self.position.borrow_mut() = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.data.borrow().is_empty()
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, numBytes: i32, numBytesRead: *mut i32) -> tresult {
        if buffer.is_null() || numBytes < 0 {
            return kInvalidArgument;
        }
        let data = self.data.borrow();
        let mut position = self.position.borrow_mut();
        let start = (*position).clamp(0, data.len() as i64) as usize;
        let count = (numBytes as usize).min(data.len() - start);
        // SAFETY: the plug-in's buffer holds `numBytes`; `count` is no more.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr().add(start), buffer.cast(), count);
            if !numBytesRead.is_null() {
                *numBytesRead = count as i32;
            }
        }
        *position = (start + count) as i64;
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        numBytes: i32,
        numBytesWritten: *mut i32,
    ) -> tresult {
        if buffer.is_null() || numBytes < 0 {
            return kInvalidArgument;
        }
        let mut data = self.data.borrow_mut();
        let mut position = self.position.borrow_mut();
        let start = (*position).max(0) as usize;
        let count = numBytes as usize;
        if data.len() < start + count {
            data.resize(start + count, 0);
        }
        // SAFETY: the plug-in's `numBytes`, into room made above.
        unsafe {
            std::ptr::copy_nonoverlapping(buffer.cast(), data.as_mut_ptr().add(start), count);
            if !numBytesWritten.is_null() {
                *numBytesWritten = count as i32;
            }
        }
        *position = (start + count) as i64;
        kResultOk
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        let length = self.data.borrow().len() as i64;
        let mut position = self.position.borrow_mut();
        // From the start, from here, from the end.
        let target = match mode {
            0 => pos,
            1 => *position + pos,
            2 => length + pos,
            _ => return kInvalidArgument,
        };
        *position = target.clamp(0, length);
        if !result.is_null() {
            // SAFETY: checked above.
            unsafe { *result = *position };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked above.
        unsafe { *pos = *self.position.borrow() };
        kResultOk
    }
}

/// Hears knobs turned in the plug-in's own window. Without it those edits
/// stay in the controller and never reach the sound.
pub struct ComponentHandler {
    bridge: Arc<ParamBridge>,
    /// For our parameter list, to follow the plug-in's window.
    edits: ParamInbox,
    /// The plug-in asked to be started over.
    restart: AtomicBool,
}

impl ComponentHandler {
    pub fn new(bridge: Arc<ParamBridge>) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            bridge,
            edits: ParamInbox::new(),
            restart: AtomicBool::new(false),
        })
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, valueNormalized: ParamValue) -> tresult {
        let change = ParamChange {
            id,
            value: valueNormalized,
        };
        self.bridge.send(change);
        self.edits.push(change);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        // A new component, buses or latency: deactivated and activated
        // again by the window. New values or names: the parameter list
        // follows the controller anyway.
        let restart = RestartFlags_::kReloadComponent
            | RestartFlags_::kIoChanged
            | RestartFlags_::kLatencyChanged;
        if flags as u32 & restart as u32 != 0 {
            self.restart.store(true, Ordering::Relaxed);
        }
        kResultOk
    }
}

pub struct Notifications(pub ComWrapper<ComponentHandler>);

impl EditNotifications for Notifications {
    fn drain(&self) -> Vec<ParamChange> {
        self.0.edits.drain()
    }

    fn wants_restart(&self) -> bool {
        self.0.restart.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_keep_their_type() {
        let list = AttributeList::new();
        let mut int = 0i64;
        let mut float = 0.0f64;
        // SAFETY: local strings and buffers.
        unsafe {
            assert_eq!(list.setInt(c"level".as_ptr(), 42), kResultOk);
            assert_eq!(list.getInt(c"level".as_ptr(), &mut int), kResultOk);
            assert_eq!(list.getFloat(c"level".as_ptr(), &mut float), kResultFalse);
            assert_eq!(list.getInt(c"absent".as_ptr(), &mut int), kResultFalse);
        }
        assert_eq!(int, 42);
    }

    #[test]
    fn a_stream_reads_back_what_was_written() {
        let stream = MemoryStream::new(Vec::new());
        let payload = [1u8, 2, 3, 4];
        let mut written = 0;
        let mut back = [0u8; 4];
        let mut read = 0;
        // SAFETY: local buffers.
        unsafe {
            stream.write(payload.as_ptr().cast_mut().cast(), 4, &mut written);
            stream.rewind();
            stream.read(back.as_mut_ptr().cast(), 4, &mut read);
        }
        assert_eq!((written, read, back), (4, 4, payload));
    }
}
