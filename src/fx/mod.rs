//! Effects on the microphone: a chain of VST3, VST2, Audio Unit (macOS) and
//! LADSPA plug-ins on Input 1, before it reaches the mixes.
//!
//! The chain runs in the callback of a stream that takes the input and plays
//! the result on the output the user picked ([`output`]), so it adds no
//! buffering of its own. Plug-ins are made, edited and taken down on the
//! window's thread; their processors live in the [`rack`] the audio thread
//! runs.

pub mod graph;
pub mod rack;
mod scan;

#[cfg(windows)]
pub mod asio;
#[cfg(target_os = "macos")]
mod au;
mod ladspa;
pub mod output;
mod vst2;
mod vst3;
mod window;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use graph::{Loaded, ParamChange, ParamInfo, PluginFormat};
use rack::Insert;
use scan::{ScanCache, ScanJob};

pub type Result<T> = std::result::Result<T, String>;

#[cfg(test)]
pub(crate) use ladspa::tests::build_amplifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Format {
    Vst3,
    Vst2,
    /// Audio Units, macOS only.
    Au,
    Ladspa,
}

impl Format {
    /// The formats of this system, in the order the list shows them.
    pub const ALL: &[Format] = if cfg!(target_os = "macos") {
        &[Format::Vst3, Format::Au, Format::Vst2, Format::Ladspa]
    } else {
        &[Format::Vst3, Format::Vst2, Format::Ladspa]
    };

    pub fn label(self) -> &'static str {
        match self {
            Format::Vst3 => "VST3",
            Format::Vst2 => "VST2",
            Format::Au => "AU",
            Format::Ladspa => "LADSPA",
        }
    }

    /// File name extensions of its plug-ins.
    fn extensions(self) -> &'static [&'static str] {
        match self {
            Format::Vst3 => &["vst3"],
            Format::Vst2 if cfg!(windows) => &["dll"],
            Format::Vst2 => &["vst"],
            Format::Ladspa if cfg!(windows) => &["dll"],
            Format::Ladspa => &["so", "dylib"],
            Format::Au => &[],
        }
    }

    fn backend(self) -> Option<Box<dyn PluginFormat>> {
        match self {
            Format::Vst3 => Some(Box::new(vst3::Vst3Format)),
            Format::Vst2 => Some(Box::new(vst2::Vst2Format)),
            Format::Ladspa => Some(Box::new(ladspa::LadspaFormat)),
            Format::Au => None,
        }
    }
}

/// A plug-in as the list of installed ones shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    pub format: Format,
    /// The file or bundle; empty for Audio Units.
    pub path: PathBuf,
    /// Tells plug-ins of one file apart: a VST3 class id, a VST2 or LADSPA
    /// id, the codes of an Audio Unit.
    pub uid: String,
    pub name: String,
    pub vendor: String,
    pub is_instrument: bool,
}

impl Descriptor {
    fn instantiate(&self, rate: f64) -> Result<Loaded> {
        if self.format != Format::Au && !self.path.exists() {
            return Err(crate::tr!("Not installed"));
        }
        match self.format {
            #[cfg(target_os = "macos")]
            Format::Au => au::instantiate(self),
            format => format
                .backend()
                .ok_or_else(|| format!("{}: not supported here", format.label()))?
                .instantiate(self, rate),
        }
    }
}

fn default_format() -> Format {
    Format::Vst3
}

/// A plug-in of the saved chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Chains from before other formats had only VST3.
    #[serde(default = "default_format")]
    pub format: Format,
    pub path: PathBuf,
    pub uid: String,
    /// Shown while the plug-in cannot be loaded.
    pub name: String,
    pub bypassed: bool,
    /// Normalized values by parameter id.
    #[serde(default)]
    pub params: Vec<(u32, f64)>,
    /// The plug-in's own state, in hex: parameters do not cover everything.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
}

impl Entry {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            format: self.format,
            path: self.path.clone(),
            uid: self.uid.clone(),
            name: self.name.clone(),
            vendor: String::new(),
            is_instrument: false,
        }
    }
}

/// A chain file, as exported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainFile {
    #[serde(default)]
    pub version: u32,
    pub chain: Vec<Entry>,
}

pub const CHAIN_EXTENSION: &str = "json";
const CHAIN_VERSION: u32 = 1;

pub mod hex {
    //! Bytes as hex digits: states in the settings, VST3 class ids.

    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";

