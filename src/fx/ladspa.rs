//! LADSPA, the plainest format: one exported function hands out the
//! plug-ins of a library by index, each a table of functions and ports. No
//! host callback, no window of its own: the parameters are control ports,
//! whose values the host keeps.

use std::ffi::{CStr, c_char, c_ulong};
use std::path::Path;

use super::graph::{
    AudioBuffers, AudioProcessor, Loaded, ParamBridge, ParamChange, ParamFormatter, ParamInfo,
    PluginFormat,
};
use super::{Descriptor, Format, Result};
use crate::tr;

/// `ladspa.h`, unchanged since 2000.
mod sys {
    use std::ffi::{c_char, c_int, c_ulong, c_void};

    pub type Handle = *mut c_void;

    pub const PORT_INPUT: c_int = 0x1;
    pub const PORT_AUDIO: c_int = 0x8;

    pub const HINT_BOUNDED_BELOW: c_int = 0x1;
    pub const HINT_BOUNDED_ABOVE: c_int = 0x2;
    pub const HINT_TOGGLED: c_int = 0x4;
    pub const HINT_SAMPLE_RATE: c_int = 0x8;
    pub const HINT_LOGARITHMIC: c_int = 0x10;
    pub const HINT_INTEGER: c_int = 0x20;
    pub const HINT_DEFAULT_MASK: c_int = 0x3C0;
    pub const HINT_DEFAULT_MINIMUM: c_int = 0x40;
    pub const HINT_DEFAULT_LOW: c_int = 0x80;
    pub const HINT_DEFAULT_MIDDLE: c_int = 0xC0;
    pub const HINT_DEFAULT_HIGH: c_int = 0x100;
    pub const HINT_DEFAULT_MAXIMUM: c_int = 0x140;
    // 0x180 and 0x1C0 are unused in `ladspa.h`.
    pub const HINT_DEFAULT_0: c_int = 0x200;
    pub const HINT_DEFAULT_1: c_int = 0x240;
    pub const HINT_DEFAULT_100: c_int = 0x280;
    pub const HINT_DEFAULT_440: c_int = 0x2C0;

    #[repr(C)]
    pub struct RangeHint {
        pub descriptor: c_int,
        pub lower_bound: f32,
        pub upper_bound: f32,
    }

    #[repr(C)]
    pub struct Descriptor {
        pub unique_id: c_ulong,
        pub label: *const c_char,
        pub properties: c_int,
        pub name: *const c_char,
        pub maker: *const c_char,
        pub copyright: *const c_char,
        pub port_count: c_ulong,
        pub port_descriptors: *const c_int,
        pub port_names: *const *const c_char,
        pub port_range_hints: *const RangeHint,
        pub implementation_data: *mut c_void,
        pub instantiate: unsafe extern "C" fn(*const Descriptor, c_ulong) -> Handle,
        pub connect_port: unsafe extern "C" fn(Handle, c_ulong, *mut f32),
        pub activate: Option<unsafe extern "C" fn(Handle)>,
        pub run: unsafe extern "C" fn(Handle, c_ulong),
        pub run_adding: Option<unsafe extern "C" fn(Handle, c_ulong)>,
        pub set_run_adding_gain: Option<unsafe extern "C" fn(Handle, f32)>,
        pub deactivate: Option<unsafe extern "C" fn(Handle)>,
        pub cleanup: unsafe extern "C" fn(Handle),
    }

    pub type DescriptorFn = unsafe extern "C" fn(c_ulong) -> *const Descriptor;
}

/// The rate port ranges are reckoned at while scanning.
const NOMINAL_RATE: f32 = 48_000.0;

#[derive(Debug, Clone)]
struct Port {
    index: usize,
    name: String,
    input: bool,
    audio: bool,
    lower: f32,
    upper: f32,
    default: f32,
    integer: bool,
    toggled: bool,
}

impl Port {
    fn normalize(&self, value: f32) -> f64 {
        if self.toggled {
            return if value > 0.0 { 1.0 } else { 0.0 };
        }
        f64::from(((value - self.lower) / (self.upper - self.lower)).clamp(0.0, 1.0))
    }

    fn denormalize(&self, value: f64) -> f32 {
        let value = value.clamp(0.0, 1.0) as f32;
        if self.toggled {
            return if value >= 0.5 { 1.0 } else { 0.0 };
        }
        let raw = self.lower + (self.upper - self.lower) * value;
        if self.integer { raw.round() } else { raw }
    }
}

