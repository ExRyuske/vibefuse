//! Windows transport through the API DLL of Arturia's USB audio driver.
//!
//! Windows lets only the owning driver talk to an audio interface. The
//! MiniFuse driver (built on the Thesycon USB Audio Class driver) exposes
//! `TUSBAUDIO_AudioControlRequestSet/Get`, which issue exactly the class
//! requests the device expects: `bmRequestType` 0x21/0xA1, entity 0.
//! The same DLL reaches the driver's software mixer (DSP properties, see
//! [`super::sls`]) and its ASIO settings.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use libloading::Library;

use super::{Asio, Bus, Device, DeviceInfo, Error, Found, Link, Result, SampleRate, protocol, sls};

const DLL_NAME: &str = "arturiaminifuseusbaudioapi_x64.dll";
const TIMEOUT_MS: u32 = 1000;
const SUCCESS: u32 = 0;
/// `TUsbAudioDeviceProperties` is three `u32` ids followed by UTF-16
/// strings; the buffer is generous so newer API revisions fit too.
const PROPERTIES_SIZE: usize = 4096;
const SERIAL_OFFSET: usize = 12;
const SERIAL_CHARS: usize = 128;

/// `TUSBAUDIO_GetASIOInstanceInfo` fills 196 bytes (API 5.72). Offsets of
/// the fields MiniFuse Control Center uses.
const ASIO_INFO_SIZE: usize = 196;
/// Identifier passed back to `TUSBAUDIO_SetASIOBufferPreferredSize`.
const ASIO_ID: usize = 0x04;
const ASIO_FLAGS: usize = 0x10;
const ASIO_SIZE: usize = 0x1C;
const ASIO_SIZE_COUNT: usize = 0x20;
const ASIO_SIZES: usize = 0x24;
const ASIO_MAX_SIZES: usize = (ASIO_INFO_SIZE - ASIO_SIZES) / 4;
/// Flag value of safe mode.
const ASIO_SAFE_MODE: u32 = 0x1_0000;

type Handle = usize;

struct Api {
    _library: Library,
    enumerate: unsafe extern "system" fn() -> u32,
    count: unsafe extern "system" fn() -> u32,
    open: unsafe extern "system" fn(u32, *mut Handle) -> u32,
    close: unsafe extern "system" fn(Handle) -> u32,
    properties: unsafe extern "system" fn(Handle, *mut u8) -> u32,
    control_set:
        unsafe extern "system" fn(Handle, u8, u8, u8, u8, *const c_void, u32, *mut u32, u32) -> u32,
    control_get:
        unsafe extern "system" fn(Handle, u8, u8, u8, u8, *mut c_void, u32, *mut u32, u32) -> u32,
    supported_rates: unsafe extern "system" fn(Handle, u32, *mut u32, *mut u32) -> u32,
    current_rate: unsafe extern "system" fn(Handle, *mut u32) -> u32,
    set_rate: unsafe extern "system" fn(Handle, u32) -> u32,
    // Newer driver releases only.
    dsp_get: Option<unsafe extern "system" fn(Handle, *mut c_void, u32) -> u32>,
    dsp_set: Option<unsafe extern "system" fn(Handle, *const c_void, u32) -> u32>,
    asio_info: Option<unsafe extern "system" fn(u32, *mut u8) -> u32>,
    asio_buffer: Option<unsafe extern "system" fn(u32, u32, u32, u32) -> u32>,
}

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("VIBEFUSE_DRIVER_DLL") {
        paths.push(PathBuf::from(path));
    }
    for base in ["ProgramW6432", "ProgramFiles"] {
        if let Some(dir) = std::env::var_os(base) {
            let dir = PathBuf::from(dir).join("Arturia");
            paths.push(dir.join("MiniFuseAudioDriver").join("x64").join(DLL_NAME));
            paths.push(dir.join("MiniFuse Audio Driver").join("x64").join(DLL_NAME));
        }
    }
    paths.push(PathBuf::from(DLL_NAME));
    paths
}

