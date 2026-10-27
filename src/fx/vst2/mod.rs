//! VST2, for the plug-ins that never moved to VST3. A plug-in is a plain
//! library (`.dll`) on Windows and a `.vst` bundle on macOS; the files of a
//! folder are tried one by one, and those without the entry point skipped.
//!
//! Steinberg no longer licenses the VST2 SDK; this host declares the few
//! structures it needs itself.

mod raw;

use std::path::Path;
use std::sync::Arc;

use super::graph::{
    AudioBuffers, AudioProcessor, EditNotifications, Loaded, ParamBridge, ParamChange,
    ParamFormatter, ParamInfo, PluginEditor, PluginFormat, PluginState,
};
use super::window::PluginWindow;
use super::{Descriptor, Format, Result};
use raw::RawPlugin;

pub struct Vst2Format;

impl PluginFormat for Vst2Format {
    fn format(&self) -> Format {
        Format::Vst2
    }

    fn probe(&self, path: &Path) -> Result<Vec<Descriptor>> {
        let plugin = RawPlugin::load(path)?;
        let name = plugin.name();
        Ok(vec![Descriptor {
            format: Format::Vst2,
            path: path.to_path_buf(),
            // One plug-in per file, known by its 32-bit id.
            uid: format!("{:08X}", plugin.unique_id()),
            name: if name.is_empty() { stem(path) } else { name },
            vendor: plugin.vendor(),
            is_instrument: plugin.num_inputs() == 0 && plugin.num_outputs() > 0,
        }])
    }

