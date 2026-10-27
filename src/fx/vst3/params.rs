//! Parameters: read from the `IEditController`, and delivered to `process`
//! through `IParameterChanges`. Setting a value on the controller alone does
//! nothing to the sound: the host passes every change to the processor.

#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_;
use vst3::Steinberg::Vst::{
    IEditController, IEditControllerTrait, IParamValueQueue, IParamValueQueueTrait,
    IParameterChanges, IParameterChangesTrait, ParamID, ParamValue, ParameterInfo,
};
use vst3::Steinberg::{kInvalidArgument, kResultOk, tresult};
use vst3::{Class, ComPtr, ComWrapper};

use super::controller::Controller;
use crate::fx::graph::{ParamFormatter, ParamInfo};

/// The parameters to list: hidden ones and program changes are left out.
pub fn read_parameters(controller: &ComPtr<IEditController>) -> Vec<ParamInfo> {
    // SAFETY: an initialized controller.
    let count = unsafe { controller.getParameterCount() };
    (0..count)
        .filter_map(|index| {
            let mut info = ParameterInfo {
                id: 0,
                title: [0; 128],
                shortTitle: [0; 128],
                units: [0; 128],
                stepCount: 0,
                defaultNormalizedValue: 0.0,
                unitId: 0,
                flags: 0,
            };
            // SAFETY: an index below the count, an output of the right type.
            if unsafe { controller.getParameterInfo(index, &mut info) } != kResultOk {
                return None;
            }
            let flags = info.flags;
            if flags & (ParameterFlags_::kIsHidden | ParameterFlags_::kIsProgramChange) != 0 {
                return None;
            }
            Some(ParamInfo {
                id: info.id,
                title: utf16(&info.title),
                units: utf16(&info.units),
                read_only: flags & ParameterFlags_::kIsReadOnly != 0,
            })
        })
        .collect()
}

/// `String128` up to its terminating zero.
fn utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

/// One parameter's points for a block. The host owns these and reuses them
/// from block to block: nothing is allocated in `process`.
pub struct ParamQueue {
    id: Cell<ParamID>,
    points: RefCell<Vec<(i32, ParamValue)>>,
}

impl ParamQueue {
    fn reset(&self, id: ParamID, value: ParamValue) {
        self.id.set(id);
        let mut points = self.points.borrow_mut();
        points.clear();
        points.push((0, value));
    }
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> i32 {
        self.points.borrow().len() as i32
    }

    unsafe fn getPoint(
        &self,
        index: i32,
        sampleOffset: *mut i32,
        value: *mut ParamValue,
    ) -> tresult {
        let points = self.points.borrow();
        let Some(&(offset, point)) = points.get(index.max(0) as usize) else {
            return kInvalidArgument;
        };
        if sampleOffset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked above.
        unsafe {
            *sampleOffset = offset;
            *value = point;
        }
        kResultOk
    }

    unsafe fn addPoint(&self, sampleOffset: i32, value: ParamValue, index: *mut i32) -> tresult {
        let mut points = self.points.borrow_mut();
        // Within the room reserved: never allocate on the audio thread.
        if points.len() == points.capacity() {
            return kInvalidArgument;
        }
        points.push((sampleOffset, value));
        if !index.is_null() {
            // SAFETY: checked above.
            unsafe { *index = points.len() as i32 - 1 };
        }
        kResultOk
    }
}

/// The queues of one block, given to the plug-in as `inputParameterChanges`.
pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    /// Taken once: taking them per block would add a reference each time
    /// that the plug-in never releases.
    queue_ptrs: Vec<ComPtr<IParamValueQueue>>,
    used: Cell<usize>,
}

