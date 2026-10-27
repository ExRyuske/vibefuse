//! CoreAudio: the sample rate (the system driver owns it on macOS) and the
//! virtual devices of the mixer plug-in (`driver/macos`).

use std::ffi::{c_char, c_void};
use std::ptr;

use super::SampleRate;

pub type AudioObjectId = u32;
pub type OsStatus = i32;
type CfRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PropertyAddress {
    pub selector: u32,
    pub scope: u32,
    pub element: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ValueRange {
    minimum: f64,
    maximum: f64,
}

#[repr(C)]
pub struct AudioBuffer {
    pub channels: u32,
    pub bytes: u32,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct AudioBufferList {
    pub count: u32,
    pub buffers: [AudioBuffer; 1],
}

impl AudioBufferList {
    /// The buffers; `count` of them follow the header.
    ///
    /// # Safety
    /// `list` must point to a valid buffer list.
    pub unsafe fn buffers<'a>(list: *const AudioBufferList) -> &'a [AudioBuffer] {
        if list.is_null() {
            return &[];
        }
        // SAFETY: CoreAudio lays out `count` buffers after the header.
        unsafe {
            std::slice::from_raw_parts((&raw const (*list).buffers).cast(), (*list).count as usize)
        }
    }
}

pub type ListenerProc =
    unsafe extern "C" fn(AudioObjectId, u32, *const PropertyAddress, *mut c_void) -> OsStatus;
pub type IoProc = unsafe extern "C" fn(
    AudioObjectId,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut AudioBufferList,
    *const c_void,
    *mut c_void,
) -> OsStatus;
pub type IoProcId = Option<IoProc>;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> OsStatus;
    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
    fn AudioObjectSetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> OsStatus;
    fn AudioObjectAddPropertyListener(
        object: AudioObjectId,
        address: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> OsStatus;
    fn AudioObjectRemovePropertyListener(
        object: AudioObjectId,
        address: *const PropertyAddress,
        listener: ListenerProc,
        client: *mut c_void,
    ) -> OsStatus;
    fn AudioHardwareCreateAggregateDevice(
        description: CfRef,
        device: *mut AudioObjectId,
    ) -> OsStatus;
    fn AudioHardwareDestroyAggregateDevice(device: AudioObjectId) -> OsStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectId,
        proc_: IoProc,
        client: *mut c_void,
        id: *mut IoProcId,
    ) -> OsStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectId, id: IoProcId) -> OsStatus;
    fn AudioDeviceStart(device: AudioObjectId, id: IoProcId) -> OsStatus;
    fn AudioDeviceStop(device: AudioObjectId, id: IoProcId) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringGetLength(string: CfRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(string: CfRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFStringCreateWithBytes(
        allocator: CfRef,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> CfRef;
    fn CFDataCreate(allocator: CfRef, bytes: *const u8, length: isize) -> CfRef;
    fn CFDataGetLength(data: CfRef) -> isize;
    fn CFDataGetBytePtr(data: CfRef) -> *const u8;
    fn CFDataGetTypeID() -> usize;
    fn CFGetTypeID(object: CfRef) -> usize;
    fn CFNumberCreate(allocator: CfRef, kind: isize, value: *const c_void) -> CfRef;
    fn CFArrayCreate(
        allocator: CfRef,
        values: *const CfRef,
        count: isize,
        callbacks: CfRef,
    ) -> CfRef;
    fn CFDictionaryCreate(
        allocator: CfRef,
        keys: *const CfRef,
        values: *const CfRef,
        count: isize,
        key_callbacks: CfRef,
        value_callbacks: CfRef,
    ) -> CfRef;
    fn CFRelease(object: CfRef);
    static kCFTypeArrayCallBacks: [u8; 0];
    static kCFTypeDictionaryKeyCallBacks: [u8; 0];
    static kCFTypeDictionaryValueCallBacks: [u8; 0];
}

pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

pub const SYSTEM_OBJECT: AudioObjectId = 1;
pub const SCOPE_GLOBAL: u32 = fourcc(b"glob");
pub const SCOPE_INPUT: u32 = fourcc(b"inpt");
pub const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const ELEMENT_MAIN: u32 = 0;
pub const HARDWARE_DEVICES: u32 = fourcc(b"dev#");
pub const PLUGIN_LIST: u32 = fourcc(b"plg#");
/// Fires once the audio server is back after a restart.
pub const SERVICE_RESTARTED: u32 = fourcc(b"srst");
const BUNDLE_TO_PLUGIN: u32 = fourcc(b"bidp");
const UID_TO_DEVICE: u32 = fourcc(b"uidd");
const DEVICE_UID: u32 = fourcc(b"uid ");
const OBJECT_NAME: u32 = fourcc(b"lnam");
const TRANSPORT_TYPE: u32 = fourcc(b"tran");
const TRANSPORT_USB: u32 = fourcc(b"usb ");
pub const NOMINAL_RATE: u32 = fourcc(b"nsrt");
const AVAILABLE_RATES: u32 = fourcc(b"nsr#");
const STREAM_CONFIGURATION: u32 = fourcc(b"slay");
const BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT32: isize = 3;

pub fn address(selector: u32) -> PropertyAddress {
    scoped(selector, SCOPE_GLOBAL)
}

pub fn scoped(selector: u32, scope: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

fn check(status: OsStatus) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("CoreAudio error {status}"))
    }
}

/// An owned CoreFoundation object.
pub struct Cf(CfRef);

impl Cf {
    /// Takes ownership of a +1 reference; `None` for null.
    fn own(object: CfRef) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }

    pub fn string(text: &str) -> Self {
        // SAFETY: valid UTF-8 bytes of the given length.
        let object = unsafe {
            CFStringCreateWithBytes(ptr::null(), text.as_ptr(), text.len() as isize, UTF8, 0)
        };
        Self::own(object).expect("CFStringCreateWithBytes")
    }

    pub fn data(bytes: &[u8]) -> Self {
        // SAFETY: valid bytes of the given length.
        let object = unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as isize) };
        Self::own(object).expect("CFDataCreate")
    }

    pub fn number(value: i32) -> Self {
        // SAFETY: points to an i32, as the type says.
        let object =
            unsafe { CFNumberCreate(ptr::null(), NUMBER_SINT32, (&raw const value).cast()) };
        Self::own(object).expect("CFNumberCreate")
    }

    pub fn array(items: &[&Cf]) -> Self {
        let values: Vec<CfRef> = items.iter().map(|item| item.0).collect();
        // SAFETY: the values are live CF objects; the array retains them.
        let object = unsafe {
            CFArrayCreate(
                ptr::null(),
                values.as_ptr(),
                values.len() as isize,
                (&raw const kCFTypeArrayCallBacks).cast(),
            )
        };
        Self::own(object).expect("CFArrayCreate")
    }

    pub fn dictionary(entries: &[(&str, &Cf)]) -> Self {
        let keys: Vec<Cf> = entries.iter().map(|(key, _)| Cf::string(key)).collect();
        let key_refs: Vec<CfRef> = keys.iter().map(|key| key.0).collect();
        let values: Vec<CfRef> = entries.iter().map(|(_, value)| value.0).collect();
        // SAFETY: live CF keys and values; the dictionary retains them.
        let object = unsafe {
            CFDictionaryCreate(
                ptr::null(),
                key_refs.as_ptr(),
                values.as_ptr(),
                values.len() as isize,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            )
        };
        Self::own(object).expect("CFDictionaryCreate")
    }

    fn as_ptr(&self) -> CfRef {
        self.0
    }

    /// The bytes of a CFData.
    fn bytes(&self) -> Option<Vec<u8>> {
        // SAFETY: type-checked before reading the data.
        unsafe {
            if CFGetTypeID(self.0) != CFDataGetTypeID() {
                return None;
            }
            let length = CFDataGetLength(self.0) as usize;
            let bytes = CFDataGetBytePtr(self.0);
            Some(if length == 0 || bytes.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(bytes, length).to_vec()
            })
        }
    }

    fn text(&self) -> Option<String> {
        // SAFETY: the object is a CFString (callers ask for string properties).
        unsafe {
            let length = CFStringGetLength(self.0);
            let capacity = CFStringGetMaximumSizeForEncoding(length, UTF8) + 1;
            let mut buffer = vec![0 as c_char; capacity as usize];
            if CFStringGetCString(self.0, buffer.as_mut_ptr(), capacity, UTF8) == 0 {
                return None;
            }
            let bytes: Vec<u8> = buffer
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { CFRelease(self.0) }
    }
}

