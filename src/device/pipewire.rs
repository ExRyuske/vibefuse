//! The software mixer on Linux: PipeWire mixes.
//!
//! A second PipeWire instance of the user (`pipewire -c`, run by the systemd
//! user service `vibefuse-mixer.service`) publishes the virtual devices and
//! a filter-chain with the gain matrix of Arturia's Windows driver:
//!
//! ```text
//! VibeFuse Main 1/2, Virtual 3/4–7/8 ─┐                   ┌─ VibeFuse Mix 3/4, Mix 5/6,
//!      (loopback sinks)              ├─ vibefuse.mixer ──┤     Loopback Main 7/8 (sources)
//! card input 1 ─ vibefuse.input ─────┘  (filter-chain)   └─ vibefuse.monitor ─ card outputs
//! ```
//!
//! Inside the filter-chain every source is copied to eight builtin `mixer`
//! nodes, one per mono output; their "Gain n" controls are the crosspoints
//! and are changed with `pw-cli set-param`. Everything runs inside PipeWire,
//! so no process of this app touches audio. All internal links are passive:
//! with no program using a virtual device the graph sleeps.
//!
//! WirePlumber would connect the two streams that face the card to some
//! other device while the card is away, so they do not connect by
//! themselves; [`keep_links`] links them to the card whenever it appears.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Error, Result, SoftMixer};
use crate::virtual_devices::State;

pub const SERVICE: &str = "vibefuse-mixer.service";
/// First line of the config; another one means an older version.
const CONFIG_TAG: &str = "# vibefuse-mixer 1";
const MIXER_NODE: &str = "vibefuse.mixer";
const INPUT_NODE: &str = "vibefuse.input";
const MONITOR_NODE: &str = "vibefuse.monitor";
/// Controls per `pw-cli set-param`.
const CONTROLS_PER_CALL: usize = 16;
/// Gains are written into the config at most this often while they move.
const PERSIST_PERIOD: Duration = Duration::from_secs(2);

const SOURCES: usize = 10;
const OUTPUTS: usize = 8;
const UNITY: f64 = 16_777_216.0;

/// Mono mixer inputs as the Windows driver numbers them, see `sls.rs`; the
/// same layout as the macOS plug-in.
#[rustfmt::skip]
const SOURCE_ADDRESS: [(u8, u8); SOURCES] = [
    (0, 0), (0, 1), // Main 1/2
    (1, 0), (1, 1), // card inputs
    (1, 2), (1, 3), // Virtual 3/4
    (2, 0), (2, 1), // Virtual 5/6
    (2, 2), (2, 3), // Virtual 7/8
];
#[rustfmt::skip]
const OUTPUT_ADDRESS: [(u8, u8); OUTPUTS] = [
    (1, 0), (1, 1), // Monitor & Phones
    (0, 2), (0, 3), // Mix 3/4
    (2, 0), (2, 1), // Mix 5/6
    (2, 2), (2, 3), // Loopback Main 7/8
];

fn config_path() -> Option<PathBuf> {
    Some(crate::settings::app_dir()?.join("pipewire-mixer.conf"))
}

fn unit_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("systemd/user").join(SERVICE))
}

/// The gain matrix, linear.
#[derive(Debug, Clone, PartialEq)]
pub struct Matrix {
    crosspoints: [[f64; SOURCES]; OUTPUTS],
    outputs: [f64; OUTPUTS],
}

impl Default for Matrix {
    /// The factory preset, like the plug-in on macOS: Main 1/2 on Monitor
    /// & Phones and Loopback Main 7/8, Virtual 3/4 on Mix 3/4, Virtual 5/6
    /// on Mix 5/6.
    fn default() -> Self {
        let mut crosspoints = [[0.0; SOURCES]; OUTPUTS];
        for (source, output) in [(0, 0), (0, 6), (4, 2), (6, 4)] {
            crosspoints[output][source] = 1.0;
            crosspoints[output + 1][source + 1] = 1.0;
        }
        Self {
            crosspoints,
            outputs: [1.0; OUTPUTS],
        }
    }
}