    pub fn encode(data: &[u8]) -> String {
        data.iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0F])
            .map(|digit| DIGITS[usize::from(digit)] as char)
            .collect()
    }

    /// `None` for an odd length or a character that is no hex digit.
    pub fn decode(text: &str) -> Option<Vec<u8>> {
        if !text.len().is_multiple_of(2) {
            return None;
        }
        (0..text.len() / 2)
            .map(|i| u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok())
            .collect()
    }
}

/// The parts of a loaded plug-in that stay with the window.
struct Plugin {
    parameters: Vec<ParamInfo>,
    values: Vec<f64>,
    bridge: std::sync::Arc<graph::ParamBridge>,
    formatter: Option<Box<dyn graph::ParamFormatter>>,
    editor: Option<Box<dyn graph::PluginEditor>>,
    notifications: Option<Box<dyn graph::EditNotifications>>,
    state: Option<Box<dyn graph::PluginState>>,
}

impl Drop for Plugin {
    /// The editor lets go of its window first.
    fn drop(&mut self) {
        if let Some(editor) = &mut self.editor {
            editor.close();
        }
    }
}

/// A plug-in of the chain, on the window's side.
pub struct Slot {
    id: u64,
    pub entry: Entry,
    plugin: Option<Plugin>,
    /// Why it does not run.
    pub error: Option<String>,
}

impl Slot {
    pub fn has_editor(&self) -> bool {
        self.plugin.as_ref().is_some_and(|p| p.editor.is_some())
    }

    pub fn editor_open(&self) -> bool {
        self.plugin
            .as_ref()
            .and_then(|p| p.editor.as_ref())
            .is_some_and(|e| e.is_open())
    }
}

/// A parameter as the window lists it.
pub struct Param {
    pub id: u32,
    pub title: String,
    pub value: f64,
    pub text: String,
    pub read_only: bool,
}

/// The chain on the window's side: plug-ins in order, the installed ones
/// to pick from, the master switch.
pub struct Effects {
    slots: Vec<Slot>,
    enabled: bool,
    /// The rate the chain is set up for.
    rate: f64,
    installed: Vec<Descriptor>,
    scanned: bool,
    cache: Option<ScanCache>,
    /// Folders the user added.
    pub folders: Vec<PathBuf>,
}

/// The rate before anything tells another.
const DEFAULT_RATE: f64 = 48_000.0;