/// # Safety
/// `ptr` is null or a NUL-terminated string.
unsafe fn string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: passed on to the caller.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// The default a port's hint names, by the algorithm of the spec.
fn default_value(hint: i32, lower: f32, upper: f32) -> f32 {
    let logarithmic = hint & sys::HINT_LOGARITHMIC != 0 && lower > 0.0 && upper > 0.0;
    let between = |t: f32| {
        if logarithmic {
            (lower.ln() + t * (upper.ln() - lower.ln())).exp()
        } else {
            lower + t * (upper - lower)
        }
    };
    match hint & sys::HINT_DEFAULT_MASK {
        sys::HINT_DEFAULT_MINIMUM => lower,
        sys::HINT_DEFAULT_MAXIMUM => upper,
        sys::HINT_DEFAULT_LOW => between(0.25),
        sys::HINT_DEFAULT_MIDDLE => between(0.5),
        sys::HINT_DEFAULT_HIGH => between(0.75),
        sys::HINT_DEFAULT_0 => 0.0,
        sys::HINT_DEFAULT_1 => 1.0,
        sys::HINT_DEFAULT_100 => 100.0,
        sys::HINT_DEFAULT_440 => 440.0,
        _ => lower,
    }
}

/// # Safety
/// `descriptor` comes from a library still loaded.
unsafe fn ports(descriptor: *const sys::Descriptor, rate: f32) -> Vec<Port> {
    // SAFETY: passed on to the caller; the arrays hold `port_count` each.
    let d = unsafe { &*descriptor };
    (0..d.port_count as usize)
        .map(|index| {
            let (bits, name, hint) = unsafe {
                (
                    *d.port_descriptors.add(index),
                    string(*d.port_names.add(index)),
                    &*d.port_range_hints.add(index),
                )
            };
            let scale = if hint.descriptor & sys::HINT_SAMPLE_RATE != 0 {
                rate
            } else {
                1.0
            };
            let below = hint.descriptor & sys::HINT_BOUNDED_BELOW != 0;
            let above = hint.descriptor & sys::HINT_BOUNDED_ABOVE != 0;
            let (lower, upper) = match (below, above) {
                (true, true) => (hint.lower_bound * scale, hint.upper_bound * scale),
                (true, false) => (hint.lower_bound * scale, hint.lower_bound * scale + 1.0),
                (false, true) => (hint.upper_bound * scale - 1.0, hint.upper_bound * scale),
                (false, false) => (0.0, 1.0),
            };
            let upper = if upper <= lower { lower + 1.0 } else { upper };
            let toggled = hint.descriptor & sys::HINT_TOGGLED != 0;
            Port {
                index,
                name,
                input: bits & sys::PORT_INPUT != 0,
                audio: bits & sys::PORT_AUDIO != 0,
                lower,
                upper,
                default: if toggled {
                    0.0
                } else {
                    default_value(hint.descriptor, lower, upper).clamp(lower, upper)
                },
                integer: hint.descriptor & sys::HINT_INTEGER != 0,
                toggled,
            }
        })
        .collect()
}

struct Library {
    entry: sys::DescriptorFn,
    /// Never closed, like the other formats' libraries.
    _library: std::mem::ManuallyDrop<libloading::Library>,
}

// SAFETY: the library is only used by the instance that owns it.
unsafe impl Send for Library {}

impl Library {
    fn load(path: &Path) -> Result<Self> {
        // SAFETY: loading foreign code is what hosting plug-ins is.
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: the spec's signature; a library without it is no LADSPA.
        let entry = unsafe { library.get::<sys::DescriptorFn>(b"ladspa_descriptor\0") }
            .map(|symbol| *symbol)
            .map_err(|_| format!("{}: not a LADSPA plug-in", path.display()))?;
        Ok(Self {
            entry,
            _library: std::mem::ManuallyDrop::new(library),
        })
    }

    /// The plug-in at `index`; `None` past the last.
    fn descriptor(&self, index: c_ulong) -> Option<*const sys::Descriptor> {
        // SAFETY: the library's own entry point.
        let descriptor = unsafe { (self.entry)(index) };
        (!descriptor.is_null()).then_some(descriptor)
    }

    /// Plug-ins are known by id: the index may change between versions.
    fn find(&self, id: c_ulong) -> Option<*const sys::Descriptor> {
        (0..)
            .map_while(|index| self.descriptor(index))
            // SAFETY: a live descriptor.
            .find(|&d| unsafe { (*d).unique_id } == id)
    }
}

pub struct LadspaFormat;

impl PluginFormat for LadspaFormat {
    fn format(&self) -> Format {
        Format::Ladspa
    }

