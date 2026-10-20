//! Wire protocol of the MiniFuse 1 control interface.
//!
//! Reverse-engineered from Arturia MiniFuse Control Center 1.4.2 (macOS).
//! Every request is a class request on the default control pipe addressed to
//! interface 0 (Audio Control), entity 0, so `wIndex` is always zero:
//!
//! | operation    | bmRequestType | bRequest | wValue            | wLength |
//! |--------------|---------------|----------|-------------------|---------|
//! | set param    | `0x21`        | `0x22`   | `param << 8 \| ch` | 2       |
//! | get param    | `0xA1`        | `0xA2`   | `param << 8 \| ch` | 2       |
//! | read state   | `0xA1`        | `0xB2`   | 0                 | 46      |
//! | read meters  | `0xA1`        | `0xC2`   | 0                 | 16      |
//! | leave MSD    | `0x21`        | `0x03`   | `0x0D00`          | 1       |
//!
//! The state is 23 little-endian `u16` words indexed by [`Param`]. Bitfield
//! parameters live at bit `ch % 16` of word `param + ch / 16`; every other
//! parameter occupies word `param + ch`.

use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, SQRT_2};

use serde::{Deserialize, Serialize};

pub const VENDOR_ID: u16 = 0x1C75;
pub const PRODUCT_ID: u16 = 0xAF80;
/// The same unit enumerated as a mass-storage device.
#[cfg_attr(windows, allow(dead_code))]
pub const PRODUCT_ID_MSD: u16 = 0xAF82;

#[cfg_attr(windows, allow(dead_code))]
pub const REQUEST_TYPE_OUT: u8 = 0x21;
#[cfg_attr(windows, allow(dead_code))]
pub const REQUEST_TYPE_IN: u8 = 0xA1;

pub const REQ_SET_PARAM: u8 = 0x22;
pub const REQ_GET_PARAM: u8 = 0xA2;
pub const REQ_GET_STATE: u8 = 0xB2;
pub const REQ_GET_METERS: u8 = 0xC2;
#[cfg_attr(windows, allow(dead_code))]
pub const REQ_SET_MODE: u8 = 0x03;

/// `wValue` and payload that switch a unit from mass-storage to audio mode.
#[cfg_attr(windows, allow(dead_code))]
pub const MODE_SELECTOR: u16 = 0x0D00;
#[cfg_attr(windows, allow(dead_code))]
pub const MODE_AUDIO: u8 = 0x02;

pub const STATE_WORDS: usize = 23;
pub const STATE_BYTES: usize = STATE_WORDS * 2;
pub const METER_SLOTS: usize = 4;
pub const METER_BYTES: usize = METER_SLOTS * 4;

/// Parameters of the firmware state (`Mf1Fw::mf1_state_param_e`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Param {
    /// Bitfield, bit = input: high-impedance instrument input.
    InputInst = 0,
    /// Detected input type (line/mic). Not shown by the official app.
    #[allow(dead_code)]
    InputType = 1,
    /// Unused: meters are read with [`REQ_GET_METERS`] instead.
    #[allow(dead_code)]
    InputVu = 2,
    /// Bitfield: +48 V phantom power.
    Input48V = 4,
    /// Bitfield: direct monitoring (`OUTPUT_DIRECTMONO`).
    DirectMonitor = 5,
    /// Level of mixer channel `ch` into the left loopback bus (6..=11).
    MixLevelLeft = 6,
    /// Level of mixer channel `ch` into the right loopback bus (12..=17).
    MixLevelRight = 12,
    /// Bitfield, bit = mixer channel: channel muted in the loopback mix.
    MixMute = 18,
    /// Bitfield: loopback feature enabled.
    Loopback = 19,
    /// Bitfield: duplicate the loopback mix on USB inputs 1/2.
    MixToUsb12 = 20,
    /// Bitfield: play the loopback mix on the hardware outputs.
    MixToOutputs = 21,
    /// Loopback mix master level.
    MixOutputLevel = 22,
}

