//! `AEffect` from `aeffect.h` and `aeffectx.h`, used directly: the library,
//! its entry point and calls through the plug-in's function table.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::fx::Result;
use crate::fx::graph::{ParamChange, ParamInbox};

type EntryProc = unsafe extern "C" fn(callback: HostCallback) -> *mut AEffect;
type HostCallback = extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type ProcessProc = extern "C" fn(*mut AEffect, *const *const f32, *mut *mut f32, i32);
type ProcessDoubleProc = extern "C" fn(*mut AEffect, *const *const f64, *mut *mut f64, i32);
type SetParameterProc = extern "C" fn(*mut AEffect, i32, f32);
type GetParameterProc = extern "C" fn(*mut AEffect, i32) -> f32;

/// "VstP" at the start of every `AEffect`.
const MAGIC: i32 = i32::from_be_bytes(*b"VstP");

/// The plug-in's structure; unread fields keep the layout.
#[repr(C)]
#[allow(non_snake_case, dead_code)]
struct AEffect {
    magic: i32,
    dispatcher: HostCallback,
    process: ProcessProc,
    setParameter: SetParameterProc,
    getParameter: GetParameterProc,
    numPrograms: i32,
    numParams: i32,
    numInputs: i32,
    numOutputs: i32,
    flags: i32,
    reserved1: isize,
    reserved2: isize,
    initialDelay: i32,
    realQualities: i32,
    offQualities: i32,
    ioRatio: f32,
    object: *mut c_void,
    user: *mut c_void,
    uniqueId: i32,
    version: i32,
    processReplacing: ProcessProc,
    processDoubleReplacing: ProcessDoubleProc,
    future: [u8; 56],
}

/// Dispatcher opcodes the host sends (SDK names where they differ).
#[derive(Clone, Copy)]
#[repr(i32)]
enum OpCode {
    /// `effOpen`.
    Open = 0,
    /// `effClose`.
    Close = 1,
    GetParameterLabel = 6,
    GetParameterDisplay = 7,
    GetParameterName = 8,
    SetSampleRate = 10,
    SetBlockSize = 11,
    /// `effMainsChanged`.
    MainsChanged = 12,
    EditorGetRect = 13,
    EditorOpen = 14,
    EditorClose = 15,
    EditorIdle = 19,
    /// `effGetChunk`.
    GetChunk = 23,
    /// `effSetChunk`.
    SetChunk = 24,
    /// `effString2Parameter`.
    StringToParameter = 27,
    GetEffectName = 45,
    /// `effGetVendorString`.
    GetVendorName = 47,
    /// `effStartProcess`, after resuming.
    StartProcess = 71,
    /// `effStopProcess`, before suspending.
    StopProcess = 72,
}

mod flags {
    /// `effFlagsHasEditor`.
    pub const HAS_EDITOR: i32 = 1;
    /// `effFlagsCanReplacing`: without it `processReplacing` may be garbage.
    pub const CAN_REPLACING: i32 = 1 << 4;
    /// `effFlagsProgramChunks`: the state is an opaque block.
    pub const PROGRAM_CHUNKS: i32 = 1 << 5;
}

/// Copies `text` into a plug-in's string buffer of `size` bytes.
///
/// # Safety
/// `ptr` is null or holds `size` bytes.
unsafe fn give_string(ptr: *mut c_void, size: usize, text: &str) -> isize {
    if ptr.is_null() {
        return 0;
    }
    let count = text.len().min(size - 1);
    // SAFETY: passed on to the caller.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), ptr.cast::<u8>(), count);
        *ptr.cast::<u8>().add(count) = 0;
    }
    1
}

