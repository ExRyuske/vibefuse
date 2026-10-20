//! Settings this app keeps between runs.
//!
//! Like Arturia's MiniFuse Control Center, the loopback mix (macOS, Linux) and the software
//! mixer (Windows) are remembered per serial number on the computer and
//! pushed back to the device when it is connected: the firmware starts with
//! its defaults after every power-up, the driver's mixer after every
//! reconnection.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::device::DeviceInfo;
use crate::device::protocol::{Mix, RawState};
use crate::device::sls::{Names, Sls};
use crate::device::worker::Restoration;
use crate::i18n::Language;
use presets::Snapshot;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub restore_mix: bool,
    /// Loopback mix per device serial number.
    pub mixes: BTreeMap<String, Mix>,
    /// Software mixer per serial number.
    pub mixers: BTreeMap<String, Sls>,
    /// Channel names per serial number.
    pub names: BTreeMap<String, Names>,
    /// Preset entries this app does not interpret (channel names, peak
    /// markers), per serial, so exports carry them along.
    pub preset_extras: BTreeMap<String, Vec<(String, String)>>,
    /// Result of the last firmware update check.
    pub firmware: Option<FirmwareCheck>,
    pub language: Language,
    pub accent: Accent,
}

/// The accent color of the window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Orange,
    Pink,
    Teal,
    Violet,
    Blue,
}

impl Accent {
    /// In the order of `Theme.accents` in the window.
    pub const ALL: [Accent; 5] = [
        Accent::Orange,
        Accent::Pink,
        Accent::Teal,
        Accent::Violet,
        Accent::Blue,
    ];

    pub fn name(self) -> String {
        match self {
            Accent::Orange => crate::tr!("Orange"),
            Accent::Pink => crate::tr!("Pink"),
            Accent::Teal => crate::tr!("Teal"),
            Accent::Violet => crate::tr!("Violet"),
            Accent::Blue => crate::tr!("Blue"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareCheck {
    /// Seconds since the Unix epoch.
    pub checked_at: u64,
    pub latest: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            restore_mix: true,
            mixes: BTreeMap::new(),
            mixers: BTreeMap::new(),
            names: BTreeMap::new(),
            preset_extras: BTreeMap::new(),
            firmware: None,
            language: Language::System,
            accent: Accent::default(),
        }
    }
}

/// Where this app keeps its files: `~/Library/Application Support/VibeFuse`,
/// `%APPDATA%\VibeFuse`, `~/.config/vibefuse`.
pub fn app_dir() -> Option<PathBuf> {
    let name = if cfg!(target_os = "linux") {
        "vibefuse"
    } else {
        "VibeFuse"
    };
    Some(dirs::config_dir()?.join(name))
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        Some(app_dir()?.join("settings.json"))
    }

    /// The saved settings. A file that cannot be read is kept aside as
    /// `settings.json.broken` instead of being overwritten by the defaults.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            let _ = fs::rename(&path, path.with_extension("json.broken"));
            Self::default()
        })
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(temp, path)
    }

    /// What a freshly connected device should get: the settings saved
    /// here, or on first contact the ones Arturia's app remembered.
    pub fn snapshot_for(&self, info: &DeviceInfo, state: &RawState) -> Snapshot {
        let current = Snapshot {
            inputs: state.inputs(),
            mix: state.mix(),
            ..Snapshot::default()
        };
        let mix = self.mixes.get(&info.serial).copied();
        let sls = self.mixers.get(&info.serial).copied();
        let names = self.names.get(&info.serial).cloned();
        let official = (mix.is_none() || sls.is_none() || names.is_none())
            .then(|| presets::official_settings(&info.serial, current.clone()))
            .flatten()
            .map(|preset| preset.snapshot);
        Snapshot {
            inputs: current.inputs,
            mix: mix
                .or(official.as_ref().map(|s| s.mix))
                .unwrap_or(current.mix),
            sls: sls.or(official.as_ref().map(|s| s.sls)).unwrap_or_default(),
            names: names.or(official.map(|s| s.names)).unwrap_or_default(),
        }
    }
}

/// How a connected device is brought to its saved settings. With the
/// Windows driver's software mixer Arturia's app leaves the device's own
/// loopback mixer alone (it feeds "Virtual 3/4"), and so does this one.
pub fn restore(info: &DeviceInfo, state: &RawState, has_mixer: bool) -> Restoration {
    let settings = Settings::load();
    if !settings.restore_mix {
        return Restoration::default();
    }
    let snapshot = settings.snapshot_for(info, state);
    if has_mixer {
        return Restoration {
            writes: Vec::new(),
            mixer: Some(snapshot.sls),
        };
    }
    let writes = if snapshot.mix.approx_eq(&state.mix()) {
        Vec::new()
    } else {
        snapshot.mix.all_writes()
    };
    Restoration {
        writes,
        mixer: None,
    }
}