/// Slots and their inserts share ids, unique in the process.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Effects {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            enabled: false,
            rate: DEFAULT_RATE,
            installed: Vec::new(),
            scanned: false,
            cache: None,
            folders: Vec::new(),
        }
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the chain changes the sound: on, with a plug-in that runs.
    pub fn active(&self) -> bool {
        self.enabled
            && self
                .slots
                .iter()
                .any(|s| s.plugin.is_some() && !s.entry.bypassed && s.error.is_none())
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        rack::set_enabled(on);
    }

    /// Sets the chain up for another rate.
    pub fn set_rate(&mut self, rate: f64) {
        if rate <= 0.0 || rate == self.rate {
            return;
        }
        self.rate = rate;
        let failed = rack::lock().activate(rate);
        for slot in &mut self.slots {
            slot.error = failed
                .iter()
                .find(|(id, _)| *id == slot.id)
                .map(|(_, error)| error.clone());
        }
    }

    /// Loads a saved chain in place of the current one.
    pub fn load(&mut self, entries: &[Entry]) {
        self.clear();
        for entry in entries {
            self.push(entry.clone());
        }
    }

    pub fn clear(&mut self) {
        let removed: Vec<Insert> = {
            let mut rack = rack::lock();
            let (ours, others) = std::mem::take(&mut rack.inserts)
                .into_iter()
                .partition(|insert| self.slots.iter().any(|s| s.id == insert.id));
            rack.inserts = others;
            ours
        };
        // The window's objects point into the plug-ins: they go first.
        self.slots.clear();
        drop(removed);
    }

    /// Adds a plug-in at the end; a failure is kept in the slot, so a
    /// missing plug-in stays in the saved chain.
    pub fn push(&mut self, entry: Entry) -> usize {
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut slot = Slot {
            id,
            entry,
            plugin: None,
            error: None,
        };
        match self.make(&mut slot) {
            Ok(processor) => {
                let mut rack = rack::lock();
                rack.prepare(self.rate);
                rack.inserts.push(processor);
            }
            Err(error) => slot.error = Some(error),
        }
        self.slots.push(slot);
        self.slots.len() - 1
    }

    pub fn add(&mut self, descriptor: &Descriptor) -> usize {
        self.push(Entry {
            format: descriptor.format,
            path: descriptor.path.clone(),
            uid: descriptor.uid.clone(),
            name: descriptor.name.clone(),
            bypassed: false,
            params: Vec::new(),
            state: String::new(),
        })
    }

    /// Instantiates, restores and activates the slot's plug-in.
    fn make(&self, slot: &mut Slot) -> Result<Insert> {
        let entry = &slot.entry;
        let loaded = entry.descriptor().instantiate(self.rate)?;
        let Loaded {
            parameters,
            mut values,
            bridge,
            formatter,
            editor,
            notifications,
            state,
            mut processor,
        } = loaded;
        // The state first, then the parameters it may not cover.
        if let (Some(state), Some(bytes)) = (&state, hex::decode(&entry.state))
            && !bytes.is_empty()
        {
            state.restore(&bytes);
        }
        if let Some(formatter) = &formatter {
            for (value, info) in values.iter_mut().zip(&parameters) {
                *value = formatter.get(info.id).unwrap_or(*value);
            }
        }
        for &(id, value) in &entry.params {
            if let Some(index) = parameters.iter().position(|p| p.id == id && !p.read_only) {
                values[index] = value;
                bridge.send(ParamChange { id, value });
                if let Some(formatter) = &formatter {
                    formatter.set(id, value);
                }
            }
        }
        processor.activate(self.rate, rack::MAX_BLOCK, rack::CHANNELS)?;
        slot.plugin = Some(Plugin {
            parameters,
            values,
            bridge,
            formatter,
            editor,
            notifications,
            state,
        });
        Ok(Insert {
            id: slot.id,
            processor,
            bypassed: entry.bypassed,
            active: true,
        })
    }

    pub fn remove(&mut self, index: usize) {
        if index >= self.slots.len() {
            return;
        }
        let slot = self.slots.remove(index);
        let insert = {
            let mut rack = rack::lock();
            let position = rack.inserts.iter().position(|i| i.id == slot.id);
            position.map(|p| rack.inserts.remove(p))
        };
        drop(slot);
        drop(insert);
    }

    /// Moves a plug-in one place up (`-1`) or down (`1`).
    pub fn shift(&mut self, index: usize, by: isize) {
        let Some(to) = index
            .checked_add_signed(by)
            .filter(|&to| to < self.slots.len())
        else {
            return;
        };
        if index >= self.slots.len() {
            return;
        }
        self.slots.swap(index, to);
        let order: Vec<u64> = self.slots.iter().map(|s| s.id).collect();
        rack::lock()
            .inserts
            .sort_by_key(|insert| order.iter().position(|&id| id == insert.id));
    }

    pub fn set_bypassed(&mut self, index: usize, bypassed: bool) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        slot.entry.bypassed = bypassed;
        if let Some(insert) = rack::lock().inserts.iter_mut().find(|i| i.id == slot.id) {
            insert.bypassed = bypassed;
        }
    }

    pub fn open_editor(&mut self, index: usize) -> Result<()> {
        let Some(slot) = self.slots.get_mut(index) else {
            return Ok(());
        };
        let title = slot.entry.name.clone();
        match slot.plugin.as_mut().and_then(|p| p.editor.as_mut()) {
            Some(editor) => editor.open(&title),
            None => Err(format!("{title}: no window of its own")),
        }
    }

    pub fn close_editors(&mut self) {
        for editor in self
            .slots
            .iter_mut()
            .filter_map(|s| s.plugin.as_mut()?.editor.as_mut())
        {
            editor.close();
        }
    }

    pub fn any_editor_open(&self) -> bool {
        self.slots.iter().any(Slot::editor_open)
    }

    /// Keeps open editors alive, takes in the edits made in them and starts
    /// over the plug-ins that asked to. Returns whether any value changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut restart = Vec::new();
        for slot in &mut self.slots {
            let Some(plugin) = slot.plugin.as_mut() else {
                continue;
            };
            if let Some(editor) = &mut plugin.editor
                && editor.is_open()
            {
                editor.poll();
            }
            let Some(notifications) = &plugin.notifications else {
                continue;
            };
            if notifications.wants_restart() {
                restart.push(slot.id);
            }
            for edit in notifications.drain() {
                if let Some(index) = plugin.parameters.iter().position(|p| p.id == edit.id) {
                    plugin.values[index] = edit.value;
                    changed = true;
                }
            }
        }
        if !restart.is_empty() {
            let mut rack = rack::lock();
            for insert in rack.inserts.iter_mut().filter(|i| restart.contains(&i.id)) {
                insert.processor.deactivate();
                let started = insert
                    .processor
                    .activate(self.rate, rack::MAX_BLOCK, rack::CHANNELS);
                insert.active = started.is_ok();
                if let (Err(error), Some(slot)) =
                    (started, self.slots.iter_mut().find(|s| s.id == insert.id))
                {
                    slot.error = Some(error);
                }
            }
        }
        changed
    }

    /// The parameters of a plug-in with their values as it shows them.
    pub fn params(&mut self, index: usize) -> Vec<Param> {
        let Some(plugin) = self.slots.get_mut(index).and_then(|s| s.plugin.as_mut()) else {
            return Vec::new();
        };
        // Follows the plug-in's own window, which may change any of them.
        if let Some(formatter) = &plugin.formatter {
            for (value, info) in plugin.values.iter_mut().zip(&plugin.parameters) {
                if let Some(now) = formatter.get(info.id) {
                    *value = now;
                }
            }
        }
        plugin
            .parameters
            .iter()
            .zip(&plugin.values)
            .map(|(info, &value)| Param {
                id: info.id,
                title: info.title.clone(),
                value,
                text: describe(plugin, info, value),
                read_only: info.read_only,
            })
            .collect()
    }

    /// One parameter, after the window changed it.
    pub fn param(&mut self, index: usize, id: u32) -> Option<Param> {
        let plugin = self.slots.get(index)?.plugin.as_ref()?;
        let position = plugin.parameters.iter().position(|p| p.id == id)?;
        let (info, value) = (&plugin.parameters[position], plugin.values[position]);
        Some(Param {
            id,
            title: info.title.clone(),
            value,
            text: describe(plugin, info, value),
            read_only: info.read_only,
        })
    }

    /// Sets a parameter from typed text, as the plug-in reads it, or as a
    /// percentage ("50%"). False when the text means nothing to it.
    pub fn type_param(&mut self, index: usize, id: u32, text: &str) -> bool {
        let Some(plugin) = self.slots.get(index).and_then(|s| s.plugin.as_ref()) else {
            return false;
        };
        let parsed = plugin.formatter.as_ref().and_then(|f| f.parse(id, text));
        let percent = || {
            let text = text.trim().strip_suffix('%')?;
            Some(graph::leading_number(text)? / 100.0)
        };
        match parsed.or_else(percent) {
            Some(value) => {
                self.set_param(index, id, value);
                true
            }
            None => false,
        }
    }

    pub fn set_param(&mut self, index: usize, id: u32, value: f64) {
        let Some(plugin) = self.slots.get_mut(index).and_then(|s| s.plugin.as_mut()) else {
            return;
        };
        let value = value.clamp(0.0, 1.0);
        if let Some(i) = plugin.parameters.iter().position(|p| p.id == id) {
            plugin.values[i] = value;
        }
        plugin.bridge.send(ParamChange { id, value });
        if let Some(formatter) = &plugin.formatter {
            formatter.set(id, value);
        }
    }

    /// The chain as it is now, to save: values and states included.
    pub fn entries(&self) -> Vec<Entry> {
        self.slots
            .iter()
            .map(|slot| {
                let mut entry = slot.entry.clone();
                if let Some(plugin) = &slot.plugin {
                    entry.params = plugin
                        .parameters
                        .iter()
                        .zip(&plugin.values)
                        .filter(|(info, _)| !info.read_only)
                        .map(|(info, &value)| (info.id, value))
                        .collect();
                    if let Some(state) = plugin.state.as_ref().and_then(|s| s.save()) {
                        entry.state = hex::encode(&state);
                    }
                }
                entry
            })
            .collect()
    }

    pub fn export(&self) -> String {
        let file = ChainFile {
            version: CHAIN_VERSION,
            chain: self.entries(),
        };
        serde_json::to_string_pretty(&file).unwrap_or_default()
    }

    /// Reads a chain file.
    pub fn parse(text: &str) -> Result<Vec<Entry>> {
        let file: ChainFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if file.version > CHAIN_VERSION {
            return Err(crate::tr!("The file is from a newer version"));
        }
        Ok(file.chain)
    }

    /// Points a chain saved elsewhere (another computer, the other system)
    /// at the plug-ins installed here, by name, where its own are missing.
    /// The same plug-in in another format starts from its defaults: its
    /// parameters and state do not carry over.
    pub fn resolve(&mut self, chain: &mut [Entry]) {
        let missing = |entry: &Entry| match entry.format {
            Format::Au => !cfg!(target_os = "macos"),
            _ => !entry.path.exists(),
        };
        if !chain.iter().any(missing) {
            return;
        }
        if !self.scanned {
            self.scan(false);
        }
        for entry in chain.iter_mut().filter(|e| missing(e)) {
            let named = |format: Format| {
                self.installed
                    .iter()
                    .find(|d| d.format == format && d.name.eq_ignore_ascii_case(&entry.name))
            };
            let found = named(entry.format).or_else(|| Format::ALL.iter().find_map(|&f| named(f)));
            let Some(found) = found.cloned() else {
                continue;
            };
            if found.format != entry.format {
                entry.params.clear();
                entry.state.clear();
            }
            entry.format = found.format;
            entry.path = found.path;
            entry.uid = found.uid;
        }
    }

    /// The installed effects found by the last [`Self::scan`].
    pub fn installed(&self) -> &[Descriptor] {
        &self.installed
    }

    pub fn scanned(&self) -> bool {
        self.scanned
    }

    /// The file the last scan crashed on, once.
    pub fn crashed(&mut self) -> Option<PathBuf> {
        self.cache.as_mut()?.take_crashed()
    }

    /// Looks for installed effects. Scanning loads plug-ins: it happens on
    /// the window's thread, when asked for. Files that did not change are
    /// not opened again unless `fresh`.
    pub fn scan(&mut self, fresh: bool) {
        let cache = self.cache.get_or_insert_with(ScanCache::load);
        let jobs: Vec<ScanJob> = Format::ALL
            .iter()
            .filter_map(|&format| {
                let backend = format.backend()?;
                let mut folders = scan::default_paths(format);
                folders.extend(self.folders.iter().cloned());
                Some((backend, folders))
            })
            .collect();
        let mut found = cache.scan(&jobs, fresh);
        #[cfg(target_os = "macos")]
        found.extend(au::list());
        found.retain(|d| !d.is_instrument);
        found.sort_by(|a, b| {
            let order = |f: Format| Format::ALL.iter().position(|&x| x == f);
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(order(a.format).cmp(&order(b.format)))
        });
        found.dedup_by(|a, b| a.format == b.format && a.uid == b.uid && a.path == b.path);
        self.installed = found;
        self.scanned = true;
    }
}

