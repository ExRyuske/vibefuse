//! Software mixer of Arturia's Windows driver ("SLS" in MiniFuse Control
//! Center, the OUTPUTS page of its window).
//!
//! The Windows driver ships a DSP plugin (`arturiaminifuseusbaudio_mixer.sys`)
//! that mixes the hardware input, the main playback and the virtual playback
//! devices into three stereo mixes: the hardware outputs ("Monitor &
//! Phones") and two recording devices ("Loopback Mix 3/4", "Loopback Mix
//! 5/6"). It is driven with `TUSBAUDIO_SetDspProperty/GetDspProperty`.
//! Every property starts with `u32 0x4D, u32 id`:
//!
//! | id  | property         | entries after the header                       |
//! |-----|------------------|------------------------------------------------|
//! | 42  | version          | `u64`, answered with 2                         |
//! | 200 | crosspoint gains | `in kind, in, out kind, out: u8; gain: i32`    |
//! | 201 | output gains     | `out kind, out: u8; gain: i32`                 |
//! | 300 | meters on/off    | `u32`, per open handle                         |
//! | 301 | meters           | `dir: u32` (0 in, 1 out), `kind, ch: u8`, `centi-dB: i16` |
//!
//! Gains are linear with `1 << 24` at 0 dB. Mixer inputs are host playback
//! (kind 0), device inputs (1) and virtual playback (2); outputs are host
//! recording (0), device outputs (1) and virtual recording (2).
//!
//! Gains are computed exactly like MiniFuse Control Center 1.4.2 for
//! Windows, so a mix set here sounds the same as one set there, and the
//! mixer's preset entries (`SLS_*`) are read and written in its format.

use std::f64::consts::FRAC_PI_2;

use serde::{Deserialize, Serialize};

pub const MIXES: usize = 3;
pub const SOURCES: usize = 5;
pub const LEVEL_MIN: f32 = -60.0;
pub const LEVEL_MAX: f32 = 0.0;

pub const MIX_NAMES: [&str; MIXES] = ["Monitor & Phones", "Loopback Mix 3/4", "Loopback Mix 5/6"];
pub const SOURCE_NAMES: [&str; SOURCES] = [
    "Input 1",
    "Main 1/2",
    "Virtual 3/4",
    "Virtual 5/6",
    "Virtual 7/8",
];
/// Index of the only mono source.
pub const INPUT_1: usize = 0;

/// Levels at or below this are sent as silence, like the official app.
const SILENCE_DB: f32 = -59.9;
/// Gain of 0 dB.
const UNITY: f64 = 16_777_216.0;
/// Entries per property. The official app reads all 24 meters of the unit in
/// one property, so the driver takes at least that many.
const MAX_ENTRIES: usize = 24;
/// Meter reading of silence (-120 dB).
pub const METER_FLOOR: f32 = -120.0;

const MAGIC: u32 = 0x4D;
#[cfg_attr(not(windows), allow(dead_code))]
const PROPERTY_VERSION: u32 = 42;
const PROPERTY_GAINS: u32 = 200;
const PROPERTY_OUTPUTS: u32 = 201;
const PROPERTY_METERS_ON: u32 = 300;
const PROPERTY_METERS: u32 = 301;
/// Version of the plugin interface this module speaks.
#[cfg_attr(not(windows), allow(dead_code))]
const VERSION: u32 = 2;

/// `Exp(-2)` mapping of the official level parameters.
const ORDER: f32 = -2.0;

mod input {
    pub const PLAYBACK: u8 = 0;
    pub const DEVICE: u8 = 1;
    pub const VIRTUAL: u8 = 2;
}

mod output {
    pub const RECORD: u8 = 0;
    pub const DEVICE: u8 = 1;
    pub const VIRTUAL: u8 = 2;
}

/// A mono mixer input or output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Channel {
    pub kind: u8,
    pub index: u8,
}

const fn ch(kind: u8, index: u8) -> Channel {
    Channel { kind, index }
}

