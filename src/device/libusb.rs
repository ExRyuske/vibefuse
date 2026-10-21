//! libusb transport for macOS and Linux.
//!
//! macOS lets user space send class requests to an interface the system
//! audio driver owns, so the device stays live. Linux refuses them until
//! the interface is claimed, which detaches `snd-usb-audio`; every session
//! therefore detaches the driver and lets it re-create the sound card
//! afterwards.

use std::time::Duration;

use rusb::UsbContext;

use super::{Bus, Device, DeviceInfo, Error, Found, Link, Result, SampleRate, SoftMixer, protocol};

const TIMEOUT: Duration = Duration::from_millis(1000);

fn usb_error(error: rusb::Error) -> Error {
    match error {
        rusb::Error::NoDevice | rusb::Error::NotFound | rusb::Error::Io => {
            Error::gone(format!("USB: {error}"))
        }
        rusb::Error::Access => Error::new(access_hint()),
        _ => Error::new(format!("USB: {error}")),
    }
}

fn access_hint() -> String {
    if cfg!(target_os = "linux") {
        crate::tr!("No access to the USB device: install the udev rule (see the README)")
    } else {
        crate::tr!("No access to the USB device")
    }
}

impl Link for rusb::DeviceHandle<rusb::Context> {
    fn get(&self, request: u8, value: u16, buf: &mut [u8]) -> Result<usize> {
        self.read_control(protocol::REQUEST_TYPE_IN, request, value, 0, buf, TIMEOUT)
            .map_err(usb_error)
    }

    fn set(&self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        let written = self
            .write_control(protocol::REQUEST_TYPE_OUT, request, value, 0, data, TIMEOUT)
            .map_err(usb_error)?;
        if written != data.len() {
            return Err(Error::new("short write"));
        }
        Ok(())
    }
}

pub struct LibusbBus {
    context: Option<rusb::Context>,
}

impl LibusbBus {
    pub fn new() -> Self {
        Self { context: None }
    }

    fn context(&mut self) -> Result<rusb::Context> {
        if self.context.is_none() {
            let context = rusb::Context::new().map_err(|e| Error::new(format!("libusb: {e}")))?;
            self.context = Some(context);
        }
        Ok(self.context.clone().expect("context was just set"))
    }

    fn devices(&mut self, product: u16) -> Result<Vec<rusb::Device<rusb::Context>>> {
        let devices = self.context()?.devices().map_err(usb_error)?;
        Ok(devices
            .iter()
            .filter(|device| {
                device.device_descriptor().is_ok_and(|d| {
                    d.vendor_id() == protocol::VENDOR_ID && d.product_id() == product
                })
            })
            .collect())
    }
}

impl Bus for LibusbBus {
    fn scan(&mut self) -> Result<Found> {
        if let Some(device) = self.devices(protocol::PRODUCT_ID)?.into_iter().next() {
            return LibusbDevice::open(&device).map(|device| Found::Audio(Box::new(device)));
        }
        if !self.devices(protocol::PRODUCT_ID_MSD)?.is_empty() {
            return Ok(Found::Storage);
        }
        Ok(Found::Nothing)
    }

    fn leave_storage_mode(&mut self) -> Result<()> {
        for device in self.devices(protocol::PRODUCT_ID_MSD)? {
            let handle = device.open().map_err(usb_error)?;
            handle.set(
                protocol::REQ_SET_MODE,
                protocol::MODE_SELECTOR,
                &[protocol::MODE_AUDIO],
            )?;
        }
        Ok(())
    }
}

/// rusb decodes `bcdDevice`; the official app formats the raw value.
fn bcd(version: rusb::Version) -> u16 {
    let major = u16::from(version.major());
    (major / 10) << 12
        | (major % 10) << 8
        | u16::from(version.minor()) << 4
        | u16::from(version.sub_minor())
}

pub struct LibusbDevice {
    handle: rusb::DeviceHandle<rusb::Context>,
    info: DeviceInfo,
    /// This app's virtual devices, when they are set up.
    mixer: Option<Box<dyn SoftMixer>>,
    /// The input's level can be recorded (Linux).
    input_level: bool,
}

impl LibusbDevice {
    fn open(device: &rusb::Device<rusb::Context>) -> Result<Self> {
        let descriptor = device.device_descriptor().map_err(usb_error)?;
        let handle = device.open().map_err(usb_error)?;
        let read = |index: Option<u8>| {
            index
                .and_then(|index| handle.read_string_descriptor_ascii(index).ok())
                .unwrap_or_default()
        };
        let product = Some(read(descriptor.product_string_index()))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "MiniFuse 1".to_owned());
        let serial = read(descriptor.serial_number_string_index());
        let info = DeviceInfo {
            product,
            serial,
            firmware: protocol::firmware_version(bcd(descriptor.device_version())),
        };
        Ok(Self {
            handle,
            info,
            mixer: super::soft_mixer(),
            #[cfg(target_os = "linux")]
            input_level: super::pipewire::capture::available(),
            #[cfg(not(target_os = "linux"))]
            input_level: false,
        })
    }
}