    fn probe(&self, path: &Path) -> Result<Vec<Descriptor>> {
        let library = Library::load(path)?;
        Ok((0..)
            .map_while(|index| library.descriptor(index))
            .map(|descriptor| {
                // SAFETY: a descriptor of the loaded library.
                let (d, ports) = unsafe { (&*descriptor, ports(descriptor, NOMINAL_RATE)) };
                // SAFETY: the descriptor's strings.
                let (name, maker) = unsafe { (string(d.name), string(d.maker)) };
                Descriptor {
                    format: Format::Ladspa,
                    path: path.to_path_buf(),
                    uid: d.unique_id.to_string(),
                    name: if name.is_empty() {
                        path.file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    } else {
                        name
                    },
                    vendor: maker,
                    // A generator has no audio input.
                    is_instrument: !ports.iter().any(|p| p.audio && p.input),
                }
            })
            .collect())
    }

    fn instantiate(&self, descriptor: &Descriptor, sample_rate: f64) -> Result<Loaded> {
        let fail = |reason: String| format!("{}: {reason}", descriptor.name);
        let library = Library::load(&descriptor.path)?;
        let id: c_ulong = descriptor
            .uid
            .parse()
            .map_err(|_| fail(format!("bad id {}", descriptor.uid)))?;
        let raw = library
            .find(id)
            .ok_or_else(|| fail("no longer in the file".into()))?;
        // SAFETY: a descriptor of the loaded library.
        let ports = unsafe { ports(raw, sample_rate as f32) };
        let pick = |audio: bool, input: bool| -> Vec<usize> {
            ports
                .iter()
                .filter(|p| p.audio == audio && p.input == input)
                .map(|p| p.index)
                .collect()
        };
        let (audio_in, audio_out) = (pick(true, true), pick(true, false));
        let (control_in, control_out) = (pick(false, true), pick(false, false));
        if audio_out.is_empty() {
            return Err(fail("no audio outputs".into()));
        }
        // Output controls (meters) are connected, as the spec wants, but not
        // listed.
        let parameters = control_in
            .iter()
            .map(|&index| ParamInfo {
                id: index as u32,
                title: ports[index].name.clone(),
                units: String::new(),
                read_only: false,
            })
            .collect::<Vec<_>>();
        let values = control_in
            .iter()
            .map(|&index| ports[index].normalize(ports[index].default))
            .collect();
        let (bridge, changes) = ParamBridge::for_parameters(parameters.len());
        Ok(Loaded {
            parameters,
            values,
            bridge,
            formatter: Some(Box::new(Formatter {
                ports: ports.clone(),
            })),
            editor: None,
            notifications: None,
            state: None,
            processor: Box::new(LadspaInstance {
                name: descriptor.name.clone(),
                control_values: control_in.iter().map(|&i| ports[i].default).collect(),
                meters: vec![0.0; control_out.len()],
                ports,
                audio_in,
                audio_out,
                control_in,
                control_out,
                handle: None,
                inputs: Vec::new(),
                outputs: Vec::new(),
                changes,
                max_frames: 0,
                descriptor: raw,
                _library: library,
            }),
        })
    }
}

struct LadspaInstance {
    name: String,
    ports: Vec<Port>,
    audio_in: Vec<usize>,
    audio_out: Vec<usize>,
    control_in: Vec<usize>,
    control_out: Vec<usize>,
    handle: Option<sys::Handle>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    /// The plug-in reads these through the pointers it was given: they keep
    /// their place from `activate` on.
    control_values: Vec<f32>,
    meters: Vec<f32>,
    changes: rtrb::Consumer<ParamChange>,
    max_frames: usize,
    /// Points into the library, which goes last.
    descriptor: *const sys::Descriptor,
    _library: Library,
}

// SAFETY: owned by one thread at a time; no window, no host callback.
unsafe impl Send for LadspaInstance {}