    fn instantiate(&self, descriptor: &Descriptor, _sample_rate: f64) -> Result<Loaded> {
        create(descriptor)
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// One instance serves the audio, the editor and the parameter list: edits
/// in the plug-in's window must reach the instance that makes the sound.
fn create(descriptor: &Descriptor) -> Result<Loaded> {
    let plugin = Arc::new(RawPlugin::load(&descriptor.path)?);
    let count = plugin.num_params().max(0);
    let parameters: Vec<ParamInfo> = (0..count)
        .map(|index| ParamInfo {
            id: index as u32,
            title: plugin.parameter_name(index),
            units: plugin.parameter_label(index),
            read_only: false,
        })
        .collect();
    let values = (0..count)
        .map(|index| f64::from(plugin.get_parameter(index)))
        .collect();
    let (bridge, changes) = ParamBridge::for_parameters(parameters.len());
    let editor = plugin.has_editor().then(|| {
        Box::new(Vst2Editor {
            plugin: Arc::clone(&plugin),
            window: None,
        }) as Box<dyn PluginEditor>
    });
    Ok(Loaded {
        parameters,
        values,
        bridge,
        formatter: Some(Box::new(Shared(Arc::clone(&plugin)))),
        editor,
        notifications: Some(Box::new(Shared(Arc::clone(&plugin)))),
        state: Some(Box::new(Shared(Arc::clone(&plugin)))),
        processor: Box::new(Vst2Instance {
            plugin,
            name: descriptor.name.clone(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            input_ptrs: Vec::new(),
            output_ptrs: Vec::new(),
            changes,
            max_frames: 0,
            active: false,
        }),
    })
}

struct Vst2Instance {
    plugin: Arc<RawPlugin>,
    name: String,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    /// Channel pointers as VST2 takes them, built in `activate`.
    input_ptrs: Vec<*const f32>,
    output_ptrs: Vec<*mut f32>,
    changes: rtrb::Consumer<ParamChange>,
    max_frames: usize,
    active: bool,
}

// SAFETY: the pointers point into the instance's own buffers; the plug-in
// itself is Send and Sync (see `RawPlugin`).
unsafe impl Send for Vst2Instance {}

impl AudioProcessor for Vst2Instance {
    fn activate(&mut self, sample_rate: f64, max_frames: usize, _channels: usize) -> Result<()> {
        self.deactivate();
        let inputs = self.plugin.num_inputs().max(0) as usize;
        let outputs = self.plugin.num_outputs().max(0) as usize;
        if outputs == 0 {
            return Err(format!("{}: the plug-in has no outputs", self.name));
        }
        self.plugin.set_sample_rate(sample_rate as f32);
        self.plugin.set_block_size(max_frames);
        self.inputs = vec![vec![0.0; max_frames]; inputs];
        self.outputs = vec![vec![0.0; max_frames]; outputs];
        self.input_ptrs = self.inputs.iter().map(|c| c.as_ptr()).collect();
        self.output_ptrs = self.outputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        self.max_frames = max_frames;
        self.plugin.set_active(true);
        self.active = true;
        Ok(())
    }

    fn deactivate(&mut self) {
        if std::mem::take(&mut self.active) {
            self.plugin.set_active(false);
        }
    }

    fn process(&mut self, buffers: &mut AudioBuffers<'_>) {
        let frames = buffers.frames();
        if !self.active || frames > self.max_frames || buffers.channel_count() == 0 {
            return;
        }
        while let Ok(change) = self.changes.pop() {
            self.plugin
                .set_parameter(change.id as i32, change.value as f32);
        }
        buffers.spread_into(&mut self.inputs);
        // SAFETY: the pointers lead to buffers of `max_frames` made in
        // `activate`, as many as the plug-in declares.
        unsafe {
            self.plugin.process_replacing(
                self.input_ptrs.as_ptr(),
                self.output_ptrs.as_mut_ptr(),
                frames as i32,
            );
        }
        buffers.collect_from(&self.outputs);
    }
}

/// The window's view of the same instance.
struct Shared(Arc<RawPlugin>);

impl ParamFormatter for Shared {
    fn text(&self, id: u32, _value: f64) -> Option<String> {
        // VST2 can only tell the current value, which the caller has set.
        let text = self.0.parameter_display(id as i32);
        (!text.is_empty()).then_some(text)
    }

    fn set(&self, id: u32, value: f64) {
        self.0.set_parameter(id as i32, value as f32);
    }

    fn get(&self, id: u32) -> Option<f64> {
        Some(f64::from(self.0.get_parameter(id as i32)))
    }

    fn parse(&self, id: u32, text: &str) -> Option<f64> {
        // The plug-in sets the value itself: what it became is the answer.
        self.0
            .string_to_parameter(id as i32, text)
            .then(|| f64::from(self.0.get_parameter(id as i32)))
    }
}

impl EditNotifications for Shared {
    fn drain(&self) -> Vec<ParamChange> {
        self.0.edits().drain()
    }

    fn wants_restart(&self) -> bool {
        self.0.take_restart()
    }
}

impl PluginState for Shared {
    fn save(&self) -> Option<Vec<u8>> {
        self.0.get_chunk()
    }

    fn restore(&self, data: &[u8]) -> bool {
        self.0.set_chunk(data)
    }
}

struct Vst2Editor {
    plugin: Arc<RawPlugin>,
    window: Option<PluginWindow>,
}

impl PluginEditor for Vst2Editor {
    fn open(&mut self, title: &str) -> Result<()> {
        if let Some(window) = &self.window {
            window.show();
            return Ok(());
        }
        // Plug-ins that do not tell their size before opening fit the window
        // to themselves after.
        let (width, height) = self.plugin.editor_size().unwrap_or((600, 400));
        let window = PluginWindow::open(title, width, height)?;
        // SAFETY: the window outlives the editor, see `close`. A 0 answer is
        // no failure: the SDK's base class returns it after opening fine.
        unsafe { self.plugin.editor_open(window.handle()) };
        if let Some((width, height)) = self.plugin.editor_size() {
            window.resize(width, height);
        }
        window.show();
        self.window = Some(window);
        Ok(())
    }

    fn close(&mut self) {
        // The plug-in lets go of the window before it goes.
        if self.window.is_some() {
            self.plugin.editor_close();
            self.window = None;
        }
    }

    fn is_open(&self) -> bool {
        self.window.is_some()
    }

    fn poll(&mut self) {
        let Some(window) = &self.window else { return };
        if window.closed_by_user() {
            return self.close();
        }
        if let Some((width, height)) = self.plugin.take_window_size() {
            window.resize(width, height);
        }
        self.plugin.editor_idle();
    }
}

impl Drop for Vst2Editor {
    fn drop(&mut self) {
        self.close();
    }
}