impl ParamChanges {
    /// `capacity` parameters may change in one block.
    pub fn new(capacity: usize) -> ComWrapper<Self> {
        let queues: Vec<ComWrapper<ParamQueue>> = (0..capacity.max(1))
            .map(|_| {
                ComWrapper::new(ParamQueue {
                    id: Cell::new(0),
                    points: RefCell::new(Vec::with_capacity(8)),
                })
            })
            .collect();
        let queue_ptrs = queues
            .iter()
            .filter_map(|q| q.to_com_ptr::<IParamValueQueue>())
            .collect();
        ComWrapper::new(Self {
            queues,
            queue_ptrs,
            used: Cell::new(0),
        })
    }

    pub fn begin_block(&self) {
        self.used.set(0);
    }

    /// A change at the start of the block; false when the queues are full.
    /// A parameter changed twice keeps one queue with the last value: a
    /// plug-in may read any one of two queues of the same id.
    pub fn add(&self, id: ParamID, value: ParamValue) -> bool {
        let used = self.used.get();
        if let Some(queue) = self.queues[..used].iter().find(|q| q.id.get() == id) {
            queue.reset(id, value);
            return true;
        }
        let Some(queue) = self.queues.get(used) else {
            return false;
        };
        queue.reset(id, value);
        self.used.set(used + 1);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.used.get() == 0
    }
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> i32 {
        self.used.get() as i32
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.used.get() {
            return std::ptr::null_mut();
        }
        // The host keeps ownership; plug-ins do not release these.
        self.queue_ptrs
            .get(index as usize)
            .map_or(std::ptr::null_mut(), ComPtr::as_ptr)
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut i32,
    ) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let slot = self.used.get();
        let Some(queue) = self.queues.get(slot) else {
            return std::ptr::null_mut();
        };
        // SAFETY: checked above.
        queue.id.set(unsafe { *id });
        queue.points.borrow_mut().clear();
        self.used.set(slot + 1);
        if !index.is_null() {
            // SAFETY: checked above.
            unsafe { *index = slot as i32 };
        }
        // SAFETY: the slot was just taken.
        unsafe { self.getParameterData(slot as i32) }
    }
}

/// Values as the plug-in writes them, through its controller.
pub struct ControllerFormatter(pub Rc<Controller>);

impl ParamFormatter for ControllerFormatter {
    fn text(&self, id: u32, value: f64) -> Option<String> {
        let mut buffer = [0u16; 128];
        // SAFETY: a String128 buffer.
        let ok = unsafe { self.0.get().getParamStringByValue(id, value, &mut buffer) } == kResultOk;
        let text = utf16(&buffer);
        (ok && !text.is_empty()).then_some(text)
    }

    fn set(&self, id: u32, value: f64) {
        // SAFETY: an id of this controller.
        unsafe { self.0.get().setParamNormalized(id, value) };
    }

    fn get(&self, id: u32) -> Option<f64> {
        // SAFETY: as above.
        Some(unsafe { self.0.get().getParamNormalized(id) })
    }

    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        let mut utf16: Vec<u16> = text.encode_utf16().take(127).collect();
        utf16.push(0);
        let mut value = 0.0;
        // SAFETY: a NUL-terminated string and an output value.
        let ok = unsafe {
            self.0
                .get()
                .getParamValueByString(id, utf16.as_mut_ptr().cast(), &mut value)
        } == kResultOk;
        ok.then_some(value.clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_stops_at_nul() {
        let mut raw = [0u16; 8];
        raw[0] = u16::from(b'a');
        raw[1] = u16::from(b'b');
        raw[3] = u16::from(b'X');
        assert_eq!(utf16(&raw), "ab");
    }

    #[test]
    fn a_repeated_parameter_keeps_one_queue_and_the_last_value() {
        let changes = ParamChanges::new(2);
        changes.begin_block();
        assert!(changes.add(1, 0.1));
        assert!(changes.add(1, 0.9));
        assert!(changes.add(2, 0.5));
        assert!(!changes.add(3, 0.5), "no third queue");
        // SAFETY: a live object.
        assert_eq!(unsafe { changes.getParameterCount() }, 2);
        assert_eq!(changes.queues[0].points.borrow().as_slice(), &[(0, 0.9)]);
        changes.begin_block();
        assert!(changes.is_empty());
    }
}
