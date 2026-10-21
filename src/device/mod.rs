//! MiniFuse 1 access: the protocol with its state model, and the transports
//! of each platform.

pub mod demo;
pub mod protocol;
pub mod sls;
pub mod worker;

#[cfg(target_os = "macos")]
pub mod coreaudio;
#[cfg(target_os = "macos")]
pub use coreaudio::hal_mixer;
#[cfg(not(windows))]
mod libusb;
#[cfg(target_os = "linux")]
pub mod pipewire;
#[cfg(windows)]
mod thesycon;

use std::fmt;
use std::thread;
use std::time::Duration;

use protocol::{Param, RawState, Write};

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    /// The device is gone and must be rediscovered.
    pub gone: bool,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            gone: false,
        }
    }

    pub fn gone(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            gone: true,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub product: String,
    pub serial: String,
    pub firmware: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleRate {
    /// `None` when the system does not report it (Linux while idle).
    pub current: Option<u32>,
    pub available: Vec<u32>,
    pub settable: bool,
}

/// ASIO settings of the Windows driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asio {
    /// Buffer sizes in samples the driver offers.
    pub sizes: Vec<u32>,
    pub size: u32,
    /// Safe mode: an extra buffer of latency against dropouts.
    pub safe_mode: bool,
}

/// A software mixer beside the device: the virtual devices this app sets up
/// on macOS and Linux. It takes the properties of the Windows driver's mixer
/// ([`sls`]), so everything above works the same on every system.
#[cfg(not(windows))]
pub trait SoftMixer: Send {
    fn set(&mut self, property: &[u8]) -> Result<()>;
    /// Fills in a meter query.
    fn get(&mut self, property: &mut [u8]) -> Result<()>;
}

/// The software mixer of this system, when its virtual devices are set up.
#[cfg(not(windows))]
pub fn soft_mixer() -> Option<Box<dyn SoftMixer>> {
    #[cfg(target_os = "macos")]
    {
        hal_mixer::find().map(|mixer| Box::new(mixer) as Box<dyn SoftMixer>)
    }
    #[cfg(target_os = "linux")]
    {
        pipewire::find().map(|mixer| Box::new(mixer) as Box<dyn SoftMixer>)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Raw access to the control interface: class requests to interface 0.
pub trait Link {
    fn get(&self, request: u8, value: u16, buf: &mut [u8]) -> Result<usize>;
    fn set(&self, request: u8, value: u16, data: &[u8]) -> Result<()>;
}

/// An opened MiniFuse.
pub trait Device: Send {
    fn info(&self) -> &DeviceInfo;

    /// Whether requests can be sent at any time. On Linux every session
    /// briefly takes the interface away from the kernel audio driver, so
    /// the device is only touched when something has to change.
    fn is_live(&self) -> bool;

    /// Whether the device is still plugged in, checked without a session.
    fn is_attached(&mut self) -> bool {
        true
    }

    /// Runs `f` with exclusive access to the control interface.
    fn session(&mut self, f: &mut dyn FnMut(&dyn Link) -> Result<()>) -> Result<()>;

    fn sample_rate(&mut self) -> Option<SampleRate>;

    fn set_sample_rate(&mut self, rate: u32) -> Result<()>;

    /// Whether the driver's software mixer ([`sls`]) is there (Windows).
    fn has_mixer(&self) -> bool {
        false
    }

    /// Sets a property of the software mixer.
    fn mixer_set(&mut self, _property: &[u8]) -> Result<()> {
        Err(Error::new("no software mixer"))
    }

    /// Reads a property of the software mixer into `property`.
    fn mixer_get(&mut self, _property: &mut [u8]) -> Result<()> {
        Err(Error::new("no software mixer"))
    }

    fn asio(&mut self) -> Option<Asio> {
        None
    }

    fn set_asio(&mut self, _size: u32, _safe_mode: bool) -> Result<()> {
        Err(Error::new("ASIO is Windows only"))
    }

    /// Whether a device that is not live still tells the level of its
    /// input, from the audio rather than the device (Linux).
    fn has_input_level(&self) -> bool {
        false
    }

    /// The peak of Input 1 (0..=1) since the last call, see
    /// [`Self::has_input_level`]; `None` while it is not known.
    fn input_level(&mut self) -> Option<f32> {
        None
    }
}

pub enum Found {
    Nothing,
    /// A MiniFuse enumerated as a mass-storage device.
    #[cfg_attr(windows, allow(dead_code))]
    Storage,
    Audio(Box<dyn Device>),
    /// Something on this system prevents access, e.g. a missing driver.
    #[cfg_attr(not(windows), allow(dead_code))]
    Unavailable(String),
}

/// Device discovery for one platform.
pub trait Bus: Send {
    fn scan(&mut self) -> Result<Found>;

    fn leave_storage_mode(&mut self) -> Result<()>;
}

/// The bus for real hardware on this platform.
pub fn system_bus() -> Box<dyn Bus> {
    #[cfg(windows)]
    {
        Box::new(thesycon::ThesyconBus::new())
    }
    #[cfg(not(windows))]
    {
        Box::new(libusb::LibusbBus::new())
    }
}

/// What to tell when the device thread does not report at all: on macOS it
/// waits for the audio server, elsewhere for the driver or USB.
pub fn unresponsive() -> String {
    #[cfg(target_os = "macos")]
    {
        crate::virtual_devices::unresponsive()
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::tr!("MiniFuse 1 is not responding. Reconnect it if this lasts.")
    }
}

/// Pause between consecutive writes, as the official app does.
const WRITE_GAP: Duration = Duration::from_millis(5);

fn get_exact(link: &dyn Link, request: u8, value: u16, buf: &mut [u8]) -> Result<()> {
    let read = link.get(request, value, buf)?;
    if read != buf.len() {
        return Err(Error::new(format!(
            "the device sent {read} bytes instead of {}",
            buf.len()
        )));
    }
    Ok(())
}

pub fn read_state(link: &dyn Link) -> Result<RawState> {
    let mut bytes = [0u8; protocol::STATE_BYTES];
    get_exact(link, protocol::REQ_GET_STATE, 0, &mut bytes)?;
    Ok(RawState::from_bytes(&bytes))
}

pub fn read_param(link: &dyn Link, param: Param, channel: u8) -> Result<u16> {
    let mut bytes = [0u8; 2];
    get_exact(
        link,
        protocol::REQ_GET_PARAM,
        protocol::w_value(param, channel),
        &mut bytes,
    )?;
    Ok(u16::from_le_bytes(bytes))
}

pub fn write_params(link: &dyn Link, writes: &[Write]) -> Result<()> {
    for (i, &(param, channel, value)) in writes.iter().enumerate() {
        if i > 0 {
            thread::sleep(WRITE_GAP);
        }
        link.set(
            protocol::REQ_SET_PARAM,
            protocol::w_value(param, channel),
            &value.to_le_bytes(),
        )?;
    }
    Ok(())
}

/// Peak amplitudes (0..=1) of the meter slots, see [`protocol::meter`].
pub fn read_meters(link: &dyn Link) -> Result<[f32; protocol::METER_SLOTS]> {
    let mut bytes = [0u8; protocol::METER_BYTES];
    get_exact(link, protocol::REQ_GET_METERS, 0, &mut bytes)?;
    let mut meters = [0.0; protocol::METER_SLOTS];
    for (meter, word) in meters.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *meter =
            protocol::meter_to_amplitude(i32::from_le_bytes([word[0], word[1], word[2], word[3]]));
    }
    Ok(meters)
}
