//! Audio Units (macOS). The system keeps the list of installed units, so
//! nothing is scanned: they are asked for by type. A unit is known by its
//! type, subtype and manufacturer codes, written "aufx:dely:appl".
//!
//! The unit pulls its input through a render callback while we render its
//! output, both inside `process` on the audio thread. Units that can only be
//! made asynchronously (some Audio Unit v3 extensions) are left out.

use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::Arc;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{msg_send, sel};

use super::graph::{
    AudioBuffers, AudioProcessor, Loaded, ParamBridge, ParamChange, ParamFormatter, ParamInfo,
    PluginEditor, PluginState,
};
use super::window::{PluginWindow, Size};
use super::{Descriptor, Format, Result};

type OsStatus = i32;
type AudioComponent = *mut c_void;
type AudioUnit = *mut c_void;
type CfRef = *const c_void;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
struct ComponentDescription {
    kind: u32,
    subtype: u32,
    manufacturer: u32,
    flags: u32,
    flags_mask: u32,
}

#[repr(C)]
#[derive(Default)]
struct StreamFormat {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

/// An `AudioBufferList` with room for two buffers.
#[repr(C)]
struct BufferList {
    count: u32,
    buffers: [AudioBuffer; 2],
}

#[repr(C)]
#[derive(Default)]
struct TimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte: [u8; 24],
    flags: u32,
    reserved: u32,
}

type RenderCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    flags: *mut u32,
    time: *const TimeStamp,
    bus: u32,
    frames: u32,
    data: *mut BufferList,
) -> OsStatus;

#[repr(C)]
struct RenderCallbackStruct {
    callback: RenderCallback,
    refcon: *mut c_void,
}

#[repr(C)]
struct ParameterInfo {
    name: [c_char; 52],
    unit_name: CfRef,
    clump_id: u32,
    cf_name: CfRef,
    unit: u32,
    min: f32,
    max: f32,
    default: f32,
    flags: u32,
}

