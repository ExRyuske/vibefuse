//! The effects' audio path on Windows: an ASIO stream of Arturia's driver
//! that takes Input 1, runs the chain and plays the result on the pair of
//! outputs the user picked. Outputs 3/4 come back from the card as the
//! mixer's "Virtual 3/4" source.
//!
//! ASIO is a COM-style table of methods, declared here directly (the SDK's
//! C++ glue would need Visual Studio to build). One duplex callback gives
//! input and output of the same block: no ring buffer, no extra delay. The
//! buffer size is the driver's, set in VibeFuse's settings.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoFreeUnusedLibraries,
    CoInitializeEx,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
};
use windows_sys::core::GUID;

use super::Result;
use super::rack;

const ASE_OK: i32 = 0;
const ASIO_TRUE: i32 = 1;

/// Little-endian sample types (the big-endian ones never occur on Windows).
mod sample_type {
    pub const INT16: i32 = 16;
    pub const INT24: i32 = 17;
    pub const INT32: i32 = 18;
    pub const FLOAT32: i32 = 19;
    pub const FLOAT64: i32 = 20;
}

/// `asioMessage` selectors, numbered as in `asio.h`.
mod message {
    pub const SELECTOR_SUPPORTED: i32 = 1;
    pub const ENGINE_VERSION: i32 = 2;
    pub const RESET_REQUEST: i32 = 3;
    pub const BUFFER_SIZE_CHANGE: i32 = 4;
    pub const RESYNC_REQUEST: i32 = 5;
    pub const LATENCIES_CHANGED: i32 = 6;
    pub const SUPPORTS_TIME_INFO: i32 = 7;
}

#[repr(C)]
struct ChannelInfo {
    channel: i32,
    is_input: i32,
    is_active: i32,
    group: i32,
    sample_type: i32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BufferInfo {
    is_input: i32,
    channel: i32,
    /// The two halves of the double buffer.
    buffers: [*mut c_void; 2],
}

/// `ASIOTime`: only passed back.
#[repr(C)]
struct AsioTime {
    _opaque: [u8; 0],
}

#[repr(C)]
struct Callbacks {
    buffer_switch: extern "system" fn(index: i32, direct: i32),
    sample_rate_changed: extern "system" fn(rate: f64),
    message:
        extern "system" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    buffer_switch_time_info:
        extern "system" fn(time: *mut AsioTime, index: i32, direct: i32) -> *mut AsioTime,
}

/// The driver's method table, in the order of the `IASIO` class: a binary
/// contract, the first three from `IUnknown`.
#[repr(C)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(*mut Iasio, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Iasio) -> u32,
    release: unsafe extern "system" fn(*mut Iasio) -> u32,
    init: unsafe extern "system" fn(*mut Iasio, *mut c_void) -> i32,
    get_driver_name: unsafe extern "system" fn(*mut Iasio, *mut u8),
    get_driver_version: unsafe extern "system" fn(*mut Iasio) -> i32,
    get_error_message: unsafe extern "system" fn(*mut Iasio, *mut u8),
    start: unsafe extern "system" fn(*mut Iasio) -> i32,
    stop: unsafe extern "system" fn(*mut Iasio) -> i32,
    get_channels: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32) -> i32,
    get_latencies: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32) -> i32,
    get_buffer_size:
        unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32, *mut i32, *mut i32) -> i32,
    can_sample_rate: unsafe extern "system" fn(*mut Iasio, f64) -> i32,
    get_sample_rate: unsafe extern "system" fn(*mut Iasio, *mut f64) -> i32,
    set_sample_rate: unsafe extern "system" fn(*mut Iasio, f64) -> i32,
    get_clock_sources: unsafe extern "system" fn(*mut Iasio, *mut c_void, *mut i32) -> i32,
    set_clock_source: unsafe extern "system" fn(*mut Iasio, i32) -> i32,
    get_sample_position: unsafe extern "system" fn(*mut Iasio, *mut c_void, *mut c_void) -> i32,
    get_channel_info: unsafe extern "system" fn(*mut Iasio, *mut ChannelInfo) -> i32,
    create_buffers:
        unsafe extern "system" fn(*mut Iasio, *mut BufferInfo, i32, i32, *const Callbacks) -> i32,
    dispose_buffers: unsafe extern "system" fn(*mut Iasio) -> i32,
    control_panel: unsafe extern "system" fn(*mut Iasio) -> i32,
    future: unsafe extern "system" fn(*mut Iasio, i32, *mut c_void) -> i32,
    output_ready: unsafe extern "system" fn(*mut Iasio) -> i32,
}