pub mod presets {
    //! Settings files of MiniFuse Control Center.
    //!
    //! * `.mfp` presets: `PresetVersion: 1`, then alternating name and value
    //!   lines. The official app only stores its Windows software mixer
    //!   (`SLS_*`, see [`crate::device::sls`]) and the channel names in them and
    //!   skips any name it does not save in presets, so hardware settings can
    //!   travel in the same file. Entries this app does not understand (mix
    //!   names, peak markers) are kept and written back unchanged.
    //! * `af.pref.xml`: where the official app remembers the settings of each
    //!   device (`<param name=… value=… device="serial"/>`).
    //!
    //! Values are normalized (0..=1) as in the official app. Hardware levels use
    //! its `PiecewiseLinear(0.842105263)` mapping, software mixer levels
    //! `Exp(-2)`.

    use std::path::PathBuf;

    use crate::device::protocol::{self, Inputs, Mix};
    use crate::device::sls::{Names, Sls};

    pub const EXTENSION: &str = "mfp";
    const HEADER: &str = "PresetVersion: ";
    const VERSION: u32 = 1;

    /// `k` of the official `PiecewiseLinear(0.842105263)` level mapping (16/19).
    const KNEE: f32 = 16.0 / 19.0;

    /// Normalized position to a value in `min..=max`: the first 75 % of the
    /// travel covers the fraction `KNEE` of the range.
    pub fn from_normalized(x: f32, min: f32, max: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        let fraction = if x < 0.75 {
            x * (4.0 / 3.0) * KNEE
        } else {
            KNEE + (1.0 - KNEE) * (4.0 * x - 3.0)
        };
        min + fraction * (max - min)
    }

    pub fn to_normalized(value: f32, min: f32, max: f32) -> f32 {
        let fraction = ((value - min) / (max - min)).clamp(0.0, 1.0);
        let x = if fraction >= KNEE {
            0.75 + 0.25 * (fraction - KNEE) / (1.0 - KNEE)
        } else {
            0.75 * fraction / KNEE
        };
        x.clamp(0.0, 1.0)
    }

    /// Everything a preset can hold.
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct Snapshot {
        pub inputs: Inputs,
        pub mix: Mix,
        pub sls: Sls,
        pub names: Names,
    }