#[repr(C)]
struct ParameterStringFromValue {
    id: u32,
    value: *const f32,
    text: CfRef,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        component: AudioComponent,
        description: *const ComponentDescription,
    ) -> AudioComponent;
    fn AudioComponentCopyName(component: AudioComponent, name: *mut CfRef) -> OsStatus;
    fn AudioComponentGetDescription(
        component: AudioComponent,
        description: *mut ComponentDescription,
    ) -> OsStatus;
    fn AudioComponentInstanceNew(component: AudioComponent, unit: *mut AudioUnit) -> OsStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OsStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitGetPropertyInfo(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        size: *mut u32,
        writable: *mut u8,
    ) -> OsStatus;
    fn AudioUnitGetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> OsStatus;
    fn AudioUnitSetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        size: u32,
    ) -> OsStatus;
    fn AudioUnitGetParameter(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        value: *mut f32,
    ) -> OsStatus;
    fn AudioUnitSetParameter(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        value: f32,
        offset: u32,
    ) -> OsStatus;
    fn AudioUnitRender(
        unit: AudioUnit,
        flags: *mut u32,
        time: *const TimeStamp,
        bus: u32,
        frames: u32,
        data: *mut BufferList,
    ) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: CfRef);
    fn CFStringGetLength(string: CfRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(string: CfRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFDataCreate(allocator: CfRef, bytes: *const u8, length: isize) -> CfRef;
    fn CFDataGetLength(data: CfRef) -> isize;
    fn CFDataGetBytePtr(data: CfRef) -> *const u8;
    fn CFPropertyListCreateData(
        allocator: CfRef,
        list: CfRef,
        format: isize,
        options: usize,
        error: *mut CfRef,
    ) -> CfRef;
    fn CFPropertyListCreateWithData(
        allocator: CfRef,
        data: CfRef,
        options: usize,
        format: *mut isize,
        error: *mut CfRef,
    ) -> CfRef;
}

const TYPE_EFFECT: u32 = fourcc(b"aufx");
const TYPE_MUSIC_EFFECT: u32 = fourcc(b"aumf");
/// `kAudioComponentFlag_RequiresAsyncInstantiation`.
const FLAG_ASYNC_ONLY: u32 = 8;

const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;

const PROPERTY_CLASS_INFO: u32 = 0;
const PROPERTY_PARAMETER_LIST: u32 = 3;
const PROPERTY_PARAMETER_INFO: u32 = 4;
const PROPERTY_STREAM_FORMAT: u32 = 8;
const PROPERTY_MAXIMUM_FRAMES: u32 = 14;
const PROPERTY_RENDER_CALLBACK: u32 = 23;
const PROPERTY_COCOA_UI: u32 = 31;
const PROPERTY_PARAMETER_STRING: u32 = 33;

const PARAMETER_HAS_CF_NAME: u32 = 1 << 27;
const PARAMETER_CF_NAME_RELEASE: u32 = 1 << 4;
const PARAMETER_HAS_STRINGS: u32 = 1 << 21;
const PARAMETER_WRITABLE: u32 = 1 << 31;

const LINEAR_PCM: u32 = fourcc(b"lpcm");
/// Float, packed, non-interleaved.
const FLOAT_PLANAR: u32 = 1 | 8 | 32;
const SAMPLE_TIME_VALID: u32 = 1;
const UTF8: u32 = 0x0800_0100;
const BINARY_PLIST: isize = 200;

/// A CoreFoundation string as text.
///
/// # Safety
/// `string` is a live CFString.
unsafe fn text(string: CfRef) -> String {
    // SAFETY: passed on to the caller.
    unsafe {
        let length = CFStringGetLength(string);
        let capacity = CFStringGetMaximumSizeForEncoding(length, UTF8) + 1;
        let mut buffer = vec![0 as c_char; capacity as usize];
        if CFStringGetCString(string, buffer.as_mut_ptr(), capacity, UTF8) == 0 {
            return String::new();
        }
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

fn code(value: u32) -> String {
    String::from_utf8_lossy(&value.to_be_bytes()).into_owned()
}

fn uid(description: &ComponentDescription) -> String {
    format!(
        "{}:{}:{}",
        code(description.kind),
        code(description.subtype),
        code(description.manufacturer)
    )
}

fn parse_uid(uid: &str) -> Option<ComponentDescription> {
    let codes: Vec<[u8; 4]> = uid
        .split(':')
        .map(|part| part.as_bytes().try_into().ok())
        .collect::<Option<_>>()?;
    let [kind, subtype, manufacturer] = codes[..] else {
        return None;
    };
    Some(ComponentDescription {
        kind: u32::from_be_bytes(kind),
        subtype: u32::from_be_bytes(subtype),
        manufacturer: u32::from_be_bytes(manufacturer),
        flags: 0,
        flags_mask: 0,
    })
}

/// The effects installed on this Mac.
pub fn list() -> Vec<Descriptor> {
    let mut found = Vec::new();
    for kind in [TYPE_EFFECT, TYPE_MUSIC_EFFECT] {
        let wanted = ComponentDescription {
            kind,
            ..ComponentDescription::default()
        };
        let mut component = ptr::null_mut();
        loop {
            // SAFETY: plain AudioToolbox calls; the name is ours to release.
            unsafe {
                component = AudioComponentFindNext(component, &wanted);
                if component.is_null() {
                    break;
                }
                let mut description = ComponentDescription::default();
                if AudioComponentGetDescription(component, &mut description) != 0
                    || description.flags & FLAG_ASYNC_ONLY != 0
                {
                    continue;
                }
                let mut name: CfRef = ptr::null();
                if AudioComponentCopyName(component, &mut name) != 0 || name.is_null() {
                    continue;
                }
                let full = text(name);
                CFRelease(name);
                // "Manufacturer: Name".
                let (vendor, name) = match full.split_once(": ") {
                    Some((vendor, name)) => (vendor.to_owned(), name.to_owned()),
                    None => (String::new(), full),
                };
                found.push(Descriptor {
                    format: Format::Au,
                    path: Default::default(),
                    uid: uid(&description),
                    name,
                    vendor,
                    is_instrument: false,
                });
            }
        }
    }
    found
}

/// `objc_msgSend` as declared, before it is given a method's signature.
type MsgSend = unsafe extern "C-unwind" fn();

/// Sends a message that takes an `AudioUnit`. Its type is encoded one way
/// or another depending on the SDK a plug-in was built with, so objc2's
/// check of the encodings (in debug builds) would refuse half the plug-ins:
/// `objc_msgSend` is called with the method's exact signature instead.
///
/// # Safety
/// `receiver` answers `selector`, which takes the unit, and the size when
/// given, and returns an object.
unsafe fn send_unit(
    receiver: *mut AnyObject,
    selector: Sel,
    unit: AudioUnit,
    size: Option<Size>,
) -> *mut AnyObject {
    type WithSize =
        unsafe extern "C-unwind" fn(*mut AnyObject, Sel, AudioUnit, Size) -> *mut AnyObject;
    type Plain = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, AudioUnit) -> *mut AnyObject;
    let send = objc2::ffi::objc_msgSend as MsgSend;
    // SAFETY: passed on to the caller; `objc_msgSend` takes the method's
    // own signature.
    unsafe {
        match size {
            Some(size) => {
                std::mem::transmute::<MsgSend, WithSize>(send)(receiver, selector, unit, size)
            }
            None => std::mem::transmute::<MsgSend, Plain>(send)(receiver, selector, unit),
        }
    }
}

/// A unit, shared by the audio thread and the window as Audio Units allow:
/// parameters may be set from any thread.
struct Unit(AudioUnit);

// SAFETY: see above; rendering happens under the rack's lock only.
unsafe impl Send for Unit {}
unsafe impl Sync for Unit {}

impl Unit {
    fn property<T>(&self, id: u32, scope: u32, value: &T) -> OsStatus {
        // SAFETY: a live unit, a value of the property's type.
        unsafe {
            AudioUnitSetProperty(
                self.0,
                id,
                scope,
                0,
                (&raw const *value).cast(),
                size_of::<T>() as u32,
            )
        }
    }

    fn parameter(&self, id: u32) -> Option<f32> {
        let mut value = 0.0;
        // SAFETY: a live unit.
        let status = unsafe { AudioUnitGetParameter(self.0, id, SCOPE_GLOBAL, 0, &mut value) };
        (status == 0).then_some(value)
    }

    fn set_parameter(&self, id: u32, value: f32) {
        // SAFETY: a live unit.
        unsafe { AudioUnitSetParameter(self.0, id, SCOPE_GLOBAL, 0, value, 0) };
    }

    fn ids(&self) -> Vec<u32> {
        let mut size = 0;
        // SAFETY: a live unit; the list is copied into our buffer.
        unsafe {
            if AudioUnitGetPropertyInfo(
                self.0,
                PROPERTY_PARAMETER_LIST,
                SCOPE_GLOBAL,
                0,
                &mut size,
                ptr::null_mut(),
            ) != 0
            {
                return Vec::new();
            }
            let mut ids = vec![0u32; size as usize / 4];
            if AudioUnitGetProperty(
                self.0,
                PROPERTY_PARAMETER_LIST,
                SCOPE_GLOBAL,
                0,
                ids.as_mut_ptr().cast(),
                &mut size,
            ) != 0
            {
                return Vec::new();
            }
            ids.truncate(size as usize / 4);
            ids
        }
    }

    fn info(&self, id: u32) -> Option<Range> {
        // SAFETY: zeroes are a valid `ParameterInfo`.
        let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
        let mut size = size_of::<ParameterInfo>() as u32;
        // SAFETY: a live unit, a buffer of the property's type.
        let status = unsafe {
            AudioUnitGetProperty(
                self.0,
                PROPERTY_PARAMETER_INFO,
                SCOPE_GLOBAL,
                id,
                (&raw mut info).cast(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let title = if info.flags & PARAMETER_HAS_CF_NAME != 0 && !info.cf_name.is_null() {
            // SAFETY: a CFString the unit gave; released if it says so.
            unsafe {
                let title = text(info.cf_name);
                if info.flags & PARAMETER_CF_NAME_RELEASE != 0 {
                    CFRelease(info.cf_name);
                }
                title
            }
        } else {
            // SAFETY: a NUL-terminated array, at worst cut at its end.
            let bytes: Vec<u8> = info
                .name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let units = match info.unit {
            3 => "%".to_owned(),
            4 => "s".to_owned(),
            8 => "Hz".to_owned(),
            9 | 20 => "cents".to_owned(),
            10 => "st".to_owned(),
            13 => "dB".to_owned(),
            15 => "°".to_owned(),
            21 => "oct".to_owned(),
            22 => "BPM".to_owned(),
            24 => "ms".to_owned(),
            // SAFETY: the unit's own name for a custom unit.
            26 if !info.unit_name.is_null() => unsafe { text(info.unit_name) },
            _ => String::new(),
        };
        Some(Range {
            info: ParamInfo {
                id,
                title,
                units,
                read_only: info.flags & PARAMETER_WRITABLE == 0,
            },
            min: info.min,
            max: if info.max > info.min {
                info.max
            } else {
                info.min + 1.0
            },
            strings: info.flags & PARAMETER_HAS_STRINGS != 0,
        })
    }

    fn uninitialize(&self) {
        // SAFETY: a live unit; uninitializing one that is not is harmless.
        unsafe { AudioUnitUninitialize(self.0) };
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.uninitialize();
        // SAFETY: the last reference.
        unsafe { AudioComponentInstanceDispose(self.0) };
    }
}

/// A parameter and the range its values span.
#[derive(Clone)]
struct Range {
    info: ParamInfo,
    min: f32,
    max: f32,
    /// The unit writes its own value texts.
    strings: bool,
}

impl Range {
    fn normalize(&self, value: f32) -> f64 {
        f64::from(((value - self.min) / (self.max - self.min)).clamp(0.0, 1.0))
    }

    fn denormalize(&self, value: f64) -> f32 {
        self.min + (self.max - self.min) * value.clamp(0.0, 1.0) as f32
    }
}

pub fn instantiate(descriptor: &Descriptor) -> Result<Loaded> {
    let fail = |reason: &str| format!("{}: {reason}", descriptor.name);
    let wanted = parse_uid(&descriptor.uid).ok_or_else(|| fail("bad id"))?;
    // SAFETY: plain AudioToolbox calls.
    let unit = unsafe {
        let component = AudioComponentFindNext(ptr::null_mut(), &wanted);
        if component.is_null() {
            return Err(fail("not installed"));
        }
        let mut unit = ptr::null_mut();
        let status = AudioComponentInstanceNew(component, &mut unit);
        if status != 0 || unit.is_null() {
            return Err(fail(&format!("did not start ({status})")));
        }
        Arc::new(Unit(unit))
    };
    let ranges: Vec<Range> = unit
        .ids()
        .into_iter()
        .filter_map(|id| unit.info(id))
        .collect();
    let values = ranges
        .iter()
        .map(|r| r.normalize(unit.parameter(r.info.id).unwrap_or(r.min)))
        .collect();
    let (bridge, changes) = ParamBridge::for_parameters(ranges.len());
    let shared = || Shared {
        unit: Arc::clone(&unit),
        ranges: ranges.clone(),
    };
    Ok(Loaded {
        parameters: ranges.iter().map(|r| r.info.clone()).collect(),
        values,
        bridge,
        formatter: Some(Box::new(shared())),
        editor: Some(Box::new(AuEditor {
            unit: Arc::clone(&unit),
            window: None,
            view: None,
        })),
        notifications: None,
        state: Some(Box::new(shared())),
        processor: Box::new(AuInstance {
            unit,
            name: descriptor.name.clone(),
            ranges,
            changes,
            input: Box::new(Input {
                channels: Vec::new(),
                frames: 0,
            }),
            outputs: Vec::new(),
            max_frames: 0,
            sample_time: 0.0,
            active: false,
        }),
    })
}

/// What the render callback hands the unit; boxed so its address stays.
struct Input {
    channels: Vec<Vec<f32>>,
    frames: usize,
}

/// Gives the unit its input: into its buffers, or ours when it has none.
unsafe extern "C" fn render_input(
    refcon: *mut c_void,
    _flags: *mut u32,
    _time: *const TimeStamp,
    _bus: u32,
    frames: u32,
    data: *mut BufferList,
) -> OsStatus {
    // SAFETY: `refcon` is the instance's boxed input, alive while it
    // renders; `data` lists `count` buffers.
    unsafe {
        let input = &mut *refcon.cast::<Input>();
        let list = &mut *data;
        let count = (list.count as usize).min(2);
        let frames = (frames as usize).min(input.frames);
        let buffers = &mut list.buffers[..count];
        for (index, buffer) in buffers.iter_mut().enumerate() {
            let Some(channel) = input.channels.get_mut(index) else {
                break;
            };
            if buffer.data.is_null() {
                buffer.data = channel.as_mut_ptr().cast();
            } else {
                ptr::copy_nonoverlapping(channel.as_ptr(), buffer.data.cast::<f32>(), frames);
            }
            buffer.bytes = (frames * 4) as u32;
        }
    }
    0
}

struct AuInstance {
    unit: Arc<Unit>,
    name: String,
    ranges: Vec<Range>,
    changes: rtrb::Consumer<ParamChange>,
    input: Box<Input>,
    outputs: Vec<Vec<f32>>,
    max_frames: usize,
    sample_time: f64,
    active: bool,
}

impl AuInstance {
    /// Sets the format on both sides; false when the unit refuses it.
    fn set_format(&self, rate: f64, channels: usize) -> bool {
        let format = StreamFormat {
            sample_rate: rate,
            format_id: LINEAR_PCM,
            format_flags: FLOAT_PLANAR,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: channels as u32,
            bits_per_channel: 32,
            reserved: 0,
        };
        [SCOPE_INPUT, SCOPE_OUTPUT]
            .iter()
            .all(|&scope| self.unit.property(PROPERTY_STREAM_FORMAT, scope, &format) == 0)
    }
}

impl AudioProcessor for AuInstance {
    fn activate(&mut self, sample_rate: f64, max_frames: usize, channels: usize) -> Result<()> {
        self.deactivate();
        // Mono costs least; units that only do stereo get the microphone on
        // both sides.
        let channels = [channels.clamp(1, 2), 2]
            .into_iter()
            .find(|&n| self.set_format(sample_rate, n))
            .ok_or_else(|| format!("{}: takes neither mono nor stereo", self.name))?;
        self.unit
            .property(PROPERTY_MAXIMUM_FRAMES, SCOPE_GLOBAL, &(max_frames as u32));
        let callback = RenderCallbackStruct {
            callback: render_input,
            refcon: (&raw mut *self.input).cast(),
        };
        self.unit
            .property(PROPERTY_RENDER_CALLBACK, SCOPE_INPUT, &callback);
        self.input.channels = vec![vec![0.0; max_frames]; channels];
        self.outputs = vec![vec![0.0; max_frames]; channels];
        // SAFETY: a live unit, set up above.
        let status = unsafe { AudioUnitInitialize(self.unit.0) };
        if status != 0 {
            return Err(format!("{}: did not start ({status})", self.name));
        }
        self.max_frames = max_frames;
        self.active = true;
        Ok(())
    }

    fn deactivate(&mut self) {
        if std::mem::take(&mut self.active) {
            self.unit.uninitialize();
        }
    }

    fn process(&mut self, buffers: &mut AudioBuffers<'_>) {
        let frames = buffers.frames();
        if !self.active || frames > self.max_frames || buffers.channel_count() == 0 {
            return;
        }
        while let Ok(change) = self.changes.pop() {
            if let Some(range) = self.ranges.iter().find(|r| r.info.id == change.id) {
                self.unit
                    .set_parameter(change.id, range.denormalize(change.value));
            }
        }
        buffers.spread_into(&mut self.input.channels);
        self.input.frames = frames;
        let mut list = BufferList {
            count: self.outputs.len() as u32,
            buffers: [AudioBuffer {
                channels: 1,
                bytes: 0,
                data: ptr::null_mut(),
            }; 2],
        };
        for (buffer, channel) in list.buffers.iter_mut().zip(&mut self.outputs) {
            buffer.bytes = (frames * 4) as u32;
            buffer.data = channel.as_mut_ptr().cast();
        }
        let time = TimeStamp {
            sample_time: self.sample_time,
            flags: SAMPLE_TIME_VALID,
            ..TimeStamp::default()
        };
        let mut flags = 0;
        // SAFETY: a live, initialized unit; buffers of `max_frames`.
        let status =
            unsafe { AudioUnitRender(self.unit.0, &mut flags, &time, 0, frames as u32, &mut list) };
        self.sample_time += frames as f64;
        if status != 0 {
            return;
        }
        // The unit may answer with buffers of its own.
        for (channel, buffer) in self.outputs.iter_mut().zip(&list.buffers) {
            if !buffer.data.is_null() && buffer.data != channel.as_mut_ptr().cast() {
                // SAFETY: the unit's buffer holds `frames` samples.
                unsafe {
                    ptr::copy_nonoverlapping(
                        buffer.data.cast::<f32>(),
                        channel.as_mut_ptr(),
                        frames,
                    );
                }
            }
        }
        buffers.collect_from(&self.outputs);
    }
}

/// The window's view of the unit.
struct Shared {
    unit: Arc<Unit>,
    ranges: Vec<Range>,
}

impl Shared {
    fn range(&self, id: u32) -> Option<&Range> {
        self.ranges.iter().find(|r| r.info.id == id)
    }
}

impl ParamFormatter for Shared {
    fn text(&self, id: u32, value: f64) -> Option<String> {
        let range = self.range(id)?;
        let value = range.denormalize(value);
        if range.strings {
            let mut request = ParameterStringFromValue {
                id,
                value: &value,
                text: ptr::null(),
            };
            let mut size = size_of::<ParameterStringFromValue>() as u32;
            // SAFETY: a live unit; the string is ours to release.
            unsafe {
                if AudioUnitGetProperty(
                    self.unit.0,
                    PROPERTY_PARAMETER_STRING,
                    SCOPE_GLOBAL,
                    0,
                    (&raw mut request).cast(),
                    &mut size,
                ) == 0
                    && !request.text.is_null()
                {
                    let text = text(request.text);
                    CFRelease(request.text);
                    return Some(text);
                }
            }
        }
        let digits = if value.abs() >= 100.0 {
            0
        } else if value.abs() >= 10.0 {
            1
        } else {
            2
        };
        Some(
            format!("{value:.digits$} {}", range.info.units)
                .trim()
                .to_owned(),
        )
    }

    fn set(&self, id: u32, value: f64) {
        if let Some(range) = self.range(id) {
            self.unit.set_parameter(id, range.denormalize(value));
        }
    }

    fn get(&self, id: u32) -> Option<f64> {
        let range = self.range(id)?;
        Some(range.normalize(self.unit.parameter(id)?))
    }

    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        let range = self.range(id)?;
        // "50%" of a parameter in other units is a share of its range.
        if text.trim().ends_with('%') && range.info.units != "%" {
            return None;
        }
        let value = crate::fx::graph::leading_number(text)?;
        Some(range.normalize(value as f32))
    }
}

impl PluginState for Shared {
    /// The unit's class info (its preset) as a binary property list.
    fn save(&self) -> Option<Vec<u8>> {
        let mut list: CfRef = ptr::null();
        let mut size = size_of::<CfRef>() as u32;
        // SAFETY: a live unit; the list and the data are ours to release.
        unsafe {
            if AudioUnitGetProperty(
                self.unit.0,
                PROPERTY_CLASS_INFO,
                SCOPE_GLOBAL,
                0,
                (&raw mut list).cast(),
                &mut size,
            ) != 0
                || list.is_null()
            {
                return None;
            }
            let data =
                CFPropertyListCreateData(ptr::null(), list, BINARY_PLIST, 0, ptr::null_mut());
            CFRelease(list);
            if data.is_null() {
                return None;
            }
            let bytes =
                std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize)
                    .to_vec();
            CFRelease(data);
            Some(bytes)
        }
    }

    fn restore(&self, bytes: &[u8]) -> bool {
        // SAFETY: plain CoreFoundation calls on our own objects.
        unsafe {
            let data = CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as isize);
            if data.is_null() {
                return false;
            }
            let list = CFPropertyListCreateWithData(
                ptr::null(),
                data,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CFRelease(data);
            if list.is_null() {
                return false;
            }
            let restored = self.unit.property(PROPERTY_CLASS_INFO, SCOPE_GLOBAL, &list) == 0;
            CFRelease(list);
            restored
        }
    }
}

/// The unit's own Cocoa view, or the system's generic one.
struct AuEditor {
    unit: Arc<Unit>,
    window: Option<PluginWindow>,
    view: Option<Retained<AnyObject>>,
}

impl AuEditor {
    /// The view the unit names in `kAudioUnitProperty_CocoaUI`.
    fn own_view(&self) -> Option<Retained<AnyObject>> {
        let mut size = 0;
        // SAFETY: a live unit; the URL and the class names are ours to
        // release (toll-free bridged to NSURL and NSString).
        unsafe {
            if AudioUnitGetPropertyInfo(
                self.unit.0,
                PROPERTY_COCOA_UI,
                SCOPE_GLOBAL,
                0,
                &mut size,
                ptr::null_mut(),
            ) != 0
                || (size as usize) < 2 * size_of::<CfRef>()
            {
                return None;
            }
            let mut info = vec![ptr::null::<c_void>(); size as usize / size_of::<CfRef>()];
            if AudioUnitGetProperty(
                self.unit.0,
                PROPERTY_COCOA_UI,
                SCOPE_GLOBAL,
                0,
                info.as_mut_ptr().cast(),
                &mut size,
            ) != 0
            {
                return None;
            }
            let (url, class_name) = (info[0], info[1]);
            let view = (|| {
                let bundle_class = AnyClass::get(c"NSBundle")?;
                let bundle: Option<Retained<AnyObject>> =
                    msg_send![bundle_class, bundleWithURL: url.cast::<AnyObject>()];
                let class: Option<&AnyClass> =
                    msg_send![&*bundle?, classNamed: class_name.cast::<AnyObject>()];
                let allocated: Allocated<AnyObject> = msg_send![class?, alloc];
                let factory: Option<Retained<AnyObject>> = msg_send![allocated, init];
                let size = Size {
                    width: 600.0,
                    height: 400.0,
                };
                // Autoreleased: retained for us.
                let view = send_unit(
                    Retained::as_ptr(&factory?).cast_mut(),
                    sel!(uiViewForAudioUnit:withSize:),
                    self.unit.0,
                    Some(size),
                );
                Retained::retain(view)
            })();
            for &object in info.iter().filter(|o| !o.is_null()) {
                CFRelease(object);
            }
            view
        }
    }

    /// CoreAudioKit's generic view: sliders for every parameter. The
    /// framework loads only then: Apple's own units bring a copy of some of
    /// its classes, and with both loaded the runtime warns about each.
    fn generic_view(&self) -> Option<Retained<AnyObject>> {
        if AnyClass::get(c"AUGenericView").is_none() {
            const KIT: &str = "/System/Library/Frameworks/CoreAudioKit.framework/CoreAudioKit";
            // SAFETY: a system framework; it stays loaded.
            let kit = unsafe { libloading::Library::new(KIT) }.ok()?;
            std::mem::forget(kit);
        }
        let class = AnyClass::get(c"AUGenericView")?;
        // SAFETY: `initWithAudioUnit:` takes the unit, which outlives the
        // view, and returns the initialized object, ours.
        unsafe {
            type Alloc = unsafe extern "C-unwind" fn(*const AnyClass, Sel) -> *mut AnyObject;
            let alloc = std::mem::transmute::<MsgSend, Alloc>(objc2::ffi::objc_msgSend as MsgSend);
            let allocated = alloc(class, sel!(alloc));
            let view = send_unit(allocated, sel!(initWithAudioUnit:), self.unit.0, None);
            Retained::from_raw(view)
        }
    }
}

impl PluginEditor for AuEditor {
    fn open(&mut self, title: &str) -> crate::fx::Result<()> {
        if let Some(window) = &self.window {
            window.show();
            return Ok(());
        }
        let view = self
            .own_view()
            .or_else(|| self.generic_view())
            .ok_or("the unit has no window")?;
        let window = PluginWindow::open_with(title, &view)?;
        window.show();
        self.window = Some(window);
        self.view = Some(view);
        Ok(())
    }

    fn close(&mut self) {
        self.window = None;
        self.view = None;
    }

    fn is_open(&self) -> bool {
        self.window.is_some()
    }

    fn poll(&mut self) {
        let (Some(window), Some(view)) = (&self.window, &self.view) else {
            return;
        };
        if window.closed_by_user() {
            return self.close();
        }
        // Units resize their views themselves.
        window.fit(view);
    }
}

impl Drop for AuEditor {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_read_back() {
        let description = ComponentDescription {
            kind: TYPE_EFFECT,
            subtype: fourcc(b"dely"),
            manufacturer: fourcc(b"appl"),
            ..ComponentDescription::default()
        };
        assert_eq!(uid(&description), "aufx:dely:appl");
        assert!(parse_uid("aufx:dely:appl") == Some(description));
        assert!(parse_uid("aufx:dely").is_none());
    }

    /// Apple's units come with every Mac.
    #[test]
    fn apples_lowpass_filters_the_microphone() {
        let lowpass = list()
            .into_iter()
            .find(|d| d.uid == "aufx:lpas:appl")
            .expect("AULowpass is installed");
        assert_eq!(lowpass.vendor, "Apple");
        let mut loaded = instantiate(&lowpass).unwrap();
        assert!(!loaded.parameters.is_empty());
        let cutoff = &loaded.parameters[0];
        let formatter = loaded.formatter.as_ref().unwrap();
        assert!(formatter.text(cutoff.id, 0.5).is_some());
        let processor = &mut loaded.processor;
        processor.activate(48_000.0, 512, 1).unwrap();
        // A 12 kHz tone, well above the default cutoff (6.9 kHz).
        let tone: Vec<f32> = (0..512)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 12_000.0 / 48_000.0).sin())
            .collect();
        let mut peak = 0.0f32;
        for _ in 0..8 {
            let mut channels = vec![tone.clone()];
            processor.process(&mut AudioBuffers::new(&mut channels, 512));
            peak = channels[0].iter().fold(0.0, |p, s| p.max(s.abs()));
        }
        assert!(peak < 0.5, "not filtered: {peak}");
        let state = loaded.state.as_ref().unwrap();
        let saved = state.save().expect("a state");
        assert!(state.restore(&saved));
    }
}
