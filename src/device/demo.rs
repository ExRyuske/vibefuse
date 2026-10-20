//! Simulated MiniFuse 1 (`--demo`). It emulates the firmware at the level of
//! control requests, and on request the Windows driver with its software
//! mixer and ASIO settings, so everything above the transport runs
//! unchanged.

use std::cell::RefCell;
use std::collections::HashMap;
use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2};
use std::time::Instant;

use super::protocol::{self, Mix, Param, RawState, StereoStrip, meter};
use super::sls::Channel;
use super::{Asio, Bus, Device, DeviceInfo, Error, Found, Link, Result, SampleRate};

pub struct DemoBus {
    taken: bool,
    windows: bool,
}

impl DemoBus {
    /// `windows`: simulate the Windows driver (software mixer, ASIO).
    pub fn new(windows: bool) -> Self {
        Self {
            taken: false,
            windows,
        }
    }
}

impl Bus for DemoBus {
    fn scan(&mut self) -> Result<Found> {
        if self.taken {
            return Ok(Found::Nothing);
        }
        self.taken = true;
        Ok(Found::Audio(Box::new(DemoDevice::new(self.windows))))
    }

    fn leave_storage_mode(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Levels a real unit starts with (USB 3-4 at 0 dB, the rest at -60 dB),
/// with loopback switched on so the demo shows the mixer.
fn initial_state() -> RawState {
    let mut state = RawState([0; protocol::STATE_WORDS]);
    let mix = Mix {
        enabled: true,
        usb34: StereoStrip {
            level_db: 0.0,
            muted: false,
        },
        usb12: StereoStrip {
            level_db: protocol::MIX_LEVEL_MIN,
            muted: false,
        },
        ..Mix::default()
    };
    mix.all_writes()
        .into_iter()
        .for_each(|write| state.apply(write));
    state
}

pub struct DemoDevice {
    info: DeviceInfo,
    firmware: RefCell<RawState>,
    started: Instant,
    rate: u32,
    mixer: Option<DemoMixer>,
    asio: Asio,
}

impl DemoDevice {
    fn new(windows: bool) -> Self {
        Self {
            info: DeviceInfo {
                product: "MiniFuse 1".to_owned(),
                serial: "DEMO".to_owned(),
                firmware: "1.5.0".to_owned(),
            },
            firmware: RefCell::new(if windows {
                RawState([0; protocol::STATE_WORDS])
            } else {
                initial_state()
            }),
            started: Instant::now(),
            rate: 48_000,
            mixer: windows.then(DemoMixer::default),
            asio: Asio {
                sizes: (4..=11).map(|shift| 1 << shift).collect(),
                size: 128,
                safe_mode: true,
            },
        }
    }

    fn voice(&self) -> f32 {
        let t = self.started.elapsed().as_secs_f32();
        0.35 * (0.5 + 0.5 * (t * 2.1).sin()) * (0.6 + 0.4 * (t * 9.0).sin().abs())
    }

    /// Test signal on a software mixer input.
    fn signal(&self, input: Channel) -> f32 {
        let t = self.started.elapsed().as_secs_f32();
        let wobble = |speed: f32, base: f32| base + 0.1 * (t * speed).sin();
        match (input.kind, input.index) {
            (1, 0) => self.voice(),
            (0, _) => wobble(3.7, 0.25),
            (1, 2 | 3) => wobble(1.3, 0.15),
            (2, 0 | 1) => wobble(2.9, 0.2),
            (2, _) => wobble(0.7, 0.1),
            _ => 0.0,
        }
    }

    fn meters(&self) -> [f32; protocol::METER_SLOTS] {
        let t = self.started.elapsed().as_secs_f32();
        let voice = self.voice();
        let music = 0.25 + 0.1 * (t * 3.7).sin();

        let mut meters = [0.0; protocol::METER_SLOTS];
        meters[meter::INPUT_1] = voice;
        let mix = self.firmware.borrow().mix();
        if mix.enabled {
            let gain = |db: f32| 10f32.powf(db / 20.0);
            let input = if mix.input1.muted {
                0.0
            } else {
                voice * gain(mix.input1.level_db)
            };
            let usb = |strip: StereoStrip| {
                if strip.muted {
                    0.0
                } else {
                    music * gain(strip.level_db) * FRAC_1_SQRT_2
                }
            };
            let bed = usb(mix.usb12) + 0.5 * usb(mix.usb34);
            let angle = mix.input1.pan * FRAC_PI_2;
            let out = gain(mix.output_db);
            meters[meter::LOOPBACK_LEFT] = ((input * angle.cos() + bed) * out).min(1.0);
            meters[meter::LOOPBACK_RIGHT] = ((input * angle.sin() + bed) * out).min(1.0);
        }
        meters
    }
}

fn param(selector: u8) -> Option<Param> {
    Some(match selector {
        0 => Param::InputInst,
        4 => Param::Input48V,
        5 => Param::DirectMonitor,
        6 => Param::MixLevelLeft,
        12 => Param::MixLevelRight,
        18 => Param::MixMute,
        19 => Param::Loopback,
        20 => Param::MixToUsb12,
        21 => Param::MixToOutputs,
        22 => Param::MixOutputLevel,
        _ => return None,
    })
}

impl Link for DemoDevice {
    fn get(&self, request: u8, _value: u16, buf: &mut [u8]) -> Result<usize> {
        let bytes: Vec<u8> = match request {
            protocol::REQ_GET_STATE => self
                .firmware
                .borrow()
                .0
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect(),
            protocol::REQ_GET_METERS => self
                .meters()
                .iter()
                .flat_map(|&amplitude| (((amplitude * 8_388_607.0) as i32) << 8).to_le_bytes())
                .collect(),
            _ => return Err(Error::new("unsupported request")),
        };
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn set(&self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        let [selector, channel] = value.to_be_bytes();
        match (request, param(selector), data) {
            (protocol::REQ_SET_PARAM, Some(param), &[lo, hi]) => {
                self.firmware
                    .borrow_mut()
                    .apply((param, channel, u16::from_le_bytes([lo, hi])));
                Ok(())
            }
            _ => Err(Error::new("unsupported request")),
        }
    }
}

impl Device for DemoDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn is_live(&self) -> bool {
        true
    }

    fn session(&mut self, f: &mut dyn FnMut(&dyn Link) -> Result<()>) -> Result<()> {
        f(self)
    }

    fn sample_rate(&mut self) -> Option<SampleRate> {
        Some(SampleRate {
            current: Some(self.rate),
            available: vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000],
            settable: true,
        })
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<()> {
        self.rate = rate;
        Ok(())
    }

    fn has_mixer(&self) -> bool {
        self.mixer.is_some()
    }

    fn mixer_set(&mut self, property: &[u8]) -> Result<()> {
        let mixer = self.mixer.as_mut().ok_or_else(|| Error::new("no mixer"))?;
        mixer.set(property)
    }

    fn mixer_get(&mut self, property: &mut [u8]) -> Result<()> {
        let mixer = self.mixer.as_ref().ok_or_else(|| Error::new("no mixer"))?;
        let (id, entries) = parse_header(property)?;
        match id {
            42 if property.len() >= 16 => property[8..16].copy_from_slice(&2u64.to_le_bytes()),
            301 if mixer.metering => {
                for i in 0..entries / 8 {
                    let entry = &property[8 + i * 8..16 + i * 8];
                    let channel = Channel {
                        kind: entry[4],
                        index: entry[5],
                    };
                    let amplitude = if entry[0] == 0 {
                        self.signal(channel)
                    } else {
                        mixer
                            .crosspoints
                            .iter()
                            .filter(|((_, to), _)| *to == channel)
                            .map(|((from, _), &gain)| self.signal(*from) * fixed(gain))
                            .sum::<f32>()
                            * fixed(*mixer.outputs.get(&channel).unwrap_or(&(1 << 24)))
                    };
                    let centi_db = (2000.0 * amplitude.max(1e-6).log10()).max(-12000.0) as i16;
                    property[14 + i * 8..16 + i * 8].copy_from_slice(&centi_db.to_le_bytes());
                }
            }
            _ => return Err(Error::new("unsupported property")),
        }
        Ok(())
    }

    fn asio(&mut self) -> Option<Asio> {
        self.mixer.is_some().then(|| self.asio.clone())
    }

    fn set_asio(&mut self, size: u32, safe_mode: bool) -> Result<()> {
        self.asio.size = size;
        self.asio.safe_mode = safe_mode;
        Ok(())
    }
}

/// The DSP plugin of the Windows driver, parsing properties like
/// `arturiaminifuseusbaudio_mixer.sys` does.
#[derive(Default)]
struct DemoMixer {
    crosspoints: HashMap<(Channel, Channel), i32>,
    outputs: HashMap<Channel, i32>,
    metering: bool,
}

const MAX_GAIN: i32 = 1 << 26;

fn fixed(gain: i32) -> f32 {
    gain as f32 / (1 << 24) as f32
}

fn parse_header(property: &[u8]) -> Result<(u32, usize)> {
    let word = |i: usize| u32::from_le_bytes(property[i..i + 4].try_into().expect("four bytes"));
    if property.len() < 8 || word(0) != 0x4D {
        return Err(Error::new("malformed property"));
    }
    Ok((word(4), property.len() - 8))
}

impl DemoMixer {
    fn set(&mut self, property: &[u8]) -> Result<()> {
        let (id, entries) = parse_header(property)?;
        let body = &property[8..];
        let gain = |bytes: &[u8]| {
            i32::from_le_bytes(bytes.try_into().expect("four bytes")).clamp(0, MAX_GAIN)
        };
        let channel = |kind: u8, index: u8| Channel { kind, index };
        match id {
            200 => {
                for entry in body.as_chunks::<8>().0 {
                    self.crosspoints.insert(
                        (channel(entry[0], entry[1]), channel(entry[2], entry[3])),
                        gain(&entry[4..8]),
                    );
                }
            }
            201 => {
                for entry in body.as_chunks::<6>().0 {
                    self.outputs
                        .insert(channel(entry[0], entry[1]), gain(&entry[2..6]));
                }
            }
            300 if entries >= 4 => self.metering = body[..4] != [0; 4],
            _ => return Err(Error::new("unsupported property")),
        }
        Ok(())
    }
}