/// The host's answers to the plug-in: what an effect on a microphone needs
/// (the rate, the block size, its window's size, who the host is). The rest
/// answers 0, "not supported".
extern "C" fn host_callback(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    const AUTOMATE: i32 = 0;
    const VERSION: i32 = 1;
    const CURRENT_ID: i32 = 2;
    const IO_CHANGED: i32 = 13;
    const SIZE_WINDOW: i32 = 15;
    const GET_SAMPLE_RATE: i32 = 16;
    const GET_BLOCK_SIZE: i32 = 17;
    const GET_CURRENT_PROCESS_LEVEL: i32 = 23;
    const GET_VENDOR_STRING: i32 = 32;
    const GET_PRODUCT_STRING: i32 = 33;
    const GET_VENDOR_VERSION: i32 = 34;
    const CAN_DO: i32 = 37;
    const GET_LANGUAGE: i32 = 38;
    const UPDATE_DISPLAY: i32 = 42;
    const BEGIN_EDIT: i32 = 43;
    const END_EDIT: i32 = 44;

    // SAFETY: `reserved1` holds our data from right after loading for as
    // long as the plug-in lives; it is 0 before (calls from the entry point).
    let host = if effect.is_null() {
        None
    } else {
        unsafe { ((*effect).reserved1 as *const HostData).as_ref() }
    };
    match opcode {
        // A knob turned in the plug-in's own window.
        AUTOMATE => {
            if let Some(host) = host {
                host.edits.push(ParamChange {
                    id: index.max(0) as u32,
                    value: f64::from(opt),
                });
            }
            0
        }
        VERSION => 2400,
        CURRENT_ID => 0,
        // New inputs, outputs or latency: the plug-in starts over.
        IO_CHANGED => {
            if let Some(host) = host {
                host.restart.store(true, Ordering::Relaxed);
            }
            1
        }
        // Its editor wants another size.
        SIZE_WINDOW => {
            if let Some(host) = host {
                let size = (index.max(1) as u64) << 32 | value.clamp(1, i32::MAX as isize) as u64;
                host.window_size.store(size, Ordering::Relaxed);
            }
            1
        }
        // SAFETY: 64 bytes, as the spec gives these.
        GET_VENDOR_STRING | GET_PRODUCT_STRING => unsafe { give_string(ptr, 64, "VibeFuse") },
        GET_VENDOR_VERSION => 1000,
        CAN_DO => {
            let asked = (!ptr.is_null())
                // SAFETY: a NUL-terminated string from the plug-in.
                .then(|| unsafe { std::ffi::CStr::from_ptr(ptr.cast()) })
                .and_then(|text| text.to_str().ok());
            match asked {
                Some("sizeWindow" | "supplyIdle") => 1,
                _ => 0,
            }
        }
        // English.
        GET_LANGUAGE => 1,
        UPDATE_DISPLAY | BEGIN_EDIT | END_EDIT => 1,
        GET_SAMPLE_RATE => host.map_or(48_000, |h| {
            f32::from_bits(h.sample_rate.load(Ordering::Relaxed)) as isize
        }),
        GET_BLOCK_SIZE => host.map_or(512, |h| h.block_size.load(Ordering::Relaxed) as isize),
        // Real time, not offline.
        GET_CURRENT_PROCESS_LEVEL => 2,
        _ => 0,
    }
}

/// What the host callback finds through `reserved1`.
struct HostData {
    /// Plug-ins of this generation report edits from `processReplacing`,
    /// the audio thread: hence a queue that never waits.
    edits: ParamInbox,
    sample_rate: AtomicU32,
    block_size: AtomicU32,
    /// The plug-in asked to be started over (`audioMasterIOChanged`).
    restart: AtomicBool,
    /// The size its editor asked for, width and height; 0 for none.
    window_size: AtomicU64,
    /// Resumed and processing.
    active: AtomicBool,
}

/// A loaded VST2 plug-in, shared by the audio thread (processing) and the
/// window (parameters, editor), as VST2 itself expects.
pub struct RawPlugin {
    effect: *mut AEffect,
    host: Box<HostData>,
    /// Never closed: plug-ins leave work behind on system queues that calls
    /// into them later (see `vst3::module`).
    _library: ManuallyDrop<libloading::Library>,
}

// SAFETY: VST2 is made for exactly this split: the dispatcher from the UI
// thread, `processReplacing` from the audio thread.
unsafe impl Send for RawPlugin {}
unsafe impl Sync for RawPlugin {}

/// The binary of a plug-in: on macOS inside its `.vst` bundle.
fn binary(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let dir = path.join("Contents").join("MacOS");
    if let Some(stem) = path.file_stem() {
        let named = dir.join(stem);
        if named.is_file() {
            return Ok(named);
        }
    }
    std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no binary in {}", dir.display()))
}