impl Matrix {
    /// Applies a property in the Windows driver's format; returns the
    /// outputs whose controls changed.
    fn apply(&mut self, property: &[u8]) -> Result<Vec<usize>> {
        let word = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(
                property.get(at..at + 4)?.try_into().ok()?,
            ))
        };
        let gain = |bytes: &[u8]| {
            f64::from(i32::from_le_bytes(bytes.try_into().unwrap_or_default())) / UNITY
        };
        let source = |kind, index| SOURCE_ADDRESS.iter().position(|&a| a == (kind, index));
        let output = |kind, index| OUTPUT_ADDRESS.iter().position(|&a| a == (kind, index));
        if word(0) != Some(0x4D) {
            return Err(Error::new("unknown mixer property"));
        }
        let mut changed = Vec::new();
        match word(4) {
            Some(200) => {
                for entry in property[8..].as_chunks::<8>().0 {
                    if let (Some(s), Some(o)) =
                        (source(entry[0], entry[1]), output(entry[2], entry[3]))
                    {
                        self.crosspoints[o][s] = gain(&entry[4..8]).clamp(0.0, 4.0);
                        changed.push(o);
                    }
                }
            }
            Some(201) => {
                for entry in property[8..].as_chunks::<6>().0 {
                    if let Some(o) = output(entry[0], entry[1]) {
                        self.outputs[o] = gain(&entry[2..6]).clamp(0.0, 4.0);
                        changed.push(o);
                    }
                }
            }
            // Meters are always "on": there are none on Linux.
            Some(300) => {}
            _ => return Err(Error::new("unknown mixer property")),
        }
        changed.sort_unstable();
        changed.dedup();
        Ok(changed)
    }

    /// Control values of one output: `a{o}` mixes sources 1–8, `b{o}` adds
    /// sources 9–10 and applies the output gain to everything.
    fn controls(&self, o: usize) -> Vec<(String, f64)> {
        let row = &self.crosspoints[o];
        let out = self.outputs[o];
        let mut controls: Vec<(String, f64)> = (0..8)
            .map(|s| (format!("a{o}:Gain {}", s + 1), row[s]))
            .collect();
        controls.push((format!("b{o}:Gain 1"), out));
        controls.push((format!("b{o}:Gain 2"), row[8] * out));
        controls.push((format!("b{o}:Gain 3"), row[9] * out));
        controls
    }
}

fn number(value: f64) -> String {
    format!("{value:.6}")
}