impl AudioProcessor for LadspaInstance {
    fn activate(&mut self, sample_rate: f64, max_frames: usize, _channels: usize) -> Result<()> {
        self.deactivate();
        // SAFETY: the descriptor of the loaded library.
        let d = unsafe { &*self.descriptor };
        // SAFETY: as above.
        let handle = unsafe { (d.instantiate)(self.descriptor, sample_rate.round() as c_ulong) };
        if handle.is_null() {
            return Err(format!("{}: the plug-in did not start", self.name));
        }
        self.inputs = vec![vec![0.0; max_frames]; self.audio_in.len()];
        self.outputs = vec![vec![0.0; max_frames]; self.audio_out.len()];
        // SAFETY: the buffers stay put until the next `activate` or
        // `deactivate`, as LADSPA requires.
        unsafe {
            for (buffer, &port) in self.inputs.iter_mut().zip(&self.audio_in) {
                (d.connect_port)(handle, port as c_ulong, buffer.as_mut_ptr());
            }
            for (buffer, &port) in self.outputs.iter_mut().zip(&self.audio_out) {
                (d.connect_port)(handle, port as c_ulong, buffer.as_mut_ptr());
            }
            for (value, &port) in self.control_values.iter_mut().zip(&self.control_in) {
                (d.connect_port)(handle, port as c_ulong, value);
            }
            for (value, &port) in self.meters.iter_mut().zip(&self.control_out) {
                (d.connect_port)(handle, port as c_ulong, value);
            }
            if let Some(activate) = d.activate {
                activate(handle);
            }
        }
        self.handle = Some(handle);
        self.max_frames = max_frames;
        Ok(())
    }

    fn deactivate(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // SAFETY: an instance of this descriptor, cleaned up once.
        unsafe {
            let d = &*self.descriptor;
            if let Some(deactivate) = d.deactivate {
                deactivate(handle);
            }
            (d.cleanup)(handle);
        }
    }

    fn process(&mut self, buffers: &mut AudioBuffers<'_>) {
        let Some(handle) = self.handle else { return };
        let frames = buffers.frames();
        if frames > self.max_frames || buffers.channel_count() == 0 {
            return;
        }
        while let Ok(change) = self.changes.pop() {
            if let Some(slot) = self.control_in.iter().position(|&p| p as u32 == change.id) {
                self.control_values[slot] =
                    self.ports[self.control_in[slot]].denormalize(change.value);
            }
        }
        buffers.spread_into(&mut self.inputs);
        // SAFETY: connected in `activate` to buffers of `max_frames`.
        unsafe { ((*self.descriptor).run)(handle, frames as c_ulong) };
        buffers.collect_from(&self.outputs);
    }
}

impl Drop for LadspaInstance {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Values as numbers in the port's range.
struct Formatter {
    ports: Vec<Port>,
}

impl ParamFormatter for Formatter {
    fn text(&self, id: u32, value: f64) -> Option<String> {
        let port = self.ports.get(id as usize)?;
        let value = port.denormalize(value);
        Some(if port.toggled {
            if value >= 0.5 { tr!("On") } else { tr!("Off") }
        } else if port.integer || value.abs() >= 100.0 {
            format!("{value:.0}")
        } else if value.abs() >= 1.0 {
            format!("{value:.2}")
        } else {
            format!("{value:.3}")
        })
    }

    fn set(&self, _id: u32, _value: f64) {}

    fn get(&self, _id: u32) -> Option<f64> {
        None
    }

    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        let port = self.ports.get(id as usize)?;
        if port.toggled {
            let text = text.trim().to_lowercase();
            if [tr!("On").to_lowercase(), "on".into(), "1".into()].contains(&text) {
                return Some(1.0);
            }
            if [tr!("Off").to_lowercase(), "off".into(), "0".into()].contains(&text) {
                return Some(0.0);
            }
            return None;
        }
        // "50%" is a share of the range, which the caller works out.
        if text.trim().ends_with('%') {
            return None;
        }
        let value = crate::fx::graph::leading_number(text)?;
        Some(port.normalize(value as f32))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A LADSPA amplifier, built with the system's C compiler.
    const AMPLIFIER: &str = r#"
        #include <stdlib.h>
        typedef float LADSPA_Data;
        typedef void *LADSPA_Handle;
        typedef struct { int HintDescriptor; LADSPA_Data LowerBound, UpperBound; } Hint;
        typedef struct Descriptor {
            unsigned long UniqueID; const char *Label; int Properties;
            const char *Name, *Maker, *Copyright; unsigned long PortCount;
            const int *PortDescriptors; const char *const *PortNames; const Hint *PortRangeHints;
            void *ImplementationData;
            LADSPA_Handle (*instantiate)(const struct Descriptor *, unsigned long);
            void (*connect_port)(LADSPA_Handle, unsigned long, LADSPA_Data *);
            void (*activate)(LADSPA_Handle);
            void (*run)(LADSPA_Handle, unsigned long);
            void (*run_adding)(LADSPA_Handle, unsigned long);
            void (*set_run_adding_gain)(LADSPA_Handle, LADSPA_Data);
            void (*deactivate)(LADSPA_Handle);
            void (*cleanup)(LADSPA_Handle);
        } Descriptor;
        typedef struct { LADSPA_Data *gain, *in, *out; } Amp;
        static LADSPA_Handle make(const Descriptor *d, unsigned long rate) { return calloc(1, sizeof(Amp)); }
        static void connect(LADSPA_Handle h, unsigned long port, LADSPA_Data *data) {
            Amp *a = h; if (port == 0) a->gain = data; else if (port == 1) a->in = data; else a->out = data;
        }
        static void run(LADSPA_Handle h, unsigned long n) {
            Amp *a = h; for (unsigned long i = 0; i < n; i++) a->out[i] = a->in[i] * *a->gain;
        }
        static void cleanup(LADSPA_Handle h) { free(h); }
        static const int ports[] = {0x1 | 0x4, 0x1 | 0x8, 0x2 | 0x8};
        static const char *const names[] = {"Gain", "Input", "Output"};
        /* Bounded 0..4, default 1. */
        static const Hint hints[] = {{0x1 | 0x2 | 0x240, 0, 4}, {0, 0, 0}, {0, 0, 0}};
        static const Descriptor amp = {4242, "amp", 0, "Test Amplifier", "VibeFuse", "None", 3,
            ports, names, hints, 0, make, connect, 0, run, 0, 0, 0, cleanup};
        #ifdef _WIN32
        __declspec(dllexport)
        #endif
        const Descriptor *ladspa_descriptor(unsigned long index) { return index == 0 ? &amp : 0; }
    "#;