/// Interfaces bound to `snd-usb-audio` (control, playback, capture). The
/// control interface is released last so the re-probing driver finds the
/// streaming interfaces free.
#[cfg(target_os = "linux")]
const CLAIM_ORDER: [u8; 3] = [0, 1, 2];
#[cfg(target_os = "linux")]
const RELEASE_ORDER: [u8; 3] = [1, 2, 0];

impl Device for LibusbDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn is_live(&self) -> bool {
        !cfg!(target_os = "linux")
    }

    fn is_attached(&mut self) -> bool {
        let ours = self.handle.device();
        self.handle.context().devices().is_ok_and(|devices| {
            devices
                .iter()
                .any(|d| d.bus_number() == ours.bus_number() && d.address() == ours.address())
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn session(&mut self, f: &mut dyn FnMut(&dyn Link) -> Result<()>) -> Result<()> {
        f(&self.handle)
    }

    #[cfg(target_os = "linux")]
    fn session(&mut self, f: &mut dyn FnMut(&dyn Link) -> Result<()>) -> Result<()> {
        self.handle
            .set_auto_detach_kernel_driver(true)
            .map_err(usb_error)?;
        let mut claimed = Vec::new();
        let mut result = Ok(());
        for interface in CLAIM_ORDER {
            match self.handle.claim_interface(interface) {
                Ok(()) => claimed.push(interface),
                Err(error) => {
                    result = Err(usb_error(error));
                    break;
                }
            }
        }
        if result.is_ok() {
            result = f(&self.handle);
        }
        for interface in RELEASE_ORDER {
            if claimed.contains(&interface) {
                // Releasing re-attaches the kernel driver; nothing binds to
                // the streaming interfaces until the control one is back.
                let _ = self.handle.release_interface(interface);
            }
        }
        result
    }

    fn has_mixer(&self) -> bool {
        self.mixer.is_some()
    }

    fn has_input_level(&self) -> bool {
        self.input_level
    }

    fn input_level(&mut self) -> Option<f32> {
        #[cfg(target_os = "linux")]
        {
            super::pipewire::capture::take_peak()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn mixer_set(&mut self, property: &[u8]) -> Result<()> {
        match &mut self.mixer {
            Some(mixer) => mixer.set(property),
            None => Err(Error::new("no virtual outputs")),
        }
    }

    fn mixer_get(&mut self, property: &mut [u8]) -> Result<()> {
        match &mut self.mixer {
            Some(mixer) => mixer.get(property),
            None => Err(Error::new("no virtual outputs")),
        }
    }

    fn sample_rate(&mut self) -> Option<SampleRate> {
        #[cfg(target_os = "macos")]
        {
            super::coreaudio::sample_rate(&self.info.serial)
        }
        #[cfg(target_os = "linux")]
        {
            alsa_sample_rate()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            super::coreaudio::set_sample_rate(&self.info.serial, rate).map_err(Error::new)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = rate;
            Err(Error::new(crate::tr!(
                "The sound server sets the sample rate"
            )))
        }
    }
}

/// The sample rate on Linux, read from `/proc/asound`. Whoever opens the
/// stream (PipeWire, JACK, …) chooses it, so it is only shown, never set.
#[cfg(target_os = "linux")]
fn alsa_sample_rate() -> Option<SampleRate> {
    let usb_id = format!("{:04x}:{:04x}", protocol::VENDOR_ID, protocol::PRODUCT_ID);
    std::fs::read_dir("/proc/asound")
        .ok()?
        .flatten()
        .find_map(|card| {
            let path = card.path();
            let id = std::fs::read_to_string(path.join("usbid")).ok()?;
            if id.trim() != usb_id {
                return None;
            }
            let stream = std::fs::read_to_string(path.join("stream0")).ok()?;
            Some(parse_alsa_stream(&stream))
        })
}

#[cfg(target_os = "linux")]
fn parse_alsa_stream(text: &str) -> SampleRate {
    let mut current = None;
    let mut available = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Momentary freq = ") {
            current = current.or_else(|| rest.split_whitespace().next()?.parse().ok());
        } else if let Some(rest) = line.strip_prefix("Rates: ") {
            available.extend(
                rest.split(',')
                    .filter_map(|rate| rate.trim().parse::<u32>().ok()),
            );
        }
    }
    available.sort_unstable();
    available.dedup();
    SampleRate {
        current,
        available,
        settable: false,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn parses_running_stream() {
        let text = "Arturia MiniFuse 1 at usb-0000:00:14.0-1, high speed : USB Audio

Playback:
  Status: Running
    Interface = 1
    Momentary freq = 48000 Hz (0x6.0000)
  Interface 1
    Altset 1
    Rates: 44100, 48000, 88200, 96000, 176400, 192000
";
        let rate = parse_alsa_stream(text);
        assert_eq!(rate.current, Some(48000));
        assert_eq!(rate.available.len(), 6);
        assert!(!rate.settable);
    }
}