/// Mixer inputs of each source. "Virtual 3/4" is USB playback 3/4, which
/// the device loops back to its USB inputs 3/4 while its own loopback mixer
/// is off.
const SOURCE_CHANNELS: [&[Channel]; SOURCES] = [
    &[ch(input::DEVICE, 0)],
    &[ch(input::PLAYBACK, 0), ch(input::PLAYBACK, 1)],
    &[ch(input::DEVICE, 2), ch(input::DEVICE, 3)],
    &[ch(input::VIRTUAL, 0), ch(input::VIRTUAL, 1)],
    &[ch(input::VIRTUAL, 2), ch(input::VIRTUAL, 3)],
];

/// Left/right outputs of each mix. Monitor & Phones is also mirrored to the
/// virtual recording device "Loopback Main 7/8".
const MIX_OUTPUTS: [&[[Channel; 2]]; MIXES] = [
    &[
        [ch(output::DEVICE, 0), ch(output::DEVICE, 1)],
        [ch(output::VIRTUAL, 2), ch(output::VIRTUAL, 3)],
    ],
    &[[ch(output::RECORD, 2), ch(output::RECORD, 3)]],
    &[[ch(output::VIRTUAL, 0), ch(output::VIRTUAL, 1)]],
];

/// One source in one mix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub level_db: f32,
    /// Pan of the left channel, or of the mono input: 0 left, 1 right.
    pub pan_left: f32,
    /// Pan of the right channel; unused for the mono input.
    pub pan_right: f32,
    /// Both channels of a stereo source share one pan.
    #[serde(default)]
    pub mono: bool,
    pub muted: bool,
    /// Whether the official app lists the channel; only kept for presets.
    pub shown: bool,
}

impl Source {
    fn new(mono: bool, muted: bool) -> Self {
        Self {
            level_db: 0.0,
            pan_left: if mono { 0.5 } else { 0.0 },
            pan_right: 1.0,
            mono: false,
            muted,
            shown: false,
        }
    }

    /// The one pan the window shows: where the middle of a stereo pair is.
    pub fn pan(&self, stereo: bool) -> f32 {
        if stereo {
            (self.pan_left + self.pan_right) / 2.0
        } else {
            self.pan_left
        }
    }

    /// Pans a stereo pair as one: in the middle each channel stays on its
    /// side; towards a side the other channel follows until both are there.
    /// In mono both channels sit at the pan.
    fn set_pan(&mut self, stereo: bool, pan: f32) {
        let pan = pan.clamp(0.0, 1.0);
        if !stereo {
            self.pan_left = pan;
        } else if self.mono {
            self.pan_left = pan;
            self.pan_right = pan;
        } else {
            self.pan_left = (2.0 * pan - 1.0).max(0.0);
            self.pan_right = (2.0 * pan).min(1.0);
        }
    }

    /// Mono puts both channels where the pair's middle was, so the sound
    /// stays put; back in stereo they spread from there again.
    fn set_mono(&mut self, mono: bool) {
        let pan = self.pan(true);
        self.mono = mono;
        self.set_pan(true, pan);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlsMix {
    pub sources: [Source; SOURCES],
    pub output_db: f32,
}

/// The whole software mixer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sls {
    pub mixes: [SlsMix; MIXES],
}

impl Default for Sls {
    /// The official factory preset: main playback on the outputs, Virtual
    /// 3/4 on Loopback Mix 3/4 and Virtual 5/6 on Loopback Mix 5/6.
    fn default() -> Self {
        let mix = |open: Option<usize>| {
            let mut sources = [0, 1, 2, 3, 4].map(|i| Source::new(i == INPUT_1, Some(i) != open));
            if let Some(i) = open.filter(|&i| i >= 2) {
                sources[i].shown = true;
            }
            SlsMix {
                sources,
                output_db: 0.0,
            }
        };
        Self {
            mixes: [mix(Some(1)), mix(Some(2)), mix(Some(3))],
        }
    }
}

/// A control of the software mixer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlsControl {
    Level {
        mix: usize,
        source: usize,
        db: f32,
    },
    /// One pan for the whole source, see [`Source::set_pan`].
    Pan {
        mix: usize,
        source: usize,
        pan: f32,
    },
    /// Sums a stereo source into one channel; the mono input ignores it.
    Mono {
        mix: usize,
        source: usize,
        mono: bool,
    },
    Mute {
        mix: usize,
        source: usize,
        muted: bool,
    },
    Output {
        mix: usize,
        db: f32,
    },
}

