//! libusb transport for macOS, which lets user space send class requests to
//! an interface the system audio driver owns, so the device stays live.

use std::time::Duration;

use rusb::UsbContext;

use super::{Bus, Device, DeviceInfo, Error, Found, Link, Result, SampleRate, SoftMixer, protocol};

const TIMEOUT: Duration = Duration::from_millis(1000);

fn usb_error(error: rusb::Error) -> Error {
    match error {
        rusb::Error::NoDevice | rusb::Error::NotFound | rusb::Error::Io => {
            Error::gone(format!("USB: {error}"))
        }
        rusb::Error::Access => Error::new(crate::tr!("No access to the USB device")),
        _ => Error::new(format!("USB: {error}")),
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
        })
    }
}

impl Device for LibusbDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn is_live(&self) -> bool {
        true
    }

    fn is_attached(&mut self) -> bool {
        let ours = self.handle.device();
        self.handle.context().devices().is_ok_and(|devices| {
            devices
                .iter()
                .any(|d| d.bus_number() == ours.bus_number() && d.address() == ours.address())
        })
    }

    fn session(&mut self, f: &mut dyn FnMut(&dyn Link) -> Result<()>) -> Result<()> {
        f(&self.handle)
    }

    fn has_mixer(&self) -> bool {
        self.mixer.is_some()
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
        super::coreaudio::sample_rate(&self.info.serial)
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<()> {
        super::coreaudio::set_sample_rate(&self.info.serial, rate).map_err(Error::new)
    }
}