/// Loopback mixer channels.
pub mod channel {
    pub const INPUT_1: u8 = 0;
    pub const USB12_LEFT: u8 = 2;
    pub const USB12_RIGHT: u8 = 3;
    pub const USB34_LEFT: u8 = 4;
    pub const USB34_RIGHT: u8 = 5;
}

/// Meter slots returned by [`REQ_GET_METERS`].
pub mod meter {
    pub const INPUT_1: usize = 0;
    pub const LOOPBACK_LEFT: usize = 2;
    pub const LOOPBACK_RIGHT: usize = 3;
}

impl Param {
    /// Bitfield parameters hold one bit per channel; the rest one word.
    pub fn is_bitfield(self) -> bool {
        !matches!(
            self,
            Param::InputType
                | Param::InputVu
                | Param::MixLevelLeft
                | Param::MixLevelRight
                | Param::MixOutputLevel
        )
    }
}

pub fn w_value(param: Param, channel: u8) -> u16 {
    (param as u16) << 8 | channel as u16
}

/// Level limits of the mixer channels, dB.
pub const MIX_LEVEL_MIN: f32 = -60.0;
pub const MIX_LEVEL_MAX: f32 = 12.0;
/// Level limits of the loopback master, dB.
pub const OUTPUT_LEVEL_MIN: f32 = -64.0;
pub const OUTPUT_LEVEL_MAX: f32 = 0.0;

/// Raw level of -inf. Levels are signed 1/256 dB, like UAC volume controls.
pub const LEVEL_OFF: i16 = i16::MIN;
/// Raw level the official app sends instead of -inf for an audible channel.
pub const LEVEL_FLOOR: i16 = -60 * 256;

/// `log2(10) / 20`: multiplying dB by this and taking `exp2` yields gain.
const DB_TO_LOG2: f32 = 0.166_096_4;
/// Device levels are clamped to this range before conversion back to dB.
const RAW_DB_LOW: f32 = MIX_LEVEL_MIN * 4.0;

fn db_to_gain(db: f32) -> f32 {
    (db * DB_TO_LOG2).exp2()
}