    /// Hardware settings as `(name, normalized value)` in official names.
    fn hardware_entries(inputs: &Inputs, mix: &Mix) -> Vec<(String, f32)> {
        let flag = |on: bool| if on { 1.0 } else { 0.0 };
        let level = |db: f32| to_normalized(db, protocol::MIX_LEVEL_MIN, protocol::MIX_LEVEL_MAX);
        [
            ("Input_INST", flag(inputs.inst)),
            ("Input_48V", flag(inputs.phantom)),
            ("Output_DirectMono", flag(inputs.direct_monitor)),
            ("LoopBack", flag(mix.enabled)),
            ("LoopbackMix_ChanLevel_0", level(mix.input1.level_db)),
            ("LoopbackMix_ChanPan_0", mix.input1.pan),
            ("LoopbackMix_InputMute_0", flag(mix.input1.muted)),
            ("LoopbackMix_ChanLevelLeft_2", level(mix.usb12.level_db)),
            ("LoopbackMix_InputMute_2", flag(mix.usb12.muted)),
            ("LoopbackMix_ChanLevelLeft_4", level(mix.usb34.level_db)),
            ("LoopbackMix_InputMute_4", flag(mix.usb34.muted)),
            (
                "LoopbackMix_OutputLevel",
                to_normalized(
                    mix.output_db,
                    protocol::OUTPUT_LEVEL_MIN,
                    protocol::OUTPUT_LEVEL_MAX,
                ),
            ),
            ("LoopbackMix_To_USBIn1_2", flag(mix.to_usb12)),
            ("LoopbackMix_To_Speakers_Phones", flag(mix.to_outputs)),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
    }

    /// Every setting in official names, software mixer first like the official
    /// files.
    pub fn entries(snapshot: &Snapshot) -> Vec<(String, f32)> {
        let mut entries = snapshot.sls.entries();
        entries.extend(hardware_entries(&snapshot.inputs, &snapshot.mix));
        entries
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Group {
        Hardware,
        Mixer,
        Names,
    }

    fn apply_entry(name: &str, value: &str, snapshot: &mut Snapshot) -> Option<Group> {
        if snapshot.names.apply_entry(name, value) {
            return Some(Group::Names);
        }
        let value = value.trim().parse::<f32>().ok()?;
        if apply_hardware(name, value, &mut snapshot.inputs, &mut snapshot.mix) {
            Some(Group::Hardware)
        } else if snapshot.sls.apply_entry(name, value) {
            Some(Group::Mixer)
        } else {
            None
        }
    }

    /// Applies one official hardware entry. Returns `false` for other names.
    fn apply_hardware(name: &str, value: f32, inputs: &mut Inputs, mix: &mut Mix) -> bool {
        let on = value >= 0.5;
        let level = || from_normalized(value, protocol::MIX_LEVEL_MIN, protocol::MIX_LEVEL_MAX);
        match name {
            "Input_INST" => inputs.inst = on,
            "Input_48V" => inputs.phantom = on,
            "Output_DirectMono" => inputs.direct_monitor = on,
            "LoopBack" => mix.enabled = on,
            "LoopbackMix_ChanLevel_0" => mix.input1.level_db = level(),
            "LoopbackMix_ChanPan_0" => mix.input1.pan = value.clamp(0.0, 1.0),
            "LoopbackMix_InputMute_0" => mix.input1.muted = on,
            "LoopbackMix_ChanLevelLeft_2" => mix.usb12.level_db = level(),
            "LoopbackMix_InputMute_2" => mix.usb12.muted = on,
            "LoopbackMix_ChanLevelLeft_4" => mix.usb34.level_db = level(),
            "LoopbackMix_InputMute_4" => mix.usb34.muted = on,
            "LoopbackMix_OutputLevel" => {
                mix.output_db = from_normalized(
                    value,
                    protocol::OUTPUT_LEVEL_MIN,
                    protocol::OUTPUT_LEVEL_MAX,
                );
            }
            "LoopbackMix_To_USBIn1_2" => mix.to_usb12 = on,
            "LoopbackMix_To_Speakers_Phones" => mix.to_outputs = on,
            _ => return false,
        }
        true
    }

    /// Formats like the official writer (`std::setprecision(7)`).
    fn format_value(value: f32) -> String {
        let text = format!("{value:.7}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        if text == "-0" {
            "0".to_owned()
        } else {
            text.to_owned()
        }
    }

    /// What an imported file contained.
    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct Preset {
        pub snapshot: Snapshot,
        /// Number of hardware entries found.
        pub hardware: usize,
        /// Number of software mixer entries found.
        pub mixer: usize,
        /// Number of channel names found.
        pub names: usize,
        /// Entries kept verbatim for re-export.
        pub other: Vec<(String, String)>,
    }

    impl Preset {
        fn new(snapshot: Snapshot) -> Self {
            Self {
                snapshot,
                ..Self::default()
            }
        }

        fn count(&mut self, group: Option<Group>) -> bool {
            match group {
                Some(Group::Hardware) => self.hardware += 1,
                Some(Group::Mixer) => self.mixer += 1,
                Some(Group::Names) => self.names += 1,
                None => return false,
            }
            true
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ParseError(pub String);

    impl std::fmt::Display for ParseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    /// Reads an `.mfp` preset on top of the current settings.
    pub fn parse_mfp(text: &str, current: Snapshot) -> Result<Preset, ParseError> {
        let mut lines = text
            .lines()
            .map(|line| line.strip_suffix('\r').unwrap_or(line));
        let header = lines.next().unwrap_or_default();
        let version = header
            .strip_prefix(HEADER)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .ok_or_else(|| ParseError(crate::tr!("This is not a MiniFuse preset")))?;
        if version > VERSION {
            return Err(ParseError(crate::tr!(
                "Unknown preset version: {}",
                version
            )));
        }
        let mut preset = Preset::new(current);
        while let Some(name) = lines.next() {
            if name.is_empty() {
                continue;
            }
            let value = lines.next().unwrap_or_default();
            let group = apply_entry(name, value, &mut preset.snapshot);
            if !preset.count(group) {
                preset.other.push((name.to_owned(), value.to_owned()));
            }
        }
        Ok(preset)
    }

    /// Writes an `.mfp` preset the official app imports without complaint.
    pub fn to_mfp(snapshot: &Snapshot, other: &[(String, String)]) -> String {
        let names = snapshot.names.entries();
        let numbers = entries(snapshot);
        let ours = |name: &str| {
            names.iter().any(|(n, _)| n == name) || numbers.iter().any(|(n, _)| n == name)
        };
        let mut text = format!("{HEADER}{VERSION}\n");
        for (name, value) in other.iter().filter(|(name, _)| !ours(name)) {
            text.push_str(&format!("{name}\n{value}\n"));
        }
        for (name, value) in &names {
            text.push_str(&format!("{name}\n{value}\n"));
        }
        for (name, value) in numbers {
            text.push_str(&format!("{name}\n{}\n", format_value(value)));
        }
        text
    }

    fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let pattern = format!("{name}=\"");
        let start = tag.find(&pattern)? + pattern.len();
        let end = tag[start..].find('"')? + start;
        Some(&tag[start..end])
    }

    /// Reads the settings the official app stored for a device. Returns `None`
    /// when the file has nothing for it.
    pub fn parse_official_prefs(xml: &str, serial: &str, current: Snapshot) -> Option<Preset> {
        let params: Vec<(&str, &str, Option<&str>)> = xml
            .split("<param ")
            .skip(1)
            .filter_map(|tag| {
                let tag = &tag[..tag.find('>')?];
                Some((
                    attribute(tag, "name")?,
                    attribute(tag, "value")?,
                    attribute(tag, "device"),
                ))
            })
            .collect();
        let mut devices: Vec<&str> = params.iter().filter_map(|p| p.2).collect();
        devices.sort_unstable();
        devices.dedup();
        let device = if devices.contains(&serial) {
            serial
        } else if devices.len() == 1 && serial.is_empty() {
            devices[0]
        } else {
            return None;
        };

        let mut preset = Preset::new(current);
        for (name, value, _) in params.iter().filter(|p| p.2 == Some(device)) {
            let group = apply_entry(name, value, &mut preset.snapshot);
            preset.count(group);
        }
        (preset.hardware + preset.mixer + preset.names > 0).then_some(preset)
    }

    /// Where the official app keeps `af.pref.xml`.
    pub fn official_prefs_paths() -> Vec<PathBuf> {
        let relative = [
            "Arturia",
            "MiniFuse Control Center",
            "resources",
            "tmp",
            "af.pref.xml",
        ];
        let mut roots = Vec::new();
        if cfg!(target_os = "macos") {
            roots.push(PathBuf::from("/Library"));
        }
        for var in ["ProgramW6432", "ProgramFiles", "ProgramData"] {
            if let Some(dir) = std::env::var_os(var) {
                roots.push(PathBuf::from(dir));
            }
        }
        roots
            .into_iter()
            .map(|root| relative.iter().fold(root, |path, part| path.join(part)))
            .collect()
    }

    /// The official app's saved settings for a device, if it has any.
    pub fn official_settings(serial: &str, current: Snapshot) -> Option<Preset> {
        official_prefs_paths()
            .into_iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .find_map(|xml| parse_official_prefs(&xml, serial, current.clone()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::device::protocol::{MonoStrip, StereoStrip};

        #[test]
        fn piecewise_mapping_round_trips() {
            assert_eq!(from_normalized(0.0, -60.0, 12.0), -60.0);
            assert!((from_normalized(1.0, -60.0, 12.0) - 12.0).abs() < 1e-4);
            assert!((from_normalized(1.0, -64.0, 0.0)).abs() < 1e-4);
            for db in [-60.0, -30.0, -6.0, 0.0, 0.5, 6.0, 12.0] {
                let back = from_normalized(to_normalized(db, -60.0, 12.0), -60.0, 12.0);
                assert!((back - db).abs() < 1e-3, "{db} -> {back}");
            }
        }

        #[test]
        fn reads_official_factory_preset() {
            let text = "PresetVersion: 1\nSLS_Mix_Name_0\nMonitor & Phones\nSLS_Input_Name_0\n\n\
                SLS_Input_Pan_0_0\n0.25\nSLS_USBInput_Mute_0_0\n1\nSLS_VirtualInput_Level_2_1\n0.5\n\
                SLS_Input_PeakRef_0_0\n0\n";
            let mut current = Snapshot::default();
            current.names.set(0, "Vocal");
            let preset = parse_mfp(text, current).unwrap();
            assert_eq!((preset.hardware, preset.mixer, preset.names), (0, 3, 1));
            let sls = preset.snapshot.sls;
            assert_eq!(sls.mixes[0].sources[0].pan_left, 0.25);
            assert!(sls.mixes[0].sources[1].muted);
            assert!((sls.mixes[2].sources[3].level_db + 16.136).abs() < 0.01);
            assert_eq!(preset.snapshot.names.get(0), None, "the preset clears it");
            assert_eq!(preset.other.len(), 2);
            assert_eq!(
                preset.other[1],
                ("SLS_Input_PeakRef_0_0".to_owned(), "0".to_owned())
            );
        }

        #[test]
        fn official_factory_file_is_fully_understood() {
            let path = "/Library/Arturia/MiniFuse Control Center/resources/devices/Minifuse1/preset/Factory/Default.mfp";
            let Ok(text) = std::fs::read_to_string(path) else {
                return;
            };
            let mut start = Snapshot::default();
            start.sls.mixes[1].output_db = -40.0;
            let preset = parse_mfp(&text, start).unwrap();
            assert_eq!(preset.snapshot.sls, Sls::default());
            assert!(
                preset
                    .other
                    .iter()
                    .all(|(name, _)| !name.contains("_Level_")
                        || name.starts_with("SLS_USBInput_Level_"))
            );
        }

        #[test]
        fn export_round_trips_and_keeps_foreign_entries() {
            let inputs = Inputs {
                inst: true,
                phantom: false,
                direct_monitor: true,
            };
            let mix = Mix {
                enabled: true,
                input1: MonoStrip {
                    level_db: -6.0,
                    pan: 0.25,
                    muted: false,
                },
                usb12: StereoStrip {
                    level_db: 3.0,
                    muted: true,
                },
                usb34: StereoStrip {
                    level_db: -20.0,
                    muted: false,
                },
                output_db: -10.0,
                to_usb12: true,
                to_outputs: false,
            };
            let mut sls = Sls::default();
            sls.mixes[2].sources[0].level_db = -12.0;
            sls.mixes[0].output_db = -3.0;
            let mut names = Names::default();
            names.set(4, "DAW");
            let snapshot = Snapshot {
                inputs,
                mix,
                sls,
                names,
            };
            let other = vec![
                ("SLS_Mix_Name_0".to_owned(), "Monitor & Phones".to_owned()),
                // Kept from an older import; the current names win.
                ("SLS_VirtualInput_Name_2".to_owned(), "old".to_owned()),
            ];
            let text = to_mfp(&snapshot, &other);
            assert!(text.starts_with(
                "PresetVersion: 1\nSLS_Mix_Name_0\nMonitor & Phones\nSLS_Input_Name_0\n\n"
            ));
            assert!(text.contains("SLS_VirtualInput_Name_2\nDAW\n"));
            assert!(!text.contains("old"));
            let preset = parse_mfp(&text.replace('\n', "\r\n"), Snapshot::default()).unwrap();
            assert_eq!(preset.snapshot.inputs, inputs);
            assert!(
                preset.snapshot.mix.approx_eq(&mix),
                "{:?}",
                preset.snapshot.mix
            );
            let read = preset.snapshot.sls;
            assert!((read.mixes[2].sources[0].level_db + 12.0).abs() < 1e-3);
            assert!((read.mixes[0].output_db + 3.0).abs() < 1e-3);
            assert_eq!(preset.snapshot.names.get(4), Some("DAW"));
            assert_eq!(preset.other, other[..1]);
            assert_eq!(preset.hardware, 14);
            assert_eq!(preset.mixer, sls.entries().len());
            assert_eq!(preset.names, 5);
        }

        #[test]
        fn reads_official_device_prefs() {
            let xml = r#"<rootnode>
    	<param name="BASE_Size" value="0.500000"/>
    	<param name="LoopBack" value="1.000000" device="8831400564280731"/>
    	<param name="LoopbackMix_OutputLevel" value="1.000000" device="8831400564280731"/>
    	<param name="LoopbackMix_ChanLevelLeft_2" value="0.000000" device="8831400564280731"/>
    	<param name="LoopbackMix_ChanPan_0" value="0.500000" device="8831400564280731"/>
    	<param name="LoopbackMix_InputMute_4" value="1.000000" device="8831400564280731"/>
    </rootnode>"#;
            let preset =
                parse_official_prefs(xml, "8831400564280731", Snapshot::default()).unwrap();
            assert_eq!(preset.hardware, 5);
            let mix = preset.snapshot.mix;
            assert!(mix.enabled);
            assert_eq!(mix.output_db, 0.0);
            assert_eq!(mix.usb12.level_db, -60.0);
            assert!(mix.usb34.muted);
            assert!(parse_official_prefs(xml, "other", Snapshot::default()).is_none());
        }

        #[test]
        fn values_are_written_like_the_official_app() {
            assert_eq!(format_value(1.0), "1");
            assert_eq!(format_value(0.0), "0");
            assert_eq!(format_value(0.5), "0.5");
            assert_eq!(format_value(0.842_105_3), "0.8421053");
        }
    }
}