fn read_array<T: Copy + Default>(
    object: AudioObjectId,
    address: PropertyAddress,
) -> Option<Vec<T>> {
    let mut size = 0u32;
    // SAFETY: valid address and out-pointer for the size query.
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut size) };
    if status != 0 {
        return None;
    }
    let mut items = vec![T::default(); size as usize / size_of::<T>()];
    // SAFETY: the buffer holds `size` bytes as reported by CoreAudio.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            items.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return None;
    }
    items.truncate(size as usize / size_of::<T>());
    Some(items)
}

fn read_qualified<T: Copy + Default>(
    object: AudioObjectId,
    address: PropertyAddress,
    qualifier: Option<&Cf>,
) -> Option<T> {
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    let reference = qualifier.map(Cf::as_ptr);
    let (qualifier_size, qualifier_ptr) = match &reference {
        Some(reference) => (
            size_of::<CfRef>() as u32,
            (reference as *const CfRef).cast::<c_void>(),
        ),
        None => (0, ptr::null()),
    };
    // SAFETY: `value` is a properly sized buffer for this property and the
    // qualifier, if any, is a live CF reference.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            qualifier_size,
            qualifier_ptr,
            &mut size,
            (&raw mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

fn read_value<T: Copy + Default>(object: AudioObjectId, selector: u32) -> Option<T> {
    read_qualified(object, address(selector), None)
}

fn read_string(object: AudioObjectId, selector: u32) -> Option<String> {
    let string = read_value::<usize>(object, selector)? as CfRef;
    Cf::own(string)?.text()
}

fn write_value<T>(
    object: AudioObjectId,
    address: PropertyAddress,
    value: &T,
) -> Result<(), String> {
    // SAFETY: the property takes a value of type T.
    check(unsafe {
        AudioObjectSetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            size_of::<T>() as u32,
            (value as *const T).cast(),
        )
    })
}

/// Finds the CoreAudio device of the MiniFuse with this serial number.
pub fn find_device(serial: &str) -> Option<AudioObjectId> {
    let devices: Vec<AudioObjectId> = read_array(SYSTEM_OBJECT, address(HARDWARE_DEVICES))?;
    let candidates: Vec<(AudioObjectId, String)> = devices
        .into_iter()
        .filter(|&id| read_value::<u32>(id, TRANSPORT_TYPE) == Some(TRANSPORT_USB))
        .filter(|&id| read_string(id, OBJECT_NAME).is_some_and(|name| name.contains("MiniFuse")))
        .map(|id| (id, read_string(id, DEVICE_UID).unwrap_or_default()))
        .collect();
    candidates
        .iter()
        .find(|(_, uid)| !serial.is_empty() && uid.contains(serial))
        .or(candidates.first())
        .map(|&(id, _)| id)
}

pub fn device_uid(device: AudioObjectId) -> Option<String> {
    read_string(device, DEVICE_UID)
}

/// Devices that play sound, as (UID, name).
pub fn output_devices() -> Vec<(String, String)> {
    let devices: Vec<AudioObjectId> =
        read_array(SYSTEM_OBJECT, address(HARDWARE_DEVICES)).unwrap_or_default();
    devices
        .into_iter()
        .filter(|&id| stream_channels(id, SCOPE_OUTPUT).iter().sum::<u32>() > 0)
        .filter_map(|id| Some((read_string(id, DEVICE_UID)?, read_string(id, OBJECT_NAME)?)))
        .collect()
}

/// The device with this UID, also hidden ones.
pub fn device_by_uid(uid: &str) -> Option<AudioObjectId> {
    let uid = Cf::string(uid);
    let id: AudioObjectId = read_qualified(SYSTEM_OBJECT, address(UID_TO_DEVICE), Some(&uid))?;
    (id != 0).then_some(id)
}

/// The plug-in with this bundle identifier, if the audio server loaded it.
pub fn plugin(bundle: &str) -> Option<AudioObjectId> {
    let bundle = Cf::string(bundle);
    let id: AudioObjectId =
        read_qualified(SYSTEM_OBJECT, address(BUNDLE_TO_PLUGIN), Some(&bundle))?;
    (id != 0).then_some(id)
}

/// A custom property holding a CFData.
pub fn get_data(object: AudioObjectId, selector: u32) -> Result<Vec<u8>, String> {
    let mut reference: CfRef = ptr::null();
    let mut size = size_of::<CfRef>() as u32;
    // SAFETY: the property is a CFPropertyListRef we take ownership of.
    check(unsafe {
        AudioObjectGetPropertyData(
            object,
            &address(selector),
            0,
            ptr::null(),
            &mut size,
            (&raw mut reference).cast(),
        )
    })?;
    Cf::own(reference)
        .and_then(|data| data.bytes())
        .ok_or_else(|| "unexpected property type".to_owned())
}

pub fn set_data(object: AudioObjectId, selector: u32, bytes: &[u8]) -> Result<(), String> {
    let data = Cf::data(bytes);
    let reference = data.as_ptr();
    write_value(object, address(selector), &reference)
}

pub fn nominal_rate(device: AudioObjectId) -> Option<f64> {
    read_value::<f64>(device, NOMINAL_RATE)
}

pub fn set_nominal_rate(device: AudioObjectId, rate: f64) -> Result<(), String> {
    write_value(device, address(NOMINAL_RATE), &rate)
}

pub fn set_buffer_frames(device: AudioObjectId, frames: u32) -> Result<(), String> {
    write_value(device, address(BUFFER_FRAME_SIZE), &frames)
}

/// Channels per buffer of a device in one direction.
pub fn stream_channels(device: AudioObjectId, scope: u32) -> Vec<u32> {
    let address = scoped(STREAM_CONFIGURATION, scope);
    let words: Option<Vec<u64>> = read_array(device, address);
    let Some(words) = words else {
        return Vec::new();
    };
    let list = words.as_ptr().cast::<AudioBufferList>();
    // SAFETY: CoreAudio filled in a buffer list of the size it reported.
    unsafe { AudioBufferList::buffers(list) }
        .iter()
        .map(|buffer| buffer.channels)
        .collect()
}

pub fn add_listener(
    object: AudioObjectId,
    address: PropertyAddress,
    listener: ListenerProc,
    client: *mut c_void,
) -> Result<(), String> {
    // SAFETY: the listener and its client data outlive the registration.
    check(unsafe { AudioObjectAddPropertyListener(object, &address, listener, client) })
}

pub fn remove_listener(
    object: AudioObjectId,
    address: PropertyAddress,
    listener: ListenerProc,
    client: *mut c_void,
) {
    // SAFETY: removes a registration made with the same arguments.
    unsafe {
        AudioObjectRemovePropertyListener(object, &address, listener, client);
    }
}

pub fn create_aggregate(description: &Cf) -> Result<AudioObjectId, String> {
    let mut device = 0;
    // SAFETY: a valid description dictionary and out-pointer.
    check(unsafe { AudioHardwareCreateAggregateDevice(description.as_ptr(), &mut device) })?;
    Ok(device)
}

pub fn destroy_aggregate(device: AudioObjectId) {
    // SAFETY: plain call on a device we created.
    unsafe {
        AudioHardwareDestroyAggregateDevice(device);
    }
}

/// Runs `io` on the device's IO thread until [`stop_io`].
pub fn start_io(
    device: AudioObjectId,
    io: IoProc,
    client: *mut c_void,
) -> Result<IoProcId, String> {
    let mut id: IoProcId = None;
    // SAFETY: the callback and its client data outlive the IO proc.
    check(unsafe { AudioDeviceCreateIOProcID(device, io, client, &mut id) })?;
    // SAFETY: the IO proc was just created on this device.
    if let Err(error) = check(unsafe { AudioDeviceStart(device, id) }) {
        // SAFETY: as above.
        unsafe { AudioDeviceDestroyIOProcID(device, id) };
        return Err(error);
    }
    Ok(id)
}

pub fn stop_io(device: AudioObjectId, id: IoProcId) {
    // SAFETY: stops and removes an IO proc created by `start_io`; once
    // AudioDeviceStop returns, the callback no longer runs.
    unsafe {
        AudioDeviceStop(device, id);
        AudioDeviceDestroyIOProcID(device, id);
    }
}

pub fn sample_rate(serial: &str) -> Option<SampleRate> {
    let device = find_device(serial)?;
    let current = nominal_rate(device)?;
    let mut available: Vec<u32> = read_array::<ValueRange>(device, address(AVAILABLE_RATES))
        .unwrap_or_default()
        .into_iter()
        .filter(|range| range.minimum == range.maximum)
        .map(|range| range.minimum as u32)
        .collect();
    available.sort_unstable();
    available.dedup();
    Some(SampleRate {
        current: Some(current as u32),
        available,
        settable: true,
    })
}

pub fn set_sample_rate(serial: &str, rate: u32) -> Result<(), String> {
    let device = find_device(serial).ok_or("CoreAudio does not list the MiniFuse")?;
    set_nominal_rate(device, f64::from(rate))
}

pub mod hal_mixer {
    //! The mixer plug-in of this app on macOS (`driver/macos/VibeFuseMixer.c`).
    //!
    //! Its virtual devices stand in for those of Arturia's Windows driver, and
    //! it takes the same mixer properties, as the custom property `mfgn` of the
    //! plug-in object. Meters come back in the driver's reply format.

    use super::{AudioObjectId, fourcc};
    use crate::device::{Error, Result, SoftMixer, coreaudio};

    pub const BUNDLE_ID: &str = "cc.yeru.vibefuse.mixer";
    /// Plug-in version this app embeds (`MIXER_VERSION`).
    pub const VERSION: u32 = 1;
    /// UID of the hidden device that connects the mixer to the card.
    pub const BRIDGE_UID: &str = "VibeFuse:Bridge";

    pub const PROPERTY_STATUS: u32 = fourcc(b"mfst");
    pub const PROPERTY_GAINS: u32 = fourcc(b"mfgn");
    pub const PROPERTY_METERS: u32 = fourcc(b"mfmt");
    pub const PROPERTY_RUNNING: u32 = fourcc(b"mfrn");

    /// Windows driver property ids the plug-in has no use for.
    const METERS_SWITCH: u32 = 300;
    const METERS: u32 = 301;

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Status {
        pub version: u32,
        /// Virtual devices other programs are using.
        pub running: u32,
        /// The bridge to the card is running.
        pub bridge: bool,
        pub rate: f64,
    }

    fn word(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }

    pub fn status(plugin: AudioObjectId) -> std::result::Result<Status, String> {
        let bytes = coreaudio::get_data(plugin, PROPERTY_STATUS)?;
        let rate = bytes
            .get(16..24)
            .and_then(|b| b.try_into().ok())
            .map(f64::from_le_bytes);
        match (word(&bytes, 0), word(&bytes, 4), word(&bytes, 8), rate) {
            (Some(version), Some(running), Some(bridge), Some(rate)) => Ok(Status {
                version,
                running,
                bridge: bridge != 0,
                rate,
            }),
            _ => Err("malformed status".to_owned()),
        }
    }

    /// The loaded plug-in, if it speaks this app's version.
    pub fn find() -> Option<HalMixer> {
        let plugin = coreaudio::plugin(BUNDLE_ID)?;
        let status = status(plugin).ok()?;
        (status.version == VERSION).then_some(HalMixer { plugin })
    }

    pub struct HalMixer {
        plugin: AudioObjectId,
    }

    impl HalMixer {
        fn header(property: &[u8]) -> Option<u32> {
            word(property, 4).filter(|_| word(property, 0) == Some(0x4D))
        }
    }

    impl SoftMixer for HalMixer {
        fn set(&mut self, property: &[u8]) -> Result<()> {
            if Self::header(property) == Some(METERS_SWITCH) {
                // The plug-in always measures.
                return Ok(());
            }
            coreaudio::set_data(self.plugin, PROPERTY_GAINS, property).map_err(Error::gone)
        }

        /// Fills in the levels of a meter query from the plug-in's table.
        fn get(&mut self, property: &mut [u8]) -> Result<()> {
            if Self::header(property) != Some(METERS) {
                return Err(Error::new("the plug-in only answers meter queries"));
            }
            let table = coreaudio::get_data(self.plugin, PROPERTY_METERS).map_err(Error::gone)?;
            fill_meters(property, &table);
            Ok(())
        }
    }

    /// Copies the levels of the channels a query asks for from the plug-in's
    /// table of all meters; channels it does not know read as silence.
    fn fill_meters(query: &mut [u8], table: &[u8]) {
        let known = table.get(8..).unwrap_or_default().as_chunks::<8>().0;
        for entry in query[8..].as_chunks_mut::<8>().0 {
            let level = known
                .iter()
                .find(|k| k[..6] == entry[..6])
                .map_or((-12000i16).to_le_bytes(), |k| [k[6], k[7]]);
            entry[6..8].copy_from_slice(&level);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::device::sls;

        #[test]
        fn plugin_passes_its_own_tests() {
            let output = std::process::Command::new(concat!(env!("OUT_DIR"), "/test_mixer"))
                .output()
                .expect("the test program of the plug-in");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        /// The table in the order the plug-in writes it (`copy_meters`).
        #[rustfmt::skip]
        fn plugin_table(levels: impl Fn(u32, u8, u8) -> i16) -> Vec<u8> {
            let sources = [(0, 0), (0, 1), (1, 0), (1, 1), (1, 2), (1, 3), (2, 0), (2, 1), (2, 2), (2, 3)];
            let outputs = [(1, 0), (1, 1), (0, 2), (0, 3), (2, 0), (2, 1), (2, 2), (2, 3)];
            let mut table = vec![0x4D, 0, 0, 0, 0x2D, 1, 0, 0];
            for (direction, channels) in [(0u32, &sources[..]), (1, &outputs[..])] {
                for &(kind, index) in channels {
                    table.extend(direction.to_le_bytes());
                    table.extend([kind, index]);
                    table.extend(levels(direction, kind, index).to_le_bytes());
                }
            }
            table
        }

        #[test]
        fn plugin_meters_land_on_the_driver_channels() {
            let table = plugin_table(|direction, kind, index| {
                -(i16::from(direction as u8) * 1000 + i16::from(kind) * 100 + i16::from(index) * 10)
            });
            let mut query = sls::meters_query();
            fill_meters(&mut query, &table);
            let meters = sls::parse_meters(&query).unwrap();
            // Input 1 is hardware input 0; Virtual 7/8 virtual playback 2/3.
            assert_eq!(meters.sources[0], [-1.0, -1.0]);
            assert_eq!(meters.sources[1], [0.0, -0.1]);
            assert_eq!(meters.sources[2], [-1.2, -1.3]);
            assert_eq!(meters.sources[4], [-2.2, -2.3]);
            // Monitor & Phones on the device outputs, the mixes on recording.
            assert_eq!(meters.mixes[0], [-11.0, -11.1]);
            assert_eq!(meters.mixes[1], [-10.2, -10.3]);
            assert_eq!(meters.mixes[2], [-12.0, -12.1]);

            let mut query = sls::meters_query();
            fill_meters(&mut query, &table[..8]);
            let meters = sls::parse_meters(&query).unwrap();
            assert!(meters.is_silent());
        }
    }
}