/// A value as the plug-in writes it, with its unit.
fn describe(plugin: &Plugin, info: &ParamInfo, value: f64) -> String {
    let text = plugin
        .formatter
        .as_ref()
        .and_then(|f| f.text(info.id, value))
        .unwrap_or_else(|| format!("{:.0}%", value * 100.0));
    let text = text.trim();
    if info.units.is_empty() || text.contains(&info.units) {
        text.to_owned()
    } else {
        format!("{text} {}", info.units)
    }
}

impl Drop for Effects {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_survive_hex() {
        let data = vec![0, 1, 15, 16, 255];
        assert_eq!(hex::encode(&data), "00010F10FF");
        assert_eq!(hex::decode(&hex::encode(&data)).unwrap(), data);
        assert_eq!(hex::decode("deadBEEF").unwrap(), [0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(hex::decode("ABC"), None);
        assert_eq!(hex::decode("ZZ"), None);
    }

    /// A chain exported on Windows, shortened.
    #[test]
    fn chain_files_are_read() {
        let text = r#"{
          "version": 1,
          "chain": [
            {"format": "Vst2", "path": "F:\\VST\\ReaPlugs\\reaeq-standalone.dll",
             "uid": "72656571", "name": "ReaEQ (ReaPlugs Edition)", "bypassed": false,
             "params": [[0, 0.14], [1, 0.0]], "state": "0A0B"},
            {"path": "F:\\VST\\rnnoise.vst3", "uid": "01EFCDAB8291EBFA5765726D526E6E31",
             "name": "RNNoise suppression for voice", "bypassed": true}
          ]
        }"#;
        let chain = Effects::parse(text).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].format, Format::Vst2);
        assert_eq!(chain[0].params, [(0, 0.14), (1, 0.0)]);
        assert_eq!(chain[1].format, Format::Vst3, "old files have only VST3");
        assert!(chain[1].bypassed);
        assert!(Effects::parse(r#"{"version": 9, "chain": []}"#).is_err());
    }

    /// Every effect installed on this computer: each loads, processes a
    /// tone and saves its state. `cargo test -- --ignored installed`.
    #[test]
    #[ignore = "depends on the plug-ins installed here"]
    fn installed_plugins_process_audio() {
        let _turn = rack::tests::TURN.lock().unwrap_or_else(|p| p.into_inner());
        let mut effects = Effects::new();
        effects.scan(true);
        let installed = effects.installed().to_vec();
        assert!(!installed.is_empty());
        let mut report = Vec::new();
        for descriptor in &installed {
            effects.load(&[]);
            effects.set_enabled(true);
            let index = effects.add(descriptor);
            let label = format!("{} ({})", descriptor.name, descriptor.format.label());
            if let Some(error) = &effects.slots()[index].error {
                report.push(format!("{label}: {error}"));
                continue;
            }
            let params = effects.params(index);
            let mut peak = 0.0f32;
            for block in 0..50 {
                let mut samples: Vec<f32> = (0..256)
                    .map(|i| {
                        let t = (block * 256 + i) as f32 / 48_000.0;
                        0.3 * (t * 2.0 * std::f32::consts::PI * 440.0).sin()
                    })
                    .collect();
                assert!(
                    rack::process(&mut samples, 48_000.0),
                    "{label}: did not run"
                );
                assert!(samples.iter().all(|s| s.is_finite()), "{label}");
                peak = samples.iter().fold(peak, |p, s| p.max(s.abs()));
            }
            let saved = effects.entries();
            effects.load(&saved);
            assert!(
                effects.slots()[0].error.is_none(),
                "{label}: did not reload"
            );
            report.push(format!(
                "{label}: {} parameters, peak {peak:.3}, state {} bytes",
                params.len(),
                saved[0].state.len() / 2
            ));
        }
        effects.set_enabled(false);
        effects.clear();
        println!("{}", report.join("\n"));
    }

    #[test]
    fn chains_from_elsewhere_find_the_plugins_installed_here() {
        let mut effects = Effects::new();
        let here = |format, name: &str, uid: &str| Descriptor {
            format,
            path: std::env::temp_dir(),
            uid: uid.into(),
            name: name.into(),
            vendor: String::new(),
            is_instrument: false,
        };
        effects.installed = vec![
            here(Format::Vst3, "BUSTERse", "F2AEE70D"),
            here(Format::Ladspa, "Amplifier", "4242"),
        ];
        effects.scanned = true;
        let saved = |format, name: &str| Entry {
            format,
            path: PathBuf::from(r"F:\VST\x"),
            uid: "0DE7AEF2".into(),
            name: name.into(),
            bypassed: false,
            params: vec![(1, 0.5)],
            state: "00".into(),
        };
        let mut chain = vec![
            saved(Format::Vst3, "busterse"),
            saved(Format::Vst2, "Amplifier"),
            saved(Format::Vst3, "ReaEQ"),
        ];
        effects.resolve(&mut chain);
        assert_eq!(chain[0].uid, "F2AEE70D");
        assert_eq!(chain[0].path, std::env::temp_dir());
        assert_eq!(chain[0].params, [(1, 0.5)], "same format, same parameters");
        assert_eq!(chain[1].format, Format::Ladspa);
        assert!(chain[1].params.is_empty() && chain[1].state.is_empty());
        assert_eq!(chain[2].path, PathBuf::from(r"F:\VST\x"), "nothing to find");
    }

    #[test]
    fn a_missing_plugin_stays_in_the_chain() {
        let _turn = rack::tests::TURN.lock().unwrap_or_else(|p| p.into_inner());
        let mut effects = Effects::new();
        let entry = Entry {
            format: Format::Vst3,
            path: PathBuf::from("/nowhere/Gone.vst3"),
            uid: "00".repeat(16),
            name: "Gone".into(),
            bypassed: false,
            params: vec![(1, 0.5)],
            state: "0102".into(),
        };
        effects.load(std::slice::from_ref(&entry));
        assert!(effects.slots()[0].error.is_some());
        assert!(!effects.active());
        assert_eq!(effects.entries(), [entry], "saved as it was");
    }
}