/// Gains to send to the driver.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gains {
    pub crosspoints: Vec<(Channel, Channel, i32)>,
    pub outputs: Vec<(Channel, i32)>,
}

impl Gains {
    pub fn is_empty(&self) -> bool {
        self.crosspoints.is_empty() && self.outputs.is_empty()
    }

    pub fn extend(&mut self, other: Gains) {
        for point in other.crosspoints {
            match self
                .crosspoints
                .iter_mut()
                .find(|p| p.0 == point.0 && p.1 == point.1)
            {
                Some(p) => *p = point,
                None => self.crosspoints.push(point),
            }
        }
        for gain in other.outputs {
            match self.outputs.iter_mut().find(|g| g.0 == gain.0) {
                Some(g) => *g = gain,
                None => self.outputs.push(gain),
            }
        }
    }

    /// The properties that set these gains.
    pub fn properties(&self) -> Vec<Vec<u8>> {
        let mut properties: Vec<Vec<u8>> = self
            .crosspoints
            .chunks(MAX_ENTRIES)
            .map(|chunk| {
                let mut p = header(PROPERTY_GAINS);
                for (from, to, gain) in chunk {
                    p.extend([from.kind, from.index, to.kind, to.index]);
                    p.extend(gain.to_le_bytes());
                }
                p
            })
            .collect();
        properties.extend(self.outputs.chunks(MAX_ENTRIES).map(|chunk| {
            let mut p = header(PROPERTY_OUTPUTS);
            for (to, gain) in chunk {
                p.extend([to.kind, to.index]);
                p.extend(gain.to_le_bytes());
            }
            p
        }));
        properties
    }
}

/// Linear gain of a level, silent at the bottom of the fader.
fn level_gain(db: f32) -> f32 {
    if db > SILENCE_DB {
        10f32.powf(db * 0.05)
    } else {
        0.0
    }
}

/// The official app passes every crosspoint gain through its 1/256 dB
/// representation, so silence ends up at -128 dB rather than zero.
fn through_db(gain: f32) -> f32 {
    let db = 20.0 * gain.log10();
    let steps = if db.is_infinite() {
        -32768.0
    } else {
        (f64::from(db) * 256.0).clamp(-32768.0, 32767.0).trunc()
    };
    let db = (steps / 256.0).clamp(-240.0, 0.0) as f32;
    10f32.powf(db * 0.05)
}

/// Constant-power pan: cos/sin of a quarter turn.
fn pan_gains(pan: f32, gain: f32) -> (f32, f32) {
    let angle = (f64::from(pan) * FRAC_PI_2) as f32;
    let left = if pan != 1.0 { angle.cos() * gain } else { 0.0 };
    let right = if pan != 0.0 { angle.sin() * gain } else { 0.0 };
    (through_db(left), through_db(right))
}

fn fixed(gain: f32) -> i32 {
    (f64::from(gain) * UNITY) as i32
}

impl Sls {
    /// Applies a control and returns the gains that change.
    pub fn apply(&mut self, control: SlsControl) -> Gains {
        match control {
            SlsControl::Level { mix, source, db } => {
                self.mixes[mix].sources[source].level_db = db.clamp(LEVEL_MIN, LEVEL_MAX);
                self.source_gains(mix, source)
            }
            SlsControl::Pan { mix, source, pan } => {
                self.mixes[mix].sources[source].set_pan(source != INPUT_1, pan);
                self.source_gains(mix, source)
            }
            SlsControl::Mono { mix, source, mono } => {
                if source != INPUT_1 {
                    self.mixes[mix].sources[source].set_mono(mono);
                }
                self.source_gains(mix, source)
            }
            SlsControl::Mute { mix, source, muted } => {
                self.mixes[mix].sources[source].muted = muted;
                self.source_gains(mix, source)
            }
            SlsControl::Output { mix, db } => {
                self.mixes[mix].output_db = db.clamp(LEVEL_MIN, LEVEL_MAX);
                self.output_gains(mix)
            }
        }
    }