impl Api {
    fn load() -> Option<Api> {
        candidates().into_iter().find_map(|path| {
            // SAFETY: loading the vendor DLL runs its initialisation code;
            // it is the same DLL the official software uses.
            let library = unsafe { Library::new(&path) }.ok()?;
            // SAFETY: the symbol types match tusbaudioapi.h (API v4+).
            unsafe {
                Some(Api {
                    enumerate: *library.get(b"TUSBAUDIO_EnumerateDevices\0").ok()?,
                    count: *library.get(b"TUSBAUDIO_GetDeviceCount\0").ok()?,
                    open: *library.get(b"TUSBAUDIO_OpenDeviceByIndex\0").ok()?,
                    close: *library.get(b"TUSBAUDIO_CloseDevice\0").ok()?,
                    properties: *library.get(b"TUSBAUDIO_GetDeviceProperties\0").ok()?,
                    control_set: *library.get(b"TUSBAUDIO_AudioControlRequestSet\0").ok()?,
                    control_get: *library.get(b"TUSBAUDIO_AudioControlRequestGet\0").ok()?,
                    supported_rates: *library.get(b"TUSBAUDIO_GetSupportedSampleRates\0").ok()?,
                    current_rate: *library.get(b"TUSBAUDIO_GetCurrentSampleRate\0").ok()?,
                    set_rate: *library.get(b"TUSBAUDIO_SetSampleRate\0").ok()?,
                    dsp_get: library.get(b"TUSBAUDIO_GetDspProperty\0").ok().map(|f| *f),
                    dsp_set: library.get(b"TUSBAUDIO_SetDspProperty\0").ok().map(|f| *f),
                    asio_info: library
                        .get(b"TUSBAUDIO_GetASIOInstanceInfo\0")
                        .ok()
                        .map(|f| *f),
                    asio_buffer: library
                        .get(b"TUSBAUDIO_SetASIOBufferPreferredSize\0")
                        .ok()
                        .map(|f| *f),
                    _library: library,
                })
            }
        })
    }
}

/// `TSTATUS_DEVICE_REMOVED` and `TSTATUS_INVALID_HANDLE` from tstatus_codes.h.
const DEVICE_REMOVED: u32 = 0xE000_0071;
const INVALID_HANDLE: u32 = 0xE000_0048;

fn status(code: u32, what: &str) -> Result<()> {
    match code {
        SUCCESS => Ok(()),
        DEVICE_REMOVED | INVALID_HANDLE => Err(Error::gone(format!("{what}: the device is gone"))),
        _ => Err(Error::new(format!("{what}: driver error 0x{code:08X}"))),
    }
}

pub struct ThesyconBus {
    api: Option<Arc<Api>>,
}

impl ThesyconBus {
    pub fn new() -> Self {
        Self { api: None }
    }
}

struct Properties {
    vendor: u32,
    product: u32,
    revision: u32,
    serial: String,
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

fn properties(api: &Api, handle: Handle) -> Result<Properties> {
    let mut bytes = vec![0u8; PROPERTIES_SIZE];
    // SAFETY: the buffer is larger than any known struct revision.
    status(
        unsafe { (api.properties)(handle, bytes.as_mut_ptr()) },
        "device properties",
    )?;
    let units: Vec<u16> = bytes[SERIAL_OFFSET..SERIAL_OFFSET + SERIAL_CHARS * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect();
    Ok(Properties {
        vendor: read_u32(&bytes, 0),
        product: read_u32(&bytes, 4),
        revision: read_u32(&bytes, 8),
        serial: String::from_utf16_lossy(&units),
    })
}

impl Bus for ThesyconBus {
    fn scan(&mut self) -> Result<Found> {
        if self.api.is_none() {
            match Api::load() {
                Some(api) => self.api = Some(Arc::new(api)),
                None => {
                    return Ok(Found::Unavailable(crate::tr!(
                        "Install Arturia's MiniFuse Audio Driver"
                    )));
                }
            }
        }
        let api = self.api.clone().expect("api was just loaded");
        // SAFETY: plain calls into the loaded DLL.
        unsafe {
            status((api.enumerate)(), "enumeration")?;
            for index in 0..(api.count)() {
                let mut handle: Handle = 0;
                if (api.open)(index, &mut handle) != SUCCESS {
                    continue;
                }
                match properties(&api, handle) {
                    Ok(p)
                        if p.vendor == u32::from(protocol::VENDOR_ID)
                            && p.product == u32::from(protocol::PRODUCT_ID) =>
                    {
                        let info = DeviceInfo {
                            product: "MiniFuse 1".to_owned(),
                            serial: p.serial,
                            firmware: protocol::firmware_version(p.revision as u16),
                        };
                        let mut device = ThesyconDevice {
                            api,
                            handle,
                            info,
                            mixer: false,
                        };
                        let mut reply = sls::version_query();
                        device.mixer =
                            device.mixer_get(&mut reply).is_ok() && sls::is_supported(&reply);
                        return Ok(Found::Audio(Box::new(device)));
                    }
                    _ => {
                        (api.close)(handle);
                    }
                }
            }
        }
        Ok(Found::Nothing)
    }

    fn leave_storage_mode(&mut self) -> Result<()> {
        Err(Error::new(crate::tr!(
            "Reconnect the MiniFuse 1 to switch it to audio mode"
        )))
    }
}

struct ThesyconDevice {
    api: Arc<Api>,
    handle: Handle,
    info: DeviceInfo,
    mixer: bool,
}

impl ThesyconDevice {
    fn asio_info(&self) -> Option<[u8; ASIO_INFO_SIZE]> {
        let get = self.api.asio_info?;
        let mut info = [0u8; ASIO_INFO_SIZE];
        // SAFETY: the buffer has the size the API fills.
        (unsafe { get(0, info.as_mut_ptr()) } == SUCCESS).then_some(info)
    }
}

impl Drop for ThesyconDevice {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenDeviceByIndex and is closed once.
        unsafe { (self.api.close)(self.handle) };
    }
}

struct DriverLink<'a> {
    api: &'a Api,
    handle: Handle,
}