impl RawPlugin {
    pub fn load(path: &Path) -> Result<Self> {
        let binary = binary(path)?;
        // SAFETY: loading foreign code is what hosting plug-ins is.
        let library = unsafe { libloading::Library::new(&binary) }
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: the entry point's signature is VST2's; `main_macho` is the
        // old name on macOS, `main` the oldest.
        let effect = unsafe {
            let entry = library
                .get::<EntryProc>(b"VSTPluginMain\0")
                .or_else(|_| library.get::<EntryProc>(b"main_macho\0"))
                .or_else(|_| library.get::<EntryProc>(b"main\0"))
                .map_err(|_| format!("{}: not a VST2 plug-in", path.display()))?;
            entry(host_callback)
        };
        if effect.is_null() {
            return Err(format!("{}: the plug-in did not start", path.display()));
        }
        // SAFETY: a non-null pointer the plug-in gave us.
        let (magic, flags) = unsafe { ((*effect).magic, (*effect).flags) };
        if magic != MAGIC {
            return Err(format!("{}: not a VST2 plug-in", path.display()));
        }
        if flags & flags::CAN_REPLACING == 0 {
            return Err(format!("{}: too old a VST2 plug-in", path.display()));
        }
        let plugin = Self {
            effect,
            host: Box::new(HostData {
                edits: ParamInbox::new(),
                sample_rate: AtomicU32::new(48_000f32.to_bits()),
                block_size: AtomicU32::new(512),
                restart: AtomicBool::new(false),
                window_size: AtomicU64::new(0),
                active: AtomicBool::new(false),
            }),
            _library: ManuallyDrop::new(library),
        };
        // SAFETY: `reserved1` is the host's; the box lives as long as `plugin`.
        unsafe { (*effect).reserved1 = (&raw const *plugin.host) as isize };
        plugin.dispatch(OpCode::Open, 0, 0, std::ptr::null_mut(), 0.0);
        Ok(plugin)
    }

    fn dispatch(&self, op: OpCode, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        // SAFETY: `effect` is valid for the life of `self`.
        unsafe { ((*self.effect).dispatcher)(self.effect, op as i32, index, value, ptr, opt) }
    }