/// The configuration of the mixer's PipeWire instance.
pub fn config(matrix: &Matrix) -> String {
    let mut nodes = Vec::new();
    let mut links = Vec::new();
    for s in 0..SOURCES {
        nodes.push(format!("{{ type = builtin name = c{s} label = copy }}"));
    }
    for o in 0..OUTPUTS {
        let controls: BTreeMap<String, f64> = matrix.controls(o).into_iter().collect();
        let control = |node: &str| {
            controls
                .iter()
                .filter(|(name, _)| name.starts_with(node))
                .map(|(name, value)| format!("\"{}\" = {}", &name[node.len()..], number(*value)))
                .collect::<Vec<_>>()
                .join(" ")
        };
        nodes.push(format!(
            "{{ type = builtin name = a{o} label = mixer control = {{ {} }} }}",
            control(&format!("a{o}:"))
        ));
        nodes.push(format!(
            "{{ type = builtin name = b{o} label = mixer control = {{ {} }} }}",
            control(&format!("b{o}:"))
        ));
        for s in 0..8 {
            links.push(format!(
                "{{ output = \"c{s}:Out\" input = \"a{o}:In {}\" }}",
                s + 1
            ));
        }
        links.push(format!("{{ output = \"a{o}:Out\" input = \"b{o}:In 1\" }}"));
        links.push(format!("{{ output = \"c8:Out\" input = \"b{o}:In 2\" }}"));
        links.push(format!("{{ output = \"c9:Out\" input = \"b{o}:In 3\" }}"));
    }
    let aux = |n: usize| {
        (0..n)
            .map(|i| format!("AUX{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let inputs: Vec<String> = (0..SOURCES).map(|s| format!("\"c{s}:In\"")).collect();
    let outputs: Vec<String> = (0..OUTPUTS).map(|o| format!("\"b{o}:Out\"")).collect();
    let loopback = |description: &str, capture: String, playback: String| {
        format!(
            "  {{ name = libpipewire-module-loopback args = {{\n      node.description = \"{description}\"\n      \
             capture.props = {{ {capture} }}\n      playback.props = {{ {playback} }} }} }}"
        )
    };
    let mut modules = vec![format!(
        "  {{ name = libpipewire-module-filter-chain args = {{\n      \
         node.description = \"VibeFuse Mixer\"\n      media.name = \"VibeFuse Mixer\"\n      \
         filter.graph = {{\n        nodes = [ {} ]\n        links = [ {} ]\n        \
         inputs = [ {} ]\n        outputs = [ {} ]\n      }}\n      \
         capture.props = {{ node.name = \"{MIXER_NODE}\" media.class = Audio/Sink/Internal \
         audio.channels = {SOURCES} audio.position = [ {} ] }}\n      \
         playback.props = {{ node.name = \"{MIXER_NODE}.out\" media.class = Audio/Source/Internal \
         audio.channels = {OUTPUTS} audio.position = [ {} ] }} }} }}",
        nodes.join(" "),
        links.join(" "),
        inputs.join(" "),
        outputs.join(" "),
        aux(SOURCES),
        aux(OUTPUTS),
    )];
    // Virtual outputs: sinks that feed their pair of mixer inputs.
    for (name, title, first) in [
        ("main12", "Main 1/2", 0),
        ("virtual34", "Virtual 3/4", 4),
        ("virtual56", "Virtual 5/6", 6),
        ("virtual78", "Virtual 7/8", 8),
    ] {
        modules.push(loopback(
            &format!("VibeFuse {title}"),
            format!(
                "node.name = \"vibefuse.{name}\" node.description = \"VibeFuse {title}\" \
                 media.class = Audio/Sink audio.position = [ FL FR ]"
            ),
            format!(
                "node.name = \"vibefuse.{name}.out\" node.passive = true \
                 target.object = \"{MIXER_NODE}\" audio.position = [ AUX{first} AUX{} ] \
                 stream.dont-remix = true",
                first + 1
            ),
        ));
    }
    // The card's input 1, linked by `keep_links`.
    modules.push(loopback(
        "VibeFuse Input 1",
        format!(
            "node.name = \"{INPUT_NODE}\" node.passive = true node.autoconnect = false \
             audio.position = [ FL ] stream.dont-remix = true"
        ),
        format!(
            "node.name = \"{INPUT_NODE}.out\" node.passive = true \
             target.object = \"{MIXER_NODE}\" audio.position = [ AUX2 ] stream.dont-remix = true"
        ),
    ));
    // The mixes: the card's outputs (linked by `keep_links`) and sources.
    for (name, title, first) in [
        ("monitor", "Monitor & Phones", 0),
        ("mix34", "Mix 3/4", 2),
        ("mix56", "Mix 5/6", 4),
        ("loopback78", "Loopback Main 7/8", 6),
    ] {
        let capture = format!(
            "node.name = \"vibefuse.{name}.in\" node.passive = true \
             target.object = \"{MIXER_NODE}.out\" audio.position = [ AUX{first} AUX{} ] \
             stream.dont-remix = true",
            first + 1
        );
        let playback = if name == "monitor" {
            format!(
                "node.name = \"{MONITOR_NODE}\" node.passive = true node.autoconnect = false \
                 audio.position = [ FL FR ] stream.dont-remix = true"
            )
        } else {
            format!(
                "node.name = \"vibefuse.{name}\" node.description = \"VibeFuse {title}\" \
                 media.class = Audio/Source audio.position = [ FL FR ]"
            )
        };
        modules.push(loopback(&format!("VibeFuse {title}"), capture, playback));
    }
    format!(
        "{CONFIG_TAG}\n\
         # Written by VibeFuse: the virtual devices of the MiniFuse 1 mixer.\n\
         context.properties = {{ log.level = 1 }}\n\
         context.spa-libs = {{\n  audio.convert.* = audioconvert/libspa-audioconvert\n  \
         support.* = support/libspa-support\n}}\n\
         context.modules = [\n  \
         {{ name = libpipewire-module-rt args = {{ nice.level = -11 }} flags = [ ifexists nofail ] }}\n  \
         {{ name = libpipewire-module-protocol-native }}\n  \
         {{ name = libpipewire-module-client-node }}\n  \
         {{ name = libpipewire-module-adapter }}\n{}\n]\n",
        modules.join("\n")
    )
}

fn tool(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|path| path.is_file())
}