impl Link for DriverLink<'_> {
    fn get(&self, request: u8, value: u16, buf: &mut [u8]) -> Result<usize> {
        let mut transferred = 0u32;
        let [selector, channel] = value.to_be_bytes();
        // SAFETY: `buf` outlives the call and its length is passed along.
        let code = unsafe {
            (self.api.control_get)(
                self.handle,
                0,
                request,
                selector,
                channel,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut transferred,
                TIMEOUT_MS,
            )
        };
        status(code, "read")?;
        Ok(transferred as usize)
    }

    fn set(&self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        let mut transferred = 0u32;
        let [selector, channel] = value.to_be_bytes();
        // SAFETY: `data` outlives the call and its length is passed along.
        let code = unsafe {
            (self.api.control_set)(
                self.handle,
                0,
                request,
                selector,
                channel,
                data.as_ptr().cast(),
                data.len() as u32,
                &mut transferred,
                TIMEOUT_MS,
            )
        };
        status(code, "write")
    }
}

impl Device for ThesyconDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn is_live(&self) -> bool {
        true
    }

    fn session(&mut self, f: &mut dyn FnMut(&dyn Link) -> Result<()>) -> Result<()> {
        f(&DriverLink {
            api: &self.api,
            handle: self.handle,
        })
    }

    fn sample_rate(&mut self) -> Option<SampleRate> {
        let mut rates = [0u32; 32];
        let mut count = 0u32;
        let mut current = 0u32;
        // SAFETY: the array length is passed as the maximum count.
        unsafe {
            if (self.api.supported_rates)(
                self.handle,
                rates.len() as u32,
                rates.as_mut_ptr(),
                &mut count,
            ) != SUCCESS
                || (self.api.current_rate)(self.handle, &mut current) != SUCCESS
            {
                return None;
            }
        }
        let mut available = rates[..(count as usize).min(rates.len())].to_vec();
        available.sort_unstable();
        Some(SampleRate {
            current: Some(current),
            available,
            settable: true,
        })
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<()> {
        // SAFETY: plain call with an open handle.
        status(
            unsafe { (self.api.set_rate)(self.handle, rate) },
            "sample rate",
        )
    }

    fn has_mixer(&self) -> bool {
        self.mixer
    }

    fn mixer_set(&mut self, property: &[u8]) -> Result<()> {
        let set = self
            .api
            .dsp_set
            .ok_or_else(|| Error::new("the driver has no software mixer"))?;
        // SAFETY: the buffer outlives the call and its length is passed along.
        let code = unsafe { set(self.handle, property.as_ptr().cast(), property.len() as u32) };
        status(code, "mixer")
    }

    fn mixer_get(&mut self, property: &mut [u8]) -> Result<()> {
        let get = self
            .api
            .dsp_get
            .ok_or_else(|| Error::new("the driver has no software mixer"))?;
        // SAFETY: as above; the driver answers in place.
        let code = unsafe {
            get(
                self.handle,
                property.as_mut_ptr().cast(),
                property.len() as u32,
            )
        };
        status(code, "mixer")
    }

    fn asio(&mut self) -> Option<Asio> {
        let info = self.asio_info()?;
        let count = (read_u32(&info, ASIO_SIZE_COUNT) as usize).min(ASIO_MAX_SIZES);
        let sizes: Vec<u32> = (0..count)
            .map(|i| read_u32(&info, ASIO_SIZES + 4 * i))
            .filter(|&size| size > 0)
            .collect();
        (!sizes.is_empty()).then(|| Asio {
            sizes,
            size: read_u32(&info, ASIO_SIZE),
            safe_mode: read_u32(&info, ASIO_FLAGS) == ASIO_SAFE_MODE,
        })
    }

    fn set_asio(&mut self, size: u32, safe_mode: bool) -> Result<()> {
        let set = self
            .api
            .asio_buffer
            .ok_or_else(|| Error::new("the driver has no ASIO settings"))?;
        let info = self
            .asio_info()
            .ok_or_else(|| Error::new("the driver sent no ASIO settings"))?;
        let flags = if safe_mode { ASIO_SAFE_MODE } else { 0 };
        // SAFETY: plain call; the arguments are the ones the official app passes.
        status(
            unsafe { set(0, read_u32(&info, ASIO_ID), size, flags) },
            "ASIO settings",
        )
    }
}