    /// Builds the amplifier, or `None` without a C compiler.
    pub(crate) fn build_amplifier(name: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("vibefuse-test-ladspa-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let source = dir.join("amp.c");
        let library = dir.join(if cfg!(windows) { "amp.dll" } else { "amp.so" });
        std::fs::write(&source, AMPLIFIER).ok()?;
        let built = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-O2", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .ok()?;
        built.success().then_some(library)
    }

    #[test]
    fn a_ladspa_amplifier_runs_with_its_gain() {
        let Some(library) = build_amplifier("unit") else {
            eprintln!("no C compiler: skipped");
            return;
        };
        let found = LadspaFormat.probe(&library).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Test Amplifier");
        assert_eq!(found[0].vendor, "VibeFuse");
        assert!(!found[0].is_instrument);
        let mut loaded = LadspaFormat.instantiate(&found[0], 48_000.0).unwrap();
        assert_eq!(loaded.parameters.len(), 1);
        assert_eq!(loaded.values, [0.25], "the default gain of 1 in 0..4");
        let formatter = loaded.formatter.as_ref().unwrap();
        assert_eq!(formatter.text(0, 0.5).as_deref(), Some("2.00"));
        loaded.processor.activate(48_000.0, 64, 1).unwrap();
        let mut channels = vec![vec![0.5; 64]];
        loaded
            .processor
            .process(&mut AudioBuffers::new(&mut channels, 64));
        assert_eq!(channels[0][0], 0.5);
        loaded.bridge.send(ParamChange { id: 0, value: 0.75 });
        loaded
            .processor
            .process(&mut AudioBuffers::new(&mut channels, 64));
        assert_eq!(channels[0][0], 1.5, "gain 3");
    }

    #[test]
    fn defaults_follow_ladspa_h() {
        let bounded = sys::HINT_BOUNDED_BELOW | sys::HINT_BOUNDED_ABOVE;
        let at = |default| default_value(bounded | default, -70.0, 1000.0);
        assert_eq!(at(0x200), 0.0);
        assert_eq!(at(0x240), 1.0);
        assert_eq!(at(0x280), 100.0);
        assert_eq!(at(0x2C0), 440.0);
        assert_eq!(at(0x40), -70.0);
        assert_eq!(at(0x140), 1000.0);
        assert_eq!(at(0), -70.0);
    }

    #[test]
    fn values_round_trip_through_the_range() {
        let mut port = Port {
            index: 0,
            name: "test".into(),
            input: true,
            audio: false,
            lower: -20.0,
            upper: 20.0,
            default: 0.0,
            integer: false,
            toggled: false,
        };
        assert_eq!(port.normalize(-20.0), 0.0);
        assert!((port.denormalize(0.5)).abs() < 1e-3);
        port.integer = true;
        assert_eq!(port.denormalize(0.51), 0.0);
        port.toggled = true;
        assert_eq!(port.denormalize(0.49), 0.0);
        assert_eq!(port.normalize(0.3), 1.0);
    }
}