fn gain_to_raw(gain: f32) -> i16 {
    let raw = f64::from(20.0 * gain.log10()) * 256.0;
    raw.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

fn raw_to_gain(raw: i16, max_db: f32) -> f32 {
    let db = (f64::from(raw) / 256.0).max(f64::from(RAW_DB_LOW));
    db_to_gain(db.min(f64::from(max_db)) as f32)
}

/// Rounds a gain to the dB value shown by the official app.
fn gain_to_display_db(gain: f32) -> f32 {
    let db = 20.0 * gain.log10();
    let db = if db.abs() < 0.01 {
        (gain.log10() * 200.0).round() * 0.1
    } else {
        db
    };
    if f64::from(db) < -59.99 {
        MIX_LEVEL_MIN
    } else {
        db
    }
}

/// Mono channel (input 1): level and pan to raw left/right bus levels.
///
/// Pan is 0 (left) ..= 1 (right) with a constant-power law.
pub fn mono_to_raw(level_db: f32, pan: f32) -> (i16, i16) {
    let gain = if level_db < MIX_LEVEL_MIN {
        0.0
    } else {
        db_to_gain(level_db)
    };
    let angle = pan * FRAC_PI_2;
    let left = if pan == 1.0 { 0.0 } else { gain * angle.cos() };
    let right = if pan == 0.0 { 0.0 } else { gain * angle.sin() };

    let mut left = gain_to_raw(left);
    let mut right = gain_to_raw(right);
    if left == LEVEL_OFF && pan != 1.0 {
        left = LEVEL_FLOOR;
    }
    if right == LEVEL_OFF && pan != 0.0 {
        right = LEVEL_FLOOR;
    }
    (left, right)
}

/// Inverse of [`mono_to_raw`]: returns `(level_db, pan)`.
pub fn raw_to_mono(left: i16, right: i16) -> (f32, f32) {
    let (level, pan) = if left == LEVEL_OFF {
        (raw_to_gain(right, MIX_LEVEL_MAX), 1.0)
    } else {
        let left_gain = raw_to_gain(left, MIX_LEVEL_MAX);
        if right == LEVEL_OFF {
            (left_gain, 0.0)
        } else {
            let right_gain = raw_to_gain(right, MIX_LEVEL_MAX);
            let pan = (right_gain / left_gain).atan() * (2.0 / std::f32::consts::PI);
            (left_gain / (pan * FRAC_PI_2).cos(), pan)
        }
    };
    (gain_to_display_db(level), pan)
}

/// Stereo channel (USB 1-2, USB 3-4): the same raw level goes to the left
/// channel's left bus and the right channel's right bus, 3 dB below the
/// fader so a centred mono source and a stereo pair match.
pub fn stereo_to_raw(level_db: f32) -> i16 {
    let gain = if level_db < MIX_LEVEL_MIN {
        0.0
    } else {
        db_to_gain(level_db) * FRAC_1_SQRT_2
    };
    match gain_to_raw(gain) {
        LEVEL_OFF => LEVEL_FLOOR,
        raw => raw,
    }
}

pub fn raw_to_stereo(raw: i16) -> f32 {
    gain_to_display_db(raw_to_gain(raw, MIX_LEVEL_MAX) * SQRT_2)
}

pub fn output_to_raw(level_db: f32) -> i16 {
    let db = f64::from(level_db.max(OUTPUT_LEVEL_MIN * 4.0)) * 256.0;
    db.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

pub fn raw_to_output(raw: i16) -> f32 {
    (f32::from(raw) / 256.0).clamp(OUTPUT_LEVEL_MIN, OUTPUT_LEVEL_MAX)
}

/// Meter word to linear peak amplitude (0..=1): a 24-bit sample in the
/// upper bits of an `i32`.
pub fn meter_to_amplitude(raw: i32) -> f32 {
    ((raw >> 8) as f32 * (1.0 / 8_388_608.0)).abs()
}

/// `bcdDevice` formatted the way the official app shows the firmware version.
pub fn firmware_version(bcd: u16) -> String {
    format!("{}.{}.{}", bcd >> 8, (bcd >> 4) & 0xF, bcd & 0xF)
}

// Typed view of the firmware state and the writes that change it.

/// One parameter write: `(param, channel, value)`.
pub type Write = (Param, u8, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawState(pub [u16; STATE_WORDS]);

impl RawState {
    pub fn from_bytes(bytes: &[u8; STATE_BYTES]) -> Self {
        let mut words = [0; STATE_WORDS];
        for (word, pair) in words.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *word = u16::from_le_bytes([pair[0], pair[1]]);
        }
        Self(words)
    }

    fn bit(&self, param: Param, channel: u8) -> bool {
        let word = self.0[param as usize + usize::from(channel / 16)];
        word >> (channel % 16) & 1 == 1
    }

    fn level(&self, param: Param, channel: u8) -> i16 {
        self.0[param as usize + usize::from(channel)] as i16
    }

    /// Applies a write the way the firmware stores it.
    pub fn apply(&mut self, (param, channel, value): Write) {
        if param.is_bitfield() {
            let word = &mut self.0[param as usize + usize::from(channel / 16)];
            let bit = 1 << (channel % 16);
            if value != 0 {
                *word |= bit;
            } else {
                *word &= !bit;
            }
        } else {
            self.0[param as usize + usize::from(channel)] = value;
        }
    }

    pub fn inputs(&self) -> Inputs {
        Inputs {
            inst: self.bit(Param::InputInst, 0),
            phantom: self.bit(Param::Input48V, 0),
            direct_monitor: self.bit(Param::DirectMonitor, 0),
        }
    }

    pub fn mix(&self) -> Mix {
        let (level_db, pan) = raw_to_mono(
            self.level(Param::MixLevelLeft, channel::INPUT_1),
            self.level(Param::MixLevelRight, channel::INPUT_1),
        );
        let stereo = |left: u8| StereoStrip {
            level_db: raw_to_stereo(self.level(Param::MixLevelLeft, left)),
            muted: self.bit(Param::MixMute, left),
        };
        Mix {
            enabled: self.bit(Param::Loopback, 0),
            input1: MonoStrip {
                level_db,
                pan,
                muted: self.bit(Param::MixMute, channel::INPUT_1),
            },
            usb12: stereo(channel::USB12_LEFT),
            usb34: stereo(channel::USB34_LEFT),
            output_db: raw_to_output(self.level(Param::MixOutputLevel, 0)),
            to_usb12: self.bit(Param::MixToUsb12, 0),
            to_outputs: self.bit(Param::MixToOutputs, 0),
        }
    }
}

/// Front-panel functions, also switchable from the device itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Inputs {
    pub inst: bool,
    pub phantom: bool,
    pub direct_monitor: bool,
}

impl Inputs {
    pub fn writes(&self) -> Vec<Write> {
        vec![
            (Param::InputInst, 0, flag(self.inst)),
            (Param::Input48V, 0, flag(self.phantom)),
            (Param::DirectMonitor, 0, flag(self.direct_monitor)),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MonoStrip {
    pub level_db: f32,
    pub pan: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StereoStrip {
    pub level_db: f32,
    pub muted: bool,
}

/// The loopback mixer: everything the official app remembers per device.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Mix {
    pub enabled: bool,
    pub input1: MonoStrip,
    pub usb12: StereoStrip,
    pub usb34: StereoStrip,
    pub output_db: f32,
    pub to_usb12: bool,
    pub to_outputs: bool,
}

impl Default for Mix {
    /// Defaults of MiniFuse Control Center.
    fn default() -> Self {
        Self {
            enabled: false,
            input1: MonoStrip {
                level_db: MIX_LEVEL_MIN,
                pan: 0.5,
                muted: false,
            },
            usb12: StereoStrip {
                level_db: 0.0,
                muted: false,
            },
            usb34: StereoStrip {
                level_db: 0.0,
                muted: false,
            },
            output_db: 0.0,
            to_usb12: false,
            to_outputs: false,
        }
    }
}

/// A single user-facing control of the loopback mixer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixControl {
    Enabled(bool),
    Input1Level(f32),
    Input1Pan(f32),
    Input1Mute(bool),
    Usb12Level(f32),
    Usb12Mute(bool),
    Usb34Level(f32),
    Usb34Mute(bool),
    OutputLevel(f32),
    ToUsb12(bool),
    ToOutputs(bool),
}

fn flag(value: bool) -> u16 {
    u16::from(value)
}

fn level(raw: i16) -> u16 {
    raw as u16
}

impl Mix {
    /// Applies a control change and returns the writes the device needs.
    pub fn apply(&mut self, control: MixControl) -> Vec<Write> {
        match control {
            MixControl::Enabled(on) => self.enabled = on,
            MixControl::Input1Level(db) => self.input1.level_db = db,
            MixControl::Input1Pan(pan) => self.input1.pan = pan,
            MixControl::Input1Mute(on) => self.input1.muted = on,
            MixControl::Usb12Level(db) => self.usb12.level_db = db,
            MixControl::Usb12Mute(on) => self.usb12.muted = on,
            MixControl::Usb34Level(db) => self.usb34.level_db = db,
            MixControl::Usb34Mute(on) => self.usb34.muted = on,
            MixControl::OutputLevel(db) => self.output_db = db,
            MixControl::ToUsb12(on) => self.to_usb12 = on,
            MixControl::ToOutputs(on) => self.to_outputs = on,
        }
        self.writes_for(control)
    }

    fn writes_for(&self, control: MixControl) -> Vec<Write> {
        match control {
            MixControl::Enabled(_) => vec![(Param::Loopback, 0, flag(self.enabled))],
            MixControl::Input1Level(_) | MixControl::Input1Pan(_) => {
                let (left, right) = mono_to_raw(self.input1.level_db, self.input1.pan);
                vec![
                    (Param::MixLevelLeft, channel::INPUT_1, level(left)),
                    (Param::MixLevelRight, channel::INPUT_1, level(right)),
                ]
            }
            MixControl::Input1Mute(_) => {
                vec![(Param::MixMute, channel::INPUT_1, flag(self.input1.muted))]
            }
            MixControl::Usb12Level(_) => stereo_level(self.usb12, USB12),
            MixControl::Usb12Mute(_) => stereo_mute(self.usb12, USB12),
            MixControl::Usb34Level(_) => stereo_level(self.usb34, USB34),
            MixControl::Usb34Mute(_) => stereo_mute(self.usb34, USB34),
            MixControl::OutputLevel(_) => vec![(
                Param::MixOutputLevel,
                0,
                level(output_to_raw(self.output_db)),
            )],
            MixControl::ToUsb12(_) => vec![(Param::MixToUsb12, 0, flag(self.to_usb12))],
            MixControl::ToOutputs(_) => vec![(Param::MixToOutputs, 0, flag(self.to_outputs))],
        }
    }

    /// Every write needed to put this mix on a device.
    pub fn all_writes(&self) -> Vec<Write> {
        [
            MixControl::Input1Level(self.input1.level_db),
            MixControl::Input1Mute(self.input1.muted),
            MixControl::Usb12Level(self.usb12.level_db),
            MixControl::Usb12Mute(self.usb12.muted),
            MixControl::Usb34Level(self.usb34.level_db),
            MixControl::Usb34Mute(self.usb34.muted),
            MixControl::OutputLevel(self.output_db),
            MixControl::ToUsb12(self.to_usb12),
            MixControl::ToOutputs(self.to_outputs),
            MixControl::Enabled(self.enabled),
        ]
        .into_iter()
        .flat_map(|control| self.writes_for(control))
        .collect()
    }

    /// Whether `other` (read back from the device) matches this mix closely
    /// enough that restoring it would change nothing audible.
    pub fn approx_eq(&self, other: &Mix) -> bool {
        let close = |a: f32, b: f32, eps: f32| (a - b).abs() <= eps;
        self.enabled == other.enabled
            && close(self.input1.level_db, other.input1.level_db, 0.1)
            && close(self.input1.pan, other.input1.pan, 0.01)
            && self.input1.muted == other.input1.muted
            && close(self.usb12.level_db, other.usb12.level_db, 0.1)
            && self.usb12.muted == other.usb12.muted
            && close(self.usb34.level_db, other.usb34.level_db, 0.1)
            && self.usb34.muted == other.usb34.muted
            && close(self.output_db, other.output_db, 0.1)
            && self.to_usb12 == other.to_usb12
            && self.to_outputs == other.to_outputs
    }
}

/// Left and right mixer channels of a stereo pair.
const USB12: (u8, u8) = (channel::USB12_LEFT, channel::USB12_RIGHT);
const USB34: (u8, u8) = (channel::USB34_LEFT, channel::USB34_RIGHT);

/// The left channel feeds only the left bus and the right channel only the
/// right bus; the cross-feeds are left untouched, as the official app does.
fn stereo_level(strip: StereoStrip, (left, right): (u8, u8)) -> Vec<Write> {
    let raw = level(stereo_to_raw(strip.level_db));
    vec![
        (Param::MixLevelLeft, left, raw),
        (Param::MixLevelRight, right, raw),
    ]
}

fn stereo_mute(strip: StereoStrip, (left, right): (u8, u8)) -> Vec<Write> {
    let muted = flag(strip.muted);
    vec![
        (Param::MixMute, left, muted),
        (Param::MixMute, right, muted),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w_value_packs_selector_and_channel() {
        assert_eq!(w_value(Param::Input48V, 0), 0x0400);
        assert_eq!(w_value(Param::MixLevelRight, 3), 0x0C03);
        assert_eq!(w_value(Param::MixOutputLevel, 0), 0x1600);
    }

    #[test]
    fn mono_centre_is_minus_three_db_per_side() {
        let (l, r) = mono_to_raw(0.0, 0.5);
        assert_eq!(l, r);
        assert!((f32::from(l) / 256.0 + 3.01).abs() < 0.02);
    }

    #[test]
    fn mono_hard_pan_turns_other_side_off() {
        assert_eq!(mono_to_raw(0.0, 0.0), (0, LEVEL_OFF));
        assert_eq!(mono_to_raw(0.0, 1.0).0, LEVEL_OFF);
    }

    #[test]
    fn mono_below_minimum_uses_floor() {
        assert_eq!(mono_to_raw(-70.0, 0.5), (LEVEL_FLOOR, LEVEL_FLOOR));
    }

    #[test]
    fn mono_round_trip() {
        for &(db, pan) in &[
            (0.0, 0.5),
            (-12.5, 0.2),
            (6.0, 0.9),
            (-40.0, 0.5),
            (12.0, 0.0),
        ] {
            let (l, r) = mono_to_raw(db, pan);
            let (db2, pan2) = raw_to_mono(l, r);
            assert!((db - db2).abs() < 0.05, "{db} -> {db2}");
            assert!((pan - pan2).abs() < 0.01, "{pan} -> {pan2}");
        }
    }

    #[test]
    fn stereo_round_trip() {
        for &db in &[0.0, -6.0, 12.0, -59.0] {
            let db2 = raw_to_stereo(stereo_to_raw(db));
            assert!((db - db2).abs() < 0.05, "{db} -> {db2}");
        }
        assert_eq!(stereo_to_raw(-80.0), LEVEL_FLOOR);
    }

    #[test]
    fn output_level_is_plain_db() {
        assert_eq!(output_to_raw(0.0), 0);
        assert_eq!(output_to_raw(-64.0), -16384);
        assert_eq!(raw_to_output(-1536), -6.0);
    }

    #[test]
    fn meter_scaling() {
        assert_eq!(meter_to_amplitude(0x7FFF_FF00), 8_388_607.0 / 8_388_608.0);
        assert_eq!(meter_to_amplitude(0x4000_0000), 0.5);
        assert_eq!(meter_to_amplitude(-0x4000_0000), 0.5);
    }

    #[test]
    fn firmware_from_bcd() {
        assert_eq!(firmware_version(0x0150), "1.5.0");
        assert_eq!(firmware_version(0x0105), "1.0.5");
    }

    fn state_with(words: &[(usize, u16)]) -> RawState {
        let mut raw = [0u16; STATE_WORDS];
        for &(i, v) in words {
            raw[i] = v;
        }
        RawState(raw)
    }

    #[test]
    fn bitfields_are_read_per_channel() {
        let state = state_with(&[(0, 1), (4, 1), (18, 0b10_0100), (19, 1)]);
        let inputs = state.inputs();
        assert!(inputs.inst && inputs.phantom && !inputs.direct_monitor);
        let mix = state.mix();
        assert!(mix.enabled);
        assert!(mix.usb12.muted);
        assert!(!mix.input1.muted);
        assert!(!mix.usb34.muted);
    }

    #[test]
    fn state_bytes_are_little_endian() {
        let mut bytes = [0u8; STATE_BYTES];
        bytes[44] = 0x00;
        bytes[45] = 0xFA;
        let state = RawState::from_bytes(&bytes);
        assert_eq!(state.0[22], 0xFA00);
        assert_eq!(state.mix().output_db, -6.0);
    }

    #[test]
    fn stereo_writes_mirror_to_right_channel() {
        let mut mix = Mix::default();
        let writes = mix.apply(MixControl::Usb34Mute(true));
        assert_eq!(writes, vec![(Param::MixMute, 4, 1), (Param::MixMute, 5, 1)]);
        let writes = mix.apply(MixControl::Usb12Level(0.0));
        assert_eq!(writes[0].0, Param::MixLevelLeft);
        assert_eq!(writes[0].1, 2);
        assert_eq!(writes[1].0, Param::MixLevelRight);
        assert_eq!(writes[1].1, 3);
        assert_eq!(writes[0].2, writes[1].2);
    }

    #[test]
    fn written_mix_reads_back_equal() {
        let mix = Mix {
            enabled: true,
            input1: MonoStrip {
                level_db: -6.0,
                pan: 0.3,
                muted: true,
            },
            usb12: StereoStrip {
                level_db: -3.0,
                muted: false,
            },
            usb34: StereoStrip {
                level_db: 4.5,
                muted: true,
            },
            output_db: -12.0,
            to_usb12: true,
            to_outputs: false,
        };
        let mut raw = RawState([0; STATE_WORDS]);
        mix.all_writes()
            .into_iter()
            .for_each(|write| raw.apply(write));
        assert!(mix.approx_eq(&raw.mix()));
    }
}