    /// Whether the card's input is heard on its outputs: the "Monitor &
    /// Phones" mix has it on. The macOS bridge must run for that.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn input_monitored(&self) -> bool {
        let mix = &self.mixes[0];
        let input = &mix.sources[INPUT_1];
        !input.muted && input.level_db > LEVEL_MIN && mix.output_db > LEVEL_MIN
    }

    pub fn source_gains(&self, mix: usize, source: usize) -> Gains {
        let s = self.mixes[mix].sources[source];
        let level = level_gain(s.level_db);
        let mut gains = Gains::default();
        for (&from, pan) in SOURCE_CHANNELS[source]
            .iter()
            .zip([s.pan_left, s.pan_right])
        {
            let (left, right) = if s.muted {
                (0.0, 0.0)
            } else {
                pan_gains(pan, level)
            };
            for &[to_left, to_right] in MIX_OUTPUTS[mix] {
                gains.crosspoints.push((from, to_left, fixed(left)));
                gains.crosspoints.push((from, to_right, fixed(right)));
            }
        }
        gains
    }

    pub fn output_gains(&self, mix: usize) -> Gains {
        let gain = fixed(level_gain(self.mixes[mix].output_db));
        Gains {
            crosspoints: Vec::new(),
            outputs: MIX_OUTPUTS[mix]
                .iter()
                .flatten()
                .map(|&to| (to, gain))
                .collect(),
        }
    }

    /// Everything, for a freshly connected device.
    pub fn all_gains(&self) -> Gains {
        let mut gains = Gains::default();
        for mix in 0..MIXES {
            for source in 0..SOURCES {
                gains.extend(self.source_gains(mix, source));
            }
            gains.extend(self.output_gains(mix));
        }
        gains
    }
}

fn header(id: u32) -> Vec<u8> {
    let mut p = MAGIC.to_le_bytes().to_vec();
    p.extend(id.to_le_bytes());
    p
}

/// Asks for the plugin interface version.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn version_query() -> Vec<u8> {
    let mut p = header(PROPERTY_VERSION);
    p.extend([0; 8]);
    p
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn is_supported(reply: &[u8]) -> bool {
    reply.len() >= 12 && reply[8..12] == VERSION.to_le_bytes()
}

/// Turns metering on or off for this handle; the driver only measures
/// while someone looks.
pub fn meters_switch(on: bool) -> Vec<u8> {
    let mut p = header(PROPERTY_METERS_ON);
    p.extend(u32::from(on).to_le_bytes());
    p
}

/// Meters of every source and every mix (left channel first).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meters {
    pub sources: [[f32; 2]; SOURCES],
    pub mixes: [[f32; 2]; MIXES],
}

impl Default for Meters {
    fn default() -> Self {
        Self {
            sources: [[METER_FLOOR; 2]; SOURCES],
            mixes: [[METER_FLOOR; 2]; MIXES],
        }
    }
}

impl Meters {
    pub fn is_silent(&self) -> bool {
        self.sources
            .iter()
            .chain(&self.mixes)
            .flatten()
            .all(|&db| db <= -90.0)
    }
}

fn metered() -> impl Iterator<Item = (u32, Channel)> {
    let inputs = SOURCE_CHANNELS
        .iter()
        .flat_map(|c| c.iter().map(|&c| (0, c)));
    let outputs = MIX_OUTPUTS
        .iter()
        .flat_map(|o| o[0].iter().map(|&c| (1, c)));
    inputs.chain(outputs)
}

/// The meter property; the driver fills in the levels.
pub fn meters_query() -> Vec<u8> {
    let mut p = header(PROPERTY_METERS);
    for (direction, channel) in metered() {
        p.extend(direction.to_le_bytes());
        p.extend([channel.kind, channel.index, 0, 0]);
    }
    p
}