fn run(program: &str, args: &[&str]) -> std::result::Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Every object of the user's PipeWire.
fn dump() -> Option<Vec<Value>> {
    let text = run("pw-dump", &[]).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .as_array()
        .cloned()
}

fn props(object: &Value) -> Option<&serde_json::Map<String, Value>> {
    object.pointer("/info/props")?.as_object()
}

fn prop<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    props(object)?.get(key)?.as_str()
}

fn is_type(object: &Value, kind: &str) -> bool {
    object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.ends_with(kind))
}

fn node_id(objects: &[Value], name: &str) -> Option<u64> {
    objects
        .iter()
        .find(|o| is_type(o, "Node") && prop(o, "node.name") == Some(name))?
        .get("id")?
        .as_u64()
}

/// Ports of a node in one direction ("in"/"out"), in channel order.
fn ports(objects: &[Value], node: u64, direction: &str) -> Vec<u64> {
    let mut ports: Vec<(u64, u64)> = objects
        .iter()
        .filter(|o| is_type(o, "Port"))
        .filter(|o| {
            props(o)
                .and_then(|p| p.get("node.id"))
                .and_then(Value::as_u64)
                == Some(node)
        })
        .filter(|o| prop(o, "port.direction") == Some(direction))
        .filter(|o| {
            // pw-dump writes `true` unquoted.
            let monitor = props(o).and_then(|p| p.get("port.monitor"));
            !matches!(monitor, Some(Value::Bool(true)))
                && monitor.and_then(Value::as_str) != Some("true")
        })
        .filter_map(|o| {
            let order = props(o)?.get("port.id")?.as_u64()?;
            Some((order, o.get("id")?.as_u64()?))
        })
        .collect();
    ports.sort_unstable();
    ports.into_iter().map(|(_, id)| id).collect()
}

/// The card's own node of a class (not our virtual devices).
fn card_node(objects: &[Value], class: &str) -> Option<u64> {
    objects
        .iter()
        .find(|o| {
            is_type(o, "Node")
                && prop(o, "media.class") == Some(class)
                && prop(o, "node.name").is_some_and(|name| {
                    name.starts_with("alsa_") && name.to_lowercase().contains("minifuse")
                })
        })?
        .get("id")?
        .as_u64()
}

/// Links the mixer to the card: its first input to "Input 1", "Monitor &
/// Phones" to its first two outputs. Missing links only.
pub fn link_card() {
    let Some(objects) = dump() else { return };
    let mut wanted = Vec::new();
    if let (Some(card), Some(input)) = (
        card_node(&objects, "Audio/Source"),
        node_id(&objects, INPUT_NODE),
    ) {
        wanted.extend(
            ports(&objects, card, "out")
                .into_iter()
                .zip(ports(&objects, input, "in"))
                .take(1),
        );
    }
    if let (Some(card), Some(monitor)) = (
        card_node(&objects, "Audio/Sink"),
        node_id(&objects, MONITOR_NODE),
    ) {
        wanted.extend(
            ports(&objects, monitor, "out")
                .into_iter()
                .zip(ports(&objects, card, "in"))
                .take(2),
        );
    }
    let linked = |from: u64, to: u64| {
        objects.iter().any(|o| {
            is_type(o, "Link")
                && o.pointer("/info/output-port-id").and_then(Value::as_u64) == Some(from)
                && o.pointer("/info/input-port-id").and_then(Value::as_u64) == Some(to)
        })
    };
    for (from, to) in wanted {
        if !linked(from, to) {
            let _ = run("pw-link", &[&from.to_string(), &to.to_string()]);
        }
    }
}