    /// A string the plug-in writes into our buffer.
    fn dispatch_string(&self, op: OpCode, index: i32) -> String {
        let mut buffer = [0u8; 256];
        self.dispatch(op, index, 0, buffer.as_mut_ptr().cast(), 0.0);
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).trim().to_owned()
    }

    fn field(&self, read: impl FnOnce(&AEffect) -> i32) -> i32 {
        // SAFETY: `effect` is valid for the life of `self`.
        read(unsafe { &*self.effect })
    }

    pub fn name(&self) -> String {
        self.dispatch_string(OpCode::GetEffectName, 0)
    }

    pub fn vendor(&self) -> String {
        self.dispatch_string(OpCode::GetVendorName, 0)
    }

    pub fn unique_id(&self) -> i32 {
        self.field(|e| e.uniqueId)
    }

    pub fn num_params(&self) -> i32 {
        self.field(|e| e.numParams)
    }

    pub fn num_inputs(&self) -> i32 {
        self.field(|e| e.numInputs)
    }

    pub fn num_outputs(&self) -> i32 {
        self.field(|e| e.numOutputs)
    }

    fn has_flag(&self, flag: i32) -> bool {
        self.field(|e| e.flags) & flag != 0
    }

    pub fn has_editor(&self) -> bool {
        self.has_flag(flags::HAS_EDITOR)
    }

    pub fn edits(&self) -> &ParamInbox {
        &self.host.edits
    }

    /// The whole state as an opaque block, from plug-ins that keep one.
    pub fn get_chunk(&self) -> Option<Vec<u8>> {
        if !self.has_flag(flags::PROGRAM_CHUNKS) {
            return None;
        }
        let mut data: *mut c_void = std::ptr::null_mut();
        // Index 0: the whole bank.
        let size = self.dispatch(OpCode::GetChunk, 0, 0, (&raw mut data).cast(), 0.0);
        if data.is_null() || size <= 0 {
            return None;
        }
        // SAFETY: the plug-in owns `size` bytes there; copied at once.
        Some(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) }.to_vec())
    }

    pub fn set_chunk(&self, data: &[u8]) -> bool {
        if !self.has_flag(flags::PROGRAM_CHUNKS) || data.is_empty() {
            return false;
        }
        let ptr = data.as_ptr().cast_mut().cast();
        self.dispatch(OpCode::SetChunk, 0, data.len() as isize, ptr, 0.0) != 0
    }

    /// The editor's size; the plug-in returns a pointer to its own `ERect`.
    pub fn editor_size(&self) -> Option<(i32, i32)> {
        let mut rect: *const i16 = std::ptr::null();
        self.dispatch(OpCode::EditorGetRect, 0, 0, (&raw mut rect).cast(), 0.0);
        if rect.is_null() {
            return None;
        }
        // SAFETY: `ERect { top, left, bottom, right }` of the plug-in.
        let [top, left, bottom, right] = unsafe { [0, 1, 2, 3].map(|i| i32::from(*rect.add(i))) };
        Some(((right - left).max(1), (bottom - top).max(1)))
    }

    /// Opens the editor in `parent` (an `NSView` on macOS, an `HWND` on
    /// Windows).
    ///
    /// # Safety
    /// `parent` must stay valid until [`Self::editor_close`].
    pub unsafe fn editor_open(&self, parent: *mut c_void) -> bool {
        self.dispatch(OpCode::EditorOpen, 0, 0, parent, 0.0) != 0
    }

    pub fn editor_close(&self) {
        self.dispatch(OpCode::EditorClose, 0, 0, std::ptr::null_mut(), 0.0);
    }

    /// Editors of this generation redraw when the host idles them.
    pub fn editor_idle(&self) {
        self.dispatch(OpCode::EditorIdle, 0, 0, std::ptr::null_mut(), 0.0);
    }

    pub fn set_sample_rate(&self, rate: f32) {
        self.host
            .sample_rate
            .store(rate.to_bits(), Ordering::Relaxed);
        self.dispatch(OpCode::SetSampleRate, 0, 0, std::ptr::null_mut(), rate);
    }

    pub fn set_block_size(&self, size: usize) {
        self.host
            .block_size
            .store(size.min(u32::MAX as usize) as u32, Ordering::Relaxed);
        self.dispatch(
            OpCode::SetBlockSize,
            0,
            size as isize,
            std::ptr::null_mut(),
            0.0,
        );
    }

    /// Resumes and starts processing, or the reverse, in the order VST 2.4
    /// hosts keep.
    pub fn set_active(&self, active: bool) {
        if self.host.active.swap(active, Ordering::Relaxed) == active {
            return;
        }
        let null = std::ptr::null_mut();
        if active {
            self.dispatch(OpCode::MainsChanged, 0, 1, null, 0.0);
            self.dispatch(OpCode::StartProcess, 0, 0, null, 0.0);
        } else {
            self.dispatch(OpCode::StopProcess, 0, 0, null, 0.0);
            self.dispatch(OpCode::MainsChanged, 0, 0, null, 0.0);
        }
    }

    /// Whether the plug-in asked to be started over since the last call.
    pub fn take_restart(&self) -> bool {
        self.host.restart.swap(false, Ordering::Relaxed)
    }

    /// The size its editor asked for since the last call.
    pub fn take_window_size(&self) -> Option<(i32, i32)> {
        let size = self.host.window_size.swap(0, Ordering::Relaxed);
        (size != 0).then_some(((size >> 32) as i32, size as u32 as i32))
    }

    pub fn get_parameter(&self, index: i32) -> f32 {
        // SAFETY: callers keep `index` below `numParams`.
        unsafe { ((*self.effect).getParameter)(self.effect, index) }
    }

    pub fn set_parameter(&self, index: i32, value: f32) {
        // SAFETY: as above.
        unsafe { ((*self.effect).setParameter)(self.effect, index, value) }
    }

    pub fn parameter_name(&self, index: i32) -> String {
        self.dispatch_string(OpCode::GetParameterName, index)
    }

    pub fn parameter_label(&self, index: i32) -> String {
        self.dispatch_string(OpCode::GetParameterLabel, index)
    }

    pub fn parameter_display(&self, index: i32) -> String {
        self.dispatch_string(OpCode::GetParameterDisplay, index)
    }

    /// Sets a parameter from typed text, if the plug-in reads it.
    pub fn string_to_parameter(&self, index: i32, text: &str) -> bool {
        let mut buffer: Vec<u8> = text.bytes().filter(|&b| b != 0).take(255).collect();
        buffer.push(0);
        let ptr = buffer.as_mut_ptr().cast();
        self.dispatch(OpCode::StringToParameter, index, 0, ptr, 0.0) != 0
    }

    /// # Safety
    /// `inputs` and `outputs` hold `numInputs` and `numOutputs` channels of
    /// at least `frames` samples.
    pub unsafe fn process_replacing(
        &self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        frames: i32,
    ) {
        // SAFETY: passed on to the caller.
        unsafe { ((*self.effect).processReplacing)(self.effect, inputs, outputs, frames) }
    }
}

impl Drop for RawPlugin {
    fn drop(&mut self) {
        self.set_active(false);
        self.dispatch(OpCode::Close, 0, 0, std::ptr::null_mut(), 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_magic_spells_vstp() {
        assert_eq!(&MAGIC.to_be_bytes(), b"VstP");
    }
}