pub fn parse_meters(reply: &[u8]) -> Option<Meters> {
    let levels: Vec<f32> = reply
        .get(8..)?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|entry| f32::from(i16::from_le_bytes([entry[6], entry[7]])) / 100.0)
        .collect();
    if levels.len() != metered().count() {
        return None;
    }
    let mut meters = Meters::default();
    let mut next = levels.into_iter();
    for (source, meter) in meters.sources.iter_mut().enumerate() {
        let left = next.next()?;
        let right = if SOURCE_CHANNELS[source].len() == 2 {
            next.next()?
        } else {
            left
        };
        *meter = [left, right];
    }
    for meter in &mut meters.mixes {
        *meter = [next.next()?, next.next()?];
    }
    Some(meters)
}

/// Official `Exp(-2)` mapping: normalized fader position to dB.
pub fn level_from_normalized(x: f32) -> f32 {
    let y = ((ORDER * x.clamp(0.0, 1.0)).exp() - 1.0) / (ORDER.exp() - 1.0);
    LEVEL_MIN + y * (LEVEL_MAX - LEVEL_MIN)
}

pub fn level_to_normalized(db: f32) -> f32 {
    let y = ((db - LEVEL_MIN) / (LEVEL_MAX - LEVEL_MIN)).clamp(0.0, 1.0);
    ((1.0 + y * (ORDER.exp() - 1.0)).ln() / ORDER).clamp(0.0, 1.0)
}

/// Official parameter group of a source and its index within the group.
fn group(source: usize) -> (&'static str, usize) {
    match source {
        INPUT_1 => ("Input", 0),
        1 => ("USBInput", 0),
        virtual_pair => ("VirtualInput", virtual_pair - 2),
    }
}

/// Longest source name the official app stores.
pub const NAME_LIMIT: usize = 20;

/// Names the user gave the sources; empty for the default name. The device's
/// own loopback mixer shares them: its USB 1-2 and USB 3-4 are the same
/// playback channels as Main 1/2 and Virtual 3/4.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Names(pub [String; SOURCES]);

impl Names {
    /// The custom name of a source, if it has one.
    pub fn get(&self, source: usize) -> Option<&str> {
        Some(self.0.get(source)?.as_str()).filter(|name| !name.is_empty())
    }

    /// Sets a name; blank text restores the default.
    pub fn set(&mut self, source: usize, name: &str) {
        if let Some(slot) = self.0.get_mut(source) {
            *slot = name.trim().chars().take(NAME_LIMIT).collect();
        }
    }

    fn key(source: usize) -> String {
        let (name, i) = group(source);
        format!("SLS_{name}_Name_{i}")
    }

    /// Preset entries in the official names (`SLS_Input_Name_0`, …).
    pub fn entries(&self) -> Vec<(String, String)> {
        (0..SOURCES)
            .map(|source| (Self::key(source), self.0[source].clone()))
            .collect()
    }

    /// Applies one official name entry; `false` for other entries.
    pub fn apply_entry(&mut self, name: &str, value: &str) -> bool {
        match (0..SOURCES).find(|&source| Self::key(source) == name) {
            Some(source) => {
                self.set(source, value);
                true
            }
            None => false,
        }
    }
}

impl Sls {
    /// Preset entries in the official names, values normalized.
    pub fn entries(&self) -> Vec<(String, f32)> {
        let flag = |on: bool| if on { 1.0 } else { 0.0 };
        let mut entries = Vec::new();
        for (m, mix) in self.mixes.iter().enumerate() {
            for (source, s) in mix.sources.iter().enumerate() {
                let (name, i) = group(source);
                let key = |field: &str| format!("SLS_{name}_{field}_{m}_{i}");
                if source == INPUT_1 {
                    entries.push((key("Pan"), s.pan_left));
                } else {
                    entries.push((format!("{}_L", key("Pan")), s.pan_left));
                    entries.push((format!("{}_R", key("Pan")), s.pan_right));
                }
                entries.push((key("Level"), level_to_normalized(s.level_db)));
                entries.push((key("Mute"), flag(s.muted)));
                if name == "VirtualInput" {
                    entries.push((key("Show"), flag(s.shown)));
                }
            }
            entries.push((
                format!("SLS_Output_Level_{m}"),
                level_to_normalized(mix.output_db),
            ));
        }
        entries
    }