/// Keeps the card linked: watches ports come and go (`pw-link -m`) and
/// links again when the card's appear. Runs until PipeWire goes away for
/// good; the caller restarts it.
pub fn keep_links() {
    use std::io::{BufRead, BufReader};
    loop {
        link_card();
        let Ok(mut child) = Command::new("pw-link")
            .args(["-m", "-o", "-i"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            std::thread::sleep(Duration::from_secs(30));
            continue;
        };
        if let Some(stdout) = child.stdout.take() {
            let mut pending: Option<Instant> = None;
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                let lower = line.to_lowercase();
                // The card's ports or those of the mixer's own nodes.
                let relevant = line.starts_with('+')
                    && (lower.contains("minifuse") || lower.contains("vibefuse"));
                if relevant && pending.is_none_or(|at| at.elapsed() > Duration::from_millis(500)) {
                    // Let the rest of the ports appear first.
                    std::thread::sleep(Duration::from_millis(300));
                    link_card();
                    pending = Some(Instant::now());
                }
            }
        }
        let _ = child.wait();
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn service_active() -> bool {
    run("systemctl", &["--user", "is-active", "--quiet", SERVICE]).is_ok()
}

fn wait_for_mixer() -> Option<u64> {
    let start = Instant::now();
    loop {
        if let Some(id) = dump().and_then(|objects| node_id(&objects, MIXER_NODE)) {
            return Some(id);
        }
        if start.elapsed() > Duration::from_secs(8) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn state() -> State {
    let missing: Vec<&str> = ["pipewire", "pw-cli", "pw-link", "pw-dump", "systemctl"]
        .into_iter()
        .filter(|name| tool(name).is_none())
        .collect();
    if !missing.is_empty() {
        return State::Unavailable(crate::tr!(
            "Needs PipeWire and systemd. Missing: {}.",
            missing.join(", ")
        ));
    }
    let Some(config) = config_path() else {
        return State::Unavailable("no config folder".to_owned());
    };
    if !unit_path().is_some_and(|unit| unit.exists()) {
        return State::Missing;
    }
    let current =
        std::fs::read_to_string(&config).is_ok_and(|text| text.lines().next() == Some(CONFIG_TAG));
    if !current {
        State::Outdated
    } else if dump().is_some_and(|objects| node_id(&objects, MIXER_NODE).is_some()) {
        State::Ready
    } else {
        State::NotLoaded
    }
}

pub fn install() -> std::result::Result<(), String> {
    if let State::Unavailable(reason) = state() {
        return Err(reason);
    }
    let pipewire = tool("pipewire").ok_or("pipewire not found")?;
    let config = config_path().ok_or("no config folder")?;
    let unit = unit_path().ok_or("no config folder")?;
    for path in [&config, &unit] {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(&config, self::config(&Matrix::default())).map_err(|e| e.to_string())?;
    let service = format!(
        "[Unit]\nDescription=VibeFuse: virtual devices of the MiniFuse 1 mixer\n\
         After=pipewire.service wireplumber.service\nPartOf=pipewire.service\n\n\
         [Service]\nExecStart=\"{}\" -c \"{}\"\nRestart=always\nRestartSec=2\n\n\
         [Install]\nWantedBy=pipewire.service\n",
        pipewire.display(),
        config.display()
    );
    std::fs::write(&unit, service).map_err(|e| e.to_string())?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", SERVICE])?;
    run("systemctl", &["--user", "restart", SERVICE])?;
    wait_for_mixer().ok_or_else(|| crate::tr!("The PipeWire mixer did not start"))?;
    link_card();
    Ok(())
}

pub fn remove() -> std::result::Result<(), String> {
    let _ = run("systemctl", &["--user", "disable", "--now", SERVICE]);
    for path in [unit_path(), config_path()].into_iter().flatten() {
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

/// The running mixer, if the service is up.
pub fn find() -> Option<PipeWireMixer> {
    if !service_active() {
        return None;
    }
    let node = dump().and_then(|objects| node_id(&objects, MIXER_NODE))?;
    Some(PipeWireMixer {
        node,
        matrix: Matrix::default(),
        persisted: None,
        dirty: false,
    })
}

pub struct PipeWireMixer {
    node: u64,
    matrix: Matrix,
    persisted: Option<Instant>,
    dirty: bool,
}

impl PipeWireMixer {
    fn send(&self, controls: &[(String, f64)]) -> std::result::Result<(), String> {
        // pw-cli quietly drops a value that does not fit its buffer (about
        // 30 controls), so they go in portions.
        for portion in controls.chunks(CONTROLS_PER_CALL) {
            let params: Vec<String> = portion
                .iter()
                .map(|(name, value)| format!("\"{name}\" {}", number(*value)))
                .collect();
            let pod = format!("{{ params = [ {} ] }}", params.join(" "));
            let echo = run(
                "pw-cli",
                &["set-param", &self.node.to_string(), "Props", &pod],
            )?;
            // On success it prints the parameter it set.
            if !echo.contains("Props") {
                return Err(format!("pw-cli: {}", echo.trim()));
            }
        }
        Ok(())
    }

    /// Keeps the gains for the next start of the service.
    fn persist(&mut self, now: bool) {
        if !self.dirty {
            return;
        }
        if !now
            && self
                .persisted
                .is_some_and(|at| at.elapsed() < PERSIST_PERIOD)
        {
            return;
        }
        if let Some(path) = config_path() {
            let _ = std::fs::write(path, config(&self.matrix));
        }
        self.persisted = Some(Instant::now());
        self.dirty = false;
    }
}

impl SoftMixer for PipeWireMixer {
    fn set(&mut self, property: &[u8]) -> Result<()> {
        let changed = self.matrix.apply(property)?;
        if changed.is_empty() {
            return Ok(());
        }
        let controls: Vec<(String, f64)> = changed
            .iter()
            .flat_map(|&o| self.matrix.controls(o))
            .collect();
        if self.send(&controls).is_err() {
            // The service may have restarted with another node id.
            let node = dump()
                .and_then(|objects| node_id(&objects, MIXER_NODE))
                .ok_or_else(|| Error::gone("the PipeWire mixer is not responding"))?;
            self.node = node;
            self.send(&controls).map_err(Error::new)?;
        }
        self.dirty = true;
        self.persist(false);
        Ok(())
    }

    fn get(&mut self, _property: &mut [u8]) -> Result<()> {
        Err(Error::new("no mixer meters on Linux"))
    }
}

impl Drop for PipeWireMixer {
    fn drop(&mut self) {
        self.persist(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::sls::{self, Sls, SlsControl};

    #[test]
    fn matrix_follows_the_driver_properties() {
        let mut matrix = Matrix::default();
        let mut sls = Sls::default();
        for property in sls.all_gains().properties() {
            matrix.apply(&property).unwrap();
        }
        // The factory mix of the app and of the matrix agree.
        let factory = Matrix::default();
        for o in 0..OUTPUTS {
            for s in 0..SOURCES {
                assert!(
                    (matrix.crosspoints[o][s] - factory.crosspoints[o][s]).abs() < 1e-6,
                    "{o} {s}"
                );
            }
        }
        // Virtual 7/8 at -6 dB on Monitor & Phones: sources 9 and 10 sit
        // in the second mixer of outputs 0 and 1.
        let gains = sls.apply(SlsControl::Mute {
            mix: 0,
            source: 4,
            muted: false,
        });
        let mut changed = Vec::new();
        for property in gains.properties() {
            changed.extend(matrix.apply(&property).unwrap());
        }
        changed.sort_unstable();
        changed.dedup();
        assert_eq!(changed, vec![0, 1, 6, 7]);
        let controls: BTreeMap<String, f64> = matrix.controls(0).into_iter().collect();
        assert!((controls["b0:Gain 2"] - 1.0).abs() < 1e-6);
        assert!((controls["a0:Gain 1"] - 1.0).abs() < 1e-6, "Main 1/2 left");
        let gains = sls.apply(SlsControl::Output { mix: 0, db: -6.0 });
        for property in gains.properties() {
            matrix.apply(&property).unwrap();
        }
        let controls: BTreeMap<String, f64> = matrix.controls(0).into_iter().collect();
        let half = 10f64.powf(-6.0 / 20.0);
        assert!((controls["b0:Gain 1"] - half).abs() < 1e-4);
        assert!((controls["b0:Gain 2"] - half).abs() < 1e-4);
        assert!(
            (controls["a0:Gain 1"] - 1.0).abs() < 1e-6,
            "applied once, in b0"
        );
        assert!(matrix.apply(&sls::meters_switch(true)).unwrap().is_empty());
    }

    /// Peak per channel of a WAV file pw-record wrote (16-bit or float),
    /// skipping the first third.
    fn peaks(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        let channels = usize::from(u16::from_le_bytes([bytes[22], bytes[23]]));
        let bits = u16::from_le_bytes([bytes[34], bytes[35]]);
        let format = u16::from_le_bytes([bytes[20], bytes[21]]);
        let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
        let samples: Vec<f32> = match (format, bits) {
            (3, 32) => bytes[data..]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| f32::from_le_bytes(b))
                .collect(),
            (_, 16) => bytes[data..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| f32::from(i16::from_le_bytes(b)) / 32768.0)
                .collect(),
            other => panic!("format {other:?}"),
        };
        let frames = samples.len() / channels;
        (0..channels)
            .map(|c| {
                (frames / 3..frames)
                    .map(|i| samples[i * channels + c].abs())
                    .fold(0.0, f32::max)
            })
            .collect()
    }

    fn tone(path: &str, left: f32, right: f32) {
        let frames: u32 = 48_000 * 4;
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend((36 + frames * 4).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(48_000u32.to_le_bytes());
        wav.extend((48_000u32 * 4).to_le_bytes());
        wav.extend(4u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((frames * 4).to_le_bytes());
        for i in 0..frames {
            let x = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
            wav.extend(((x * left * 32767.0) as i16).to_le_bytes());
            wav.extend(((x * right * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(path, wav).unwrap();
    }

    /// Plays a tone into one node while recording another.
    fn through(play: &str, record: &[&str], left: f32, right: f32) -> Vec<f32> {
        tone("/tmp/vibefuse-tone.wav", left, right);
        let mut player =
            quiet(Command::new("pw-play").args(["--target", play, "/tmp/vibefuse-tone.wav"]));
        std::thread::sleep(Duration::from_millis(800));
        let _ = quiet(
            Command::new("timeout")
                .args(["1.5", "pw-record"])
                .args(record)
                .arg("/tmp/vibefuse-rec.wav"),
        )
        .wait();
        let _ = player.kill();
        let _ = player.wait();
        peaks("/tmp/vibefuse-rec.wav")
    }

    /// Child processes that end with the test, also when it fails.
    struct Children(Vec<std::process::Child>);

    impl Drop for Children {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn quiet(command: &mut Command) -> std::process::Child {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Runs the generated graph in the PipeWire of this session, with a
    /// fake card. `cargo test -- --ignored`, in a Linux session with
    /// PipeWire and WirePlumber.
    #[test]
    #[ignore]
    fn live_pipewire() {
        let card_out = "alsa_output.usb-ARTURIA_MiniFuse_1_TEST-00.analog-stereo";
        let card_in = "alsa_input.usb-ARTURIA_MiniFuse_1_TEST-00.analog-stereo";
        run("pw-cli", &["create-node", "adapter", &format!(
            "{{ factory.name=support.null-audio-sink node.name={card_out} media.class=Audio/Sink audio.position=[FL FR] object.linger=true }}"
        )]).unwrap();
        let mut children = Children(Vec::new());
        children.0.push(quiet(
            Command::new("pw-loopback")
                .arg("--capture-props=media.class=Audio/Sink node.name=fake.mic audio.position=[FL FR]")
                .arg(format!("--playback-props=media.class=Audio/Source node.name={card_in} audio.position=[FL FR]")),
        ));
        std::fs::write("/tmp/vibefuse-mixer.conf", config(&Matrix::default())).unwrap();
        children.0.push(quiet(
            Command::new("pipewire").args(["-c", "/tmp/vibefuse-mixer.conf"]),
        ));
        let node = wait_for_mixer().expect("the mixer node");
        let mut mixer = PipeWireMixer {
            node,
            matrix: Matrix::default(),
            persisted: None,
            dirty: false,
        };

        // Factory mix: Virtual 5/6 on Mix 5/6 at 0 dB.
        let peak = through(
            "vibefuse.virtual56",
            &["--target", "vibefuse.mix56"],
            0.5,
            0.25,
        );
        assert!(
            (peak[0] - 0.5).abs() < 0.02 && (peak[1] - 0.25).abs() < 0.02,
            "{peak:?}"
        );

        // The same gains the app sends to the Windows driver.
        let mut sls = Sls::default();
        for property in sls
            .apply(SlsControl::Level {
                mix: 2,
                source: 3,
                db: -6.0,
            })
            .properties()
        {
            mixer.set(&property).unwrap();
        }
        let peak = through(
            "vibefuse.virtual56",
            &["--target", "vibefuse.mix56"],
            0.5,
            0.25,
        );
        assert!((peak[0] - 0.25).abs() < 0.02, "{peak:?}");

        // Input 1 (the card's first input) centred on Mix 3/4, once linked.
        let gains = sls.apply(SlsControl::Mute {
            mix: 1,
            source: 0,
            muted: false,
        });
        for property in gains.properties() {
            mixer.set(&property).unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
        link_card();
        let peak = through("fake.mic", &["--target", "vibefuse.mix34"], 0.5, 0.0);
        assert!(
            (peak[0] - 0.354).abs() < 0.02 && (peak[1] - 0.354).abs() < 0.02,
            "{peak:?}"
        );

        // Virtual 7/8 on the card's outputs (Monitor & Phones).
        for property in sls
            .apply(SlsControl::Mute {
                mix: 0,
                source: 4,
                muted: false,
            })
            .properties()
        {
            mixer.set(&property).unwrap();
        }
        // Monitor & Phones is mirrored on Loopback Main 7/8.
        let peak = through(
            "vibefuse.virtual78",
            &["--target", "vibefuse.loopback78"],
            0.5,
            0.25,
        );
        assert!(
            (peak[0] - 0.5).abs() < 0.02 && (peak[1] - 0.25).abs() < 0.02,
            "{peak:?}"
        );
        let peak = through(
            "vibefuse.virtual78",
            &["-P", "{ stream.capture.sink = true }", "--target", card_out],
            0.5,
            0.25,
        );
        assert!(
            (peak[0] - 0.5).abs() < 0.02 && (peak[1] - 0.25).abs() < 0.02,
            "{peak:?}"
        );

        // The card is unplugged and comes back: the keeper links it again.
        std::thread::spawn(keep_links);
        std::thread::sleep(Duration::from_millis(500));
        let card = dump()
            .and_then(|objects| node_id(&objects, card_out))
            .unwrap();
        run("pw-cli", &["destroy", &card.to_string()]).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        run("pw-cli", &["create-node", "adapter", &format!(
            "{{ factory.name=support.null-audio-sink node.name={card_out} media.class=Audio/Sink audio.position=[FL FR] object.linger=true }}"
        )]).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let peak = through(
            "vibefuse.virtual78",
            &["-P", "{ stream.capture.sink = true }", "--target", card_out],
            0.5,
            0.25,
        );
        assert!(
            (peak[0] - 0.5).abs() < 0.02 && (peak[1] - 0.25).abs() < 0.02,
            "{peak:?}"
        );
    }

    #[test]
    fn config_describes_the_whole_graph() {
        let text = config(&Matrix::default());
        assert!(text.starts_with(CONFIG_TAG));
        for name in [
            "vibefuse.main12",
            "vibefuse.virtual78",
            "vibefuse.mix56",
            "vibefuse.loopback78",
        ] {
            assert!(text.contains(&format!("node.name = \"{name}\"")), "{name}");
        }
        assert_eq!(text.matches("label = mixer").count(), 16);
        assert_eq!(text.matches("libpipewire-module-loopback").count(), 9);
        assert!(
            text.contains("\"Gain 7\" = 1.000000"),
            "Virtual 5/6 on Mix 5/6"
        );
        assert_eq!(text.matches('{').count(), text.matches('}').count());
        assert_eq!(text.matches('[').count(), text.matches(']').count());
    }
}