#[repr(C)]
struct Iasio {
    vtbl: *const Vtbl,
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, braces optional.
fn parse_clsid(text: &str) -> Option<GUID> {
    let text = text.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = text.split('-').collect();
    if parts.iter().map(|p| p.len()).ne([8, 4, 4, 4, 12]) {
        return None;
    }
    let tail = format!("{}{}", parts[3], parts[4]);
    let mut data4 = [0u8; 8];
    for (i, byte) in data4.iter_mut().enumerate() {
        *byte = u8::from_str_radix(tail.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(GUID {
        data1: u32::from_str_radix(parts[0], 16).ok()?,
        data2: u16::from_str_radix(parts[1], 16).ok()?,
        data3: u16::from_str_radix(parts[2], 16).ok()?,
        data4,
    })
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// ASIO drivers by name and class id, from `HKLM\SOFTWARE\ASIO`.
fn drivers() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut root: HKEY = std::ptr::null_mut();
    // SAFETY: registry calls with buffers of the sizes passed; every key
    // opened is closed.
    unsafe {
        let path = wide(r"SOFTWARE\ASIO");
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut root) != ERROR_SUCCESS
        {
            return found;
        }
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut length = name.len() as u32;
            let enumerated = RegEnumKeyExW(
                root,
                index,
                name.as_mut_ptr(),
                &mut length,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if enumerated != ERROR_SUCCESS {
                break;
            }
            let name = String::from_utf16_lossy(&name[..length as usize]);
            let mut key: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(root, wide(&name).as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS {
                continue;
            }
            let mut value = [0u16; 64];
            let mut size = (value.len() * 2) as u32;
            let read = RegQueryValueExW(
                key,
                wide("CLSID").as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut size,
            );
            RegCloseKey(key);
            if read == ERROR_SUCCESS {
                let clsid: Vec<u16> = value[..(size as usize / 2).min(value.len())]
                    .iter()
                    .copied()
                    .take_while(|&c| c != 0)
                    .collect();
                found.push((name, String::from_utf16_lossy(&clsid)));
            }
        }
        RegCloseKey(root);
    }
    found
}

/// The MiniFuse's driver: Arturia names it after the card.
fn minifuse_driver() -> Option<(String, String)> {
    let drivers = drivers();
    let named = |word: &str| {
        drivers
            .iter()
            .find(|(name, _)| name.to_lowercase().contains(word))
            .cloned()
    };
    named("minifuse").or_else(|| named("arturia"))
}

/// An open driver.
struct Driver {
    ptr: *mut Iasio,
    name: String,
}

impl Driver {
    fn open() -> Result<Self> {
        let (name, clsid) = minifuse_driver()
            .ok_or_else(|| crate::tr!("Arturia's ASIO driver is not installed"))?;
        let clsid = parse_clsid(&clsid).ok_or_else(|| format!("{name}: bad class id {clsid}"))?;
        let mut object: *mut c_void = std::ptr::null_mut();
        // SAFETY: COM for this thread (again is harmless); ASIO passes the
        // class id as the interface id too, as its SDK does.
        let created = unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            CoCreateInstance(
                &clsid,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &clsid,
                &mut object,
            )
        };
        if created < 0 || object.is_null() {
            return Err(format!("{name}: did not start ({created:#010X})"));
        }
        let driver = Self {
            ptr: object.cast(),
            name,
        };
        // SAFETY: a live driver; no window to parent its panel to.
        if unsafe { (driver.vtbl().init)(driver.ptr, std::ptr::null_mut()) } != ASIO_TRUE {
            return Err(format!("{}: {}", driver.name, driver.error()));
        }
        Ok(driver)
    }

    fn vtbl(&self) -> &Vtbl {
        // SAFETY: a live COM object.
        unsafe { &*(*self.ptr).vtbl }
    }

    fn error(&self) -> String {
        let mut buffer = [0u8; 128];
        // SAFETY: the 124 bytes the spec allows, and then some.
        unsafe { (self.vtbl().get_error_message)(self.ptr, buffer.as_mut_ptr()) };
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    }

    fn check(&self, code: i32, what: &str) -> Result<()> {
        if code == ASE_OK {
            Ok(())
        } else {
            Err(format!("{}: {what} ({code}) {}", self.name, self.error()))
        }
    }

    fn channels(&self) -> Result<(usize, usize)> {
        let (mut inputs, mut outputs) = (0, 0);
        // SAFETY: outputs of the right type.
        let code = unsafe { (self.vtbl().get_channels)(self.ptr, &mut inputs, &mut outputs) };
        self.check(code, "getChannels")?;
        Ok((inputs.max(0) as usize, outputs.max(0) as usize))
    }

    fn preferred_buffer(&self) -> Result<usize> {
        let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
        // SAFETY: as above.
        let code = unsafe {
            (self.vtbl().get_buffer_size)(
                self.ptr,
                &mut min,
                &mut max,
                &mut preferred,
                &mut granularity,
            )
        };
        self.check(code, "getBufferSize")?;
        Ok(preferred.max(1) as usize)
    }

    fn sample_rate(&self) -> Result<f64> {
        let mut rate = 0.0;
        // SAFETY: as above.
        let code = unsafe { (self.vtbl().get_sample_rate)(self.ptr, &mut rate) };
        self.check(code, "getSampleRate")?;
        Ok(rate)
    }

    fn channel(&self, channel: usize, input: bool) -> Result<(String, i32)> {
        let mut info = ChannelInfo {
            channel: channel as i32,
            is_input: i32::from(input),
            is_active: 0,
            group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        // SAFETY: as above.
        let code = unsafe { (self.vtbl().get_channel_info)(self.ptr, &mut info) };
        self.check(code, "getChannelInfo")?;
        let end = info.name.iter().position(|&c| c == 0).unwrap_or(32);
        Ok((
            String::from_utf8_lossy(&info.name[..end]).into_owned(),
            info.sample_type,
        ))
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // SAFETY: our reference, released once; then the driver's library
        // may unload, so the card is free for others.
        unsafe {
            (self.vtbl().release)(self.ptr);
            CoFreeUnusedLibraries();
        }
    }
}

/// Converts `frames` samples of the driver's type to `f32`.
///
/// # Safety
/// `from` holds `frames` samples of `kind`.
unsafe fn read(from: *const u8, kind: i32, to: &mut [f32]) {
    let frames = to.len();
    let size = sample_size(kind);
    // SAFETY: passed on to the caller.
    let bytes = unsafe { std::slice::from_raw_parts(from, frames * size) };
    match kind {
        sample_type::INT16 => {
            for (out, b) in to.iter_mut().zip(bytes.as_chunks::<2>().0) {
                *out = f32::from(i16::from_le_bytes(*b)) / 32_768.0;
            }
        }
        sample_type::INT24 => {
            for (out, b) in to.iter_mut().zip(bytes.as_chunks::<3>().0) {
                *out = (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0;
            }
        }
        sample_type::INT32 => {
            for (out, b) in to.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *out = i32::from_le_bytes(*b) as f32 / 2_147_483_648.0;
            }
        }
        sample_type::FLOAT32 => {
            for (out, b) in to.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *out = f32::from_le_bytes(*b);
            }
        }
        _ => {
            for (out, b) in to.iter_mut().zip(bytes.as_chunks::<8>().0) {
                *out = f64::from_le_bytes(*b) as f32;
            }
        }
    }
}

/// Converts back to the driver's type, clipping.
///
/// # Safety
/// `to` has room for `from.len()` samples of `kind`.
unsafe fn write(from: &[f32], kind: i32, to: *mut u8) {
    let size = sample_size(kind);
    // SAFETY: passed on to the caller.
    let bytes = unsafe { std::slice::from_raw_parts_mut(to, from.len() * size) };
    let chunks = bytes.chunks_exact_mut(size);
    for (&sample, out) in from.iter().zip(chunks) {
        let sample = sample.clamp(-1.0, 1.0);
        match kind {
            sample_type::INT16 => out.copy_from_slice(&((sample * 32_767.0) as i16).to_le_bytes()),
            sample_type::INT24 => {
                let value = (sample * 8_388_607.0) as i32;
                out.copy_from_slice(&value.to_le_bytes()[..3]);
            }
            sample_type::INT32 => {
                out.copy_from_slice(&((f64::from(sample) * 2_147_483_647.0) as i32).to_le_bytes());
            }
            sample_type::FLOAT32 => out.copy_from_slice(&sample.to_le_bytes()),
            _ => out.copy_from_slice(&f64::from(sample).to_le_bytes()),
        }
    }
}

fn sample_size(kind: i32) -> usize {
    match kind {
        sample_type::INT16 => 2,
        sample_type::INT24 => 3,
        sample_type::FLOAT64 => 8,
        _ => 4,
    }
}

/// What the callback works with; it lives while the stream runs.
struct Context {
    input: [*mut c_void; 2],
    outputs: Vec<[*mut c_void; 2]>,
    kind: i32,
    rate: f64,
    driver: *mut Iasio,
    output_ready: bool,
    samples: Vec<f32>,
}

/// ASIO passes callbacks no context: the running stream is found here.
static ACTIVE: AtomicPtr<Context> = AtomicPtr::new(std::ptr::null_mut());
/// Set while the callback runs, so the context is not freed under it. The
/// two sides write one flag and read the other: only `SeqCst` orders that.
static IN_CALLBACK: AtomicBool = AtomicBool::new(false);
/// The driver asked for new buffers (another buffer size or rate).
static RESET: AtomicBool = AtomicBool::new(false);

extern "system" fn buffer_switch(index: i32, _direct: i32) {
    IN_CALLBACK.store(true, Ordering::SeqCst);
    let context = ACTIVE.load(Ordering::SeqCst);
    if !context.is_null() {
        // SAFETY: not freed while the flag is up.
        unsafe { process(&mut *context, index as usize & 1) };
    }
    IN_CALLBACK.store(false, Ordering::SeqCst);
}

extern "system" fn buffer_switch_time_info(
    time: *mut AsioTime,
    index: i32,
    direct: i32,
) -> *mut AsioTime {
    buffer_switch(index, direct);
    time
}

extern "system" fn sample_rate_changed(_rate: f64) {
    RESET.store(true, Ordering::Release);
}

extern "system" fn asio_message(
    selector: i32,
    value: i32,
    _message: *mut c_void,
    _opt: *mut f64,
) -> i32 {
    match selector {
        message::SELECTOR_SUPPORTED => i32::from(matches!(
            value,
            message::ENGINE_VERSION
                | message::RESET_REQUEST
                | message::BUFFER_SIZE_CHANGE
                | message::RESYNC_REQUEST
                | message::LATENCIES_CHANGED
                | message::SUPPORTS_TIME_INFO
        )),
        message::ENGINE_VERSION => 2,
        message::RESET_REQUEST | message::BUFFER_SIZE_CHANGE => {
            RESET.store(true, Ordering::Release);
            1
        }
        message::RESYNC_REQUEST | message::LATENCIES_CHANGED | message::SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}

/// One block, on the driver's thread.
///
/// # Safety
/// Only from `buffer_switch`, with a live context.
unsafe fn process(context: &mut Context, half: usize) {
    let kind = context.kind;
    // SAFETY: the driver's buffers hold one block of its type.
    unsafe { read(context.input[half].cast(), kind, &mut context.samples) };
    // Unprocessed, the microphone still goes on: the mixer takes Input 1
    // from here while the stream runs.
    rack::process(&mut context.samples, context.rate);
    for output in &context.outputs {
        // SAFETY: as above.
        unsafe { write(&context.samples, kind, output[half].cast()) };
    }
    if context.output_ready {
        // SAFETY: the driver lives while the stream does.
        unsafe { ((*(*context.driver).vtbl).output_ready)(context.driver) };
    }
}

/// The running stream: the effects are heard while it lives.
pub struct Stream {
    driver: Driver,
    context: *mut Context,
    rate: f64,
}

/// Input 1.
const INPUT: usize = 0;

impl Stream {
    /// Plays on the output `first` and the one after it, if there is one.
    pub fn start(first: usize) -> Result<Self> {
        let driver = Driver::open()?;
        let (inputs, outputs) = driver.channels()?;
        if inputs == INPUT || first >= outputs {
            return Err(format!("{}: no such output", driver.name));
        }
        let pair: Vec<usize> = (first..outputs.min(first + 2)).collect();
        let rate = driver.sample_rate()?;
        let frames = driver.preferred_buffer()?;
        let (_, kind) = driver.channel(INPUT, true)?;
        if !(sample_type::INT16..=sample_type::FLOAT64).contains(&kind) {
            return Err(format!("{}: sample type {kind}", driver.name));
        }
        let mut infos: Vec<BufferInfo> = std::iter::once((true, INPUT))
            .chain(pair.iter().map(|&o| (false, o)))
            .map(|(input, channel)| BufferInfo {
                is_input: i32::from(input),
                channel: channel as i32,
                buffers: [std::ptr::null_mut(); 2],
            })
            .collect();
        // The driver keeps using the callbacks until `disposeBuffers`: they
        // stay for the program's life, a few bytes per start.
        let callbacks: &'static Callbacks = Box::leak(Box::new(Callbacks {
            buffer_switch,
            sample_rate_changed,
            message: asio_message,
            buffer_switch_time_info,
        }));
        // SAFETY: the infos are filled in place; the callbacks live on.
        let code = unsafe {
            (driver.vtbl().create_buffers)(
                driver.ptr,
                infos.as_mut_ptr(),
                infos.len() as i32,
                frames as i32,
                callbacks,
            )
        };
        driver.check(code, "createBuffers")?;
        RESET.store(false, Ordering::Release);
        let context = Box::into_raw(Box::new(Context {
            input: infos[0].buffers,
            outputs: infos[1..].iter().map(|i| i.buffers).collect(),
            kind,
            rate,
            driver: driver.ptr,
            // SAFETY: a plain query of the driver.
            output_ready: unsafe { (driver.vtbl().output_ready)(driver.ptr) } == ASE_OK,
            samples: vec![0.0; frames],
        }));
        // Before `start`: the first block may come right away.
        ACTIVE.store(context, Ordering::SeqCst);
        // SAFETY: buffers made, context published.
        let code = unsafe { (driver.vtbl().start)(driver.ptr) };
        if let Err(error) = driver.check(code, "start") {
            ACTIVE.store(std::ptr::null_mut(), Ordering::SeqCst);
            // SAFETY: never started, so no callback holds the context.
            unsafe {
                (driver.vtbl().dispose_buffers)(driver.ptr);
                drop(Box::from_raw(context));
            }
            return Err(error);
        }
        Ok(Self {
            driver,
            context,
            rate,
        })
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The driver wants the stream made again (its buffer size or rate
    /// changed).
    pub fn needs_restart(&self) -> bool {
        RESET.load(Ordering::Acquire)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stop, unpublish, wait out a callback in progress, then free.
        unsafe { (self.driver.vtbl().stop)(self.driver.ptr) };
        ACTIVE.store(std::ptr::null_mut(), Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut quiet = true;
        while IN_CALLBACK.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                quiet = false;
                break;
            }
            std::thread::yield_now();
        }
        // SAFETY: the driver's buffers go back in any case.
        unsafe { (self.driver.vtbl().dispose_buffers)(self.driver.ptr) };
        if quiet {
            // SAFETY: no callback can reach the context any more.
            unsafe { drop(Box::from_raw(self.context)) };
        }
        // Otherwise a hung driver may still read it: leaking beats a crash.
    }
}

/// The driver's outputs in pairs, by the first channel of each.
pub fn outputs() -> Result<Vec<(usize, String)>> {
    let driver = Driver::open()?;
    let (_, outputs) = driver.channels()?;
    let names: Vec<String> = (0..outputs)
        .map(|c| driver.channel(c, false).map(|(name, _)| name))
        .collect::<Result<_>>()?;
    Ok(names
        .chunks(2)
        .enumerate()
        .map(|(pair, names)| (pair * 2, names.join(" / ")))
        .collect())
}

/// The driver's channels, for `vibefuse --dump`.
pub fn describe() -> Result<String> {
    let driver = Driver::open()?;
    let (inputs, outputs) = driver.channels()?;
    let names = |count: usize, input: bool| -> Vec<String> {
        (0..count)
            .map(|c| {
                driver
                    .channel(c, input)
                    .map_or_else(|e| e, |(name, _)| name)
            })
            .collect()
    };
    Ok(format!(
        "{} · {} Hz · buffer {} · in: {} · out: {}",
        driver.name,
        driver.sample_rate()?,
        driver.preferred_buffer()?,
        names(inputs, true).join(", "),
        names(outputs, false).join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ids_are_read_with_or_without_braces() {
        let guid = parse_clsid("{232685C6-6548-49D8-846D-4141A3EF7560}").unwrap();
        assert_eq!(
            (guid.data1, guid.data2, guid.data3, guid.data4),
            (
                0x232685C6,
                0x6548,
                0x49D8,
                [0x84, 0x6D, 0x41, 0x41, 0xA3, 0xEF, 0x75, 0x60]
            )
        );
        assert!(parse_clsid("232685C6-6548-49D8-846D-4141A3EF7560").is_some());
        assert!(parse_clsid("{232685C6-6548-49D8-846D}").is_none());
    }

    #[test]
    fn samples_survive_every_type() {
        for kind in [
            sample_type::INT16,
            sample_type::INT24,
            sample_type::INT32,
            sample_type::FLOAT32,
            sample_type::FLOAT64,
        ] {
            let samples = [0.5f32, -0.25, 0.0, 0.999];
            let mut bytes = vec![0u8; samples.len() * sample_size(kind)];
            let mut back = [0.0f32; 4];
            // SAFETY: buffers of the sizes the types need.
            unsafe {
                write(&samples, kind, bytes.as_mut_ptr());
                read(bytes.as_ptr(), kind, &mut back);
            }
            for (a, b) in samples.iter().zip(back) {
                assert!((a - b).abs() < 1e-3, "type {kind}: {a} became {b}");
            }
        }
    }

    #[test]
    fn selectors_follow_the_sdk_numbering() {
        let call = |selector, value| {
            asio_message(selector, value, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(call(2, 0), 2);
        assert_eq!(call(1, 3), 1);
        assert_eq!(call(3, 0), 1);
        assert!(RESET.swap(false, Ordering::AcqRel));
        assert_eq!(call(5, 0), 1);
        assert!(!RESET.load(Ordering::Acquire));
    }
}