    /// Applies one official entry. Returns `false` for names it does not
    /// interpret (names, peak markers, the official app's spare entries).
    pub fn apply_entry(&mut self, name: &str, value: f32) -> bool {
        let parts: Vec<&str> = name.split('_').collect();
        let index = |i: usize| parts.get(i).and_then(|p| p.parse::<usize>().ok());
        if parts.first() != Some(&"SLS") {
            return false;
        }
        if parts.len() == 4 && parts[1] == "Output" && parts[2] == "Level" {
            return match index(3).filter(|&m| m < MIXES) {
                Some(m) => {
                    self.mixes[m].output_db = level_from_normalized(value);
                    true
                }
                None => false,
            };
        }
        let source = match (parts.get(1).copied(), index(4)) {
            (Some("Input"), Some(0)) => INPUT_1,
            (Some("USBInput"), Some(0)) => 1,
            (Some("VirtualInput"), Some(k)) if k < 3 => 2 + k,
            _ => return false,
        };
        let Some(mix) = index(3).filter(|&m| m < MIXES) else {
            return false;
        };
        let side = parts.get(5).copied();
        let s = &mut self.mixes[mix].sources[source];
        let on = value >= 0.5;
        match (parts.get(2).copied(), side) {
            (Some("Level"), None) => s.level_db = level_from_normalized(value),
            (Some("Mute"), None) => s.muted = on,
            (Some("Show"), None) if source >= 2 => s.shown = on,
            (Some("Pan"), None) if source == INPUT_1 => s.pan_left = value.clamp(0.0, 1.0),
            (Some("Pan"), Some("L")) if source != INPUT_1 => s.pan_left = value.clamp(0.0, 1.0),
            (Some("Pan"), Some("R")) if source != INPUT_1 => s.pan_right = value.clamp(0.0, 1.0),
            _ => return false,
        }
        // Presets only have the pans: channels on one spot, away from the
        // ends where stereo puts them together too, mean mono.
        if source != INPUT_1 {
            s.mono = s.pan_left == s.pan_right && s.pan_left > 0.0 && s.pan_left < 1.0;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain(gains: &Gains, from: Channel, to: Channel) -> i32 {
        gains
            .crosspoints
            .iter()
            .find(|p| p.0 == from && p.1 == to)
            .unwrap()
            .2
    }

    #[test]
    fn stereo_source_matches_the_official_gains() {
        let gains = Sls::default().source_gains(0, 1);
        let (left, right) = (ch(input::PLAYBACK, 0), ch(input::PLAYBACK, 1));
        let out = ch(output::DEVICE, 0);
        let out_r = ch(output::DEVICE, 1);
        assert_eq!(gain(&gains, left, out), 1 << 24);
        // Silence goes through -128 dB like in the official app.
        assert_eq!(gain(&gains, left, out_r), 6);
        assert_eq!(gain(&gains, right, out_r), 1 << 24);
        // Mirrored on the virtual "Loopback Main 7/8".
        assert_eq!(gain(&gains, left, ch(output::VIRTUAL, 2)), 1 << 24);
        assert_eq!(gains.crosspoints.len(), 8);
    }

    #[test]
    fn one_pan_moves_a_stereo_pair() {
        let mut sls = Sls::default();
        let pan = |sls: &mut Sls, pan| {
            sls.apply(SlsControl::Pan {
                mix: 0,
                source: 1,
                pan,
            });
            let s = sls.mixes[0].sources[1];
            (s.pan_left, s.pan_right, s.pan(true))
        };
        assert_eq!(pan(&mut sls, 0.5), (0.0, 1.0, 0.5));
        assert_eq!(pan(&mut sls, 0.25), (0.0, 0.5, 0.25));
        assert_eq!(pan(&mut sls, 0.0), (0.0, 0.0, 0.0));
        assert_eq!(pan(&mut sls, 0.75), (0.5, 1.0, 0.75));
        assert_eq!(pan(&mut sls, 1.0), (1.0, 1.0, 1.0));
        assert!(!sls.mixes[0].sources[1].mono);

        // Mono keeps the middle; its pan moves both channels together.
        pan(&mut sls, 0.75);
        let gains = sls.apply(SlsControl::Mono {
            mix: 0,
            source: 1,
            mono: true,
        });
        let s = sls.mixes[0].sources[1];
        assert!(s.mono);
        assert_eq!((s.pan_left, s.pan_right), (0.75, 0.75));
        let (left, right) = (ch(input::PLAYBACK, 0), ch(input::PLAYBACK, 1));
        let out = ch(output::DEVICE, 0);
        assert_eq!(gain(&gains, left, out), gain(&gains, right, out));
        assert_eq!(pan(&mut sls, 0.5), (0.5, 0.5, 0.5));
        sls.apply(SlsControl::Mono {
            mix: 0,
            source: 1,
            mono: false,
        });
        let s = sls.mixes[0].sources[1];
        assert_eq!((s.pan_left, s.pan_right, s.mono), (0.0, 1.0, false));
        // The mono input has one channel to pan.
        sls.apply(SlsControl::Pan {
            mix: 0,
            source: INPUT_1,
            pan: 0.25,
        });
        assert_eq!(sls.mixes[0].sources[INPUT_1].pan(false), 0.25);
        assert_eq!(sls.mixes[0].sources[INPUT_1].pan_right, 1.0);
    }

    #[test]
    fn the_input_is_monitored_while_heard_in_monitor_and_phones() {
        let mut sls = Sls::default();
        assert!(!sls.input_monitored(), "the factory preset mutes it");
        let mute = |muted| SlsControl::Mute {
            mix: 0,
            source: INPUT_1,
            muted,
        };
        sls.apply(mute(false));
        assert!(sls.input_monitored());
        sls.apply(SlsControl::Output {
            mix: 0,
            db: LEVEL_MIN,
        });
        assert!(!sls.input_monitored());
        let mut other = Sls::default();
        other.apply(SlsControl::Mute {
            mix: 1,
            source: INPUT_1,
            muted: false,
        });
        assert!(
            !other.input_monitored(),
            "recording mixes run with their apps"
        );
    }

    #[test]
    fn centred_mono_input_is_three_db_down_in_256ths() {
        let mut sls = Sls::default();
        let gains = sls.apply(SlsControl::Mute {
            mix: 1,
            source: INPUT_1,
            muted: false,
        });
        let from = ch(input::DEVICE, 0);
        // -3.0103 dB truncated to -770/256 dB.
        let expected = 11_866_681;
        assert!((gain(&gains, from, ch(output::RECORD, 2)) - expected).abs() <= 2);
        assert!((gain(&gains, from, ch(output::RECORD, 3)) - expected).abs() <= 2);
        let muted = sls.apply(SlsControl::Mute {
            mix: 1,
            source: INPUT_1,
            muted: true,
        });
        assert!(muted.crosspoints.iter().all(|p| p.2 == 0));
    }

    #[test]
    fn properties_have_the_driver_layout() {
        let gains = Gains {
            crosspoints: vec![(ch(1, 2), ch(0, 3), 0x0100_0000)],
            outputs: vec![(ch(2, 1), 0x0080_0000)],
        };
        assert_eq!(
            gains.properties(),
            vec![
                vec![0x4D, 0, 0, 0, 200, 0, 0, 0, 1, 2, 0, 3, 0, 0, 0, 1],
                vec![0x4D, 0, 0, 0, 201, 0, 0, 0, 2, 1, 0, 0, 0x80, 0],
            ]
        );
        let all = Sls::default().all_gains().properties();
        assert!(all.iter().all(|p| p.len() <= 8 + MAX_ENTRIES * 8));
        assert_eq!(
            meters_switch(true),
            vec![0x4D, 0, 0, 0, 0x2C, 1, 0, 0, 1, 0, 0, 0]
        );
    }

    #[test]
    fn silent_fader_and_output_level() {
        let mut sls = Sls::default();
        let gains = sls.apply(SlsControl::Output { mix: 2, db: -6.0 });
        assert_eq!(gains.outputs.len(), 2);
        let expected = (10f64.powf(-6.0 / 20.0) * 16_777_216.0) as i32;
        assert!((gains.outputs[0].1 - expected).abs() <= 2);
        let gains = sls.apply(SlsControl::Output { mix: 2, db: -60.0 });
        assert_eq!(gains.outputs[0].1, 0);
    }

    #[test]
    fn meters_round_trip() {
        let mut reply = meters_query();
        assert_eq!(reply.len(), 8 + 15 * 8);
        let levels: Vec<i16> = (0..15).map(|i| -100 * i).collect();
        for (entry, level) in reply[8..].as_chunks_mut::<8>().0.iter_mut().zip(&levels) {
            entry[6..8].copy_from_slice(&level.to_le_bytes());
        }
        let meters = parse_meters(&reply).unwrap();
        assert_eq!(meters.sources[0], [0.0, 0.0]);
        assert_eq!(meters.sources[1], [-1.0, -2.0]);
        assert_eq!(meters.mixes[2], [-13.0, -14.0]);
    }

    #[test]
    fn exp_mapping_matches_the_official_curve() {
        assert_eq!(level_from_normalized(1.0), 0.0);
        assert_eq!(level_from_normalized(0.0), -60.0);
        assert!((level_from_normalized(0.5) + 16.136).abs() < 0.01);
        for db in [-60.0, -40.0, -12.5, -3.0, 0.0] {
            let back = level_from_normalized(level_to_normalized(db));
            assert!((back - db).abs() < 1e-3, "{db} -> {back}");
        }
    }

    #[test]
    fn preset_entries_round_trip() {
        let mut sls = Sls::default();
        sls.apply(SlsControl::Level {
            mix: 2,
            source: 4,
            db: -20.0,
        });
        sls.apply(SlsControl::Pan {
            mix: 0,
            source: 1,
            pan: 0.75,
        });
        sls.mixes[1].sources[3].shown = true;
        sls.apply(SlsControl::Mono {
            mix: 1,
            source: 2,
            mono: true,
        });
        let mut read = Sls::default();
        read.mixes[0].output_db = -30.0;
        for (name, value) in sls.entries() {
            assert!(read.apply_entry(&name, value), "{name}");
        }
        for (a, b) in read.entries().iter().zip(sls.entries()) {
            assert_eq!(a.0, b.0);
            assert!((a.1 - b.1).abs() < 1e-5, "{}", a.0);
        }
        assert!(
            read.mixes[1].sources[2].mono,
            "mono comes back from the pans"
        );
        assert!(!read.mixes[0].sources[1].mono);
        assert!(!read.apply_entry("SLS_USBInput_Level_0_1", 1.0));
        assert!(!read.apply_entry("SLS_Input_PeakRef_0_0", 0.0));
        assert!(!read.apply_entry("SLS_Mix_Name_0", 0.0));
    }

    #[test]
    fn names_use_the_official_keys() {
        let mut names = Names::default();
        names.set(3, "  Zoom  ");
        names.set(0, "Микрофон с очень длинным названием");
        assert_eq!(names.get(3), Some("Zoom"));
        assert_eq!(names.get(0).unwrap().chars().count(), NAME_LIMIT);
        assert_eq!(names.get(1), None);
        let entries = names.entries();
        assert_eq!(entries[0].0, "SLS_Input_Name_0");
        assert_eq!(
            entries[1],
            ("SLS_USBInput_Name_0".to_owned(), String::new())
        );
        assert_eq!(
            entries[3],
            ("SLS_VirtualInput_Name_1".to_owned(), "Zoom".to_owned())
        );
        let mut read = Names::default();
        for (name, value) in &entries {
            assert!(read.apply_entry(name, value));
        }
        assert_eq!(read, names);
        assert!(!read.apply_entry("SLS_Mix_Name_0", "Monitor & Phones"));
        assert!(!read.apply_entry("SLS_USBInput_Name_1", ""));
    }
}
