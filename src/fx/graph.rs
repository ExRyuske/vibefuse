//! What every plug-in format offers the rest of the app: the processor that
//! runs on the audio thread, and what stays with the window (parameters, the
//! plug-in's own editor, its state).
//!
//! Buffers are planar, as VST3, VST2, AU and LADSPA all take them.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{Descriptor, Format, Result};

/// Planar buffers of the chain, borrowed from memory allocated beforehand:
/// collecting slices for each block would allocate on the audio thread.
pub struct AudioBuffers<'a> {
    channels: &'a mut [Vec<f32>],
    frames: usize,
}

impl<'a> AudioBuffers<'a> {
    /// `frames` is cut to the shortest channel.
    pub fn new(channels: &'a mut [Vec<f32>], frames: usize) -> Self {
        let frames = channels.iter().map(Vec::len).min().unwrap_or(0).min(frames);
        Self { channels, frames }
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn channel(&mut self, index: usize) -> &mut [f32] {
        &mut self.channels[index][..self.frames]
    }

    pub fn channel_ref(&self, index: usize) -> &[f32] {
        &self.channels[index][..self.frames]
    }

    /// Which chain channel feeds the plug-in's channel `index`: the mono
    /// microphone goes to both inputs of a stereo plug-in, extra channels get
    /// the last one there is.
    pub fn source_index(&self, index: usize) -> usize {
        index.min(self.channels.len().saturating_sub(1))
    }

    /// Copies the chain's signal into the plug-in's buffers, which hold at
    /// least `frames` samples each.
    pub fn spread_into(&self, destination: &mut [Vec<f32>]) {
        if self.channels.is_empty() {
            return;
        }
        for (index, channel) in destination.iter_mut().enumerate() {
            let source = self.channel_ref(self.source_index(index));
            channel[..self.frames].copy_from_slice(source);
        }
    }

    /// Takes the plug-in's output back; a mono plug-in feeds every channel.
    pub fn collect_from(&mut self, source: &[Vec<f32>]) {
        if source.is_empty() {
            return;
        }
        let last = source.len() - 1;
        let frames = self.frames;
        for index in 0..self.channel_count() {
            let taken = &source[index.min(last)][..frames];
            self.channel(index).copy_from_slice(taken);
        }
    }
}

/// A parameter as the window lists it.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub read_only: bool,
}

/// A parameter change, normalized to 0..=1 as VST3 takes them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamChange {
    pub id: u32,
    pub value: f64,
}

/// Parameter changes on their way from the window to the audio thread,
/// without a lock on the audio side.
pub struct ParamBridge {
    // Guards the window's side only; the audio thread has its own consumer.
    tx: Mutex<rtrb::Producer<ParamChange>>,
}

impl ParamBridge {
    /// Room for every parameter at once (a restored chain sends them all
    /// before the first block) and then some.
    pub fn for_parameters(parameters: usize) -> (Arc<Self>, rtrb::Consumer<ParamChange>) {
        let (tx, rx) = rtrb::RingBuffer::new(parameters + 256);
        (Arc::new(Self { tx: Mutex::new(tx) }), rx)
    }

    /// Never waits: VST3 plug-ins may call this from their audio thread
    /// (`performEdit`). A change lost to a full queue or a busy lock is a
    /// step of a knob still moving; the next one follows.
    pub fn send(&self, change: ParamChange) -> bool {
        self.tx
            .try_lock()
            .map(|mut tx| tx.push(change).is_ok())
            .unwrap_or(false)
    }
}

/// Edits made in a plug-in's own window, on their way to ours. VST2
/// plug-ins report them from their audio thread, so pushing never waits
/// and never allocates: when full, the oldest change makes room.
pub struct ParamInbox {
    pending: Mutex<VecDeque<ParamChange>>,
}

impl ParamInbox {
    const CAPACITY: usize = 256;

    pub fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::with_capacity(Self::CAPACITY)),
        }
    }

    pub fn push(&self, change: ParamChange) {
        let Ok(mut queue) = self.pending.try_lock() else {
            return;
        };
        if queue.len() == queue.capacity() {
            queue.pop_front();
        }
        queue.push_back(change);
    }

    /// Keeps the queue's memory, so the next push does not allocate.
    pub fn drain(&self) -> Vec<ParamChange> {
        self.pending
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }
}

/// What runs in the chain. `process` is called on the audio thread: no
/// allocation, no locks, no I/O, no panics there.
pub trait AudioProcessor: Send {
    /// Prepares for `process`, off the audio thread. `max_frames` bounds the
    /// blocks to come, `channels` is the chain's channel count.
    fn activate(&mut self, sample_rate: f64, max_frames: usize, channels: usize) -> Result<()>;

    fn deactivate(&mut self) {}

    /// Processes all channels in place.
    fn process(&mut self, buffers: &mut AudioBuffers<'_>);
}

/// The plug-in's state beyond its parameter list (some keep settings no
/// parameter shows).
pub trait PluginState {
    fn save(&self) -> Option<Vec<u8>>;
    fn restore(&self, data: &[u8]) -> bool;
}

/// The plug-in's own window. Lives on the UI thread, as window systems and
/// VST3's `IPlugView` require.
pub trait PluginEditor {
    /// Opens the window; opening an open one is fine.
    fn open(&mut self, title: &str) -> Result<()>;
    fn close(&mut self);
    fn is_open(&self) -> bool;
    /// Call regularly while open: notices the user closing it, and keeps
    /// editors that wait for the host to idle them alive.
    fn poll(&mut self);
}

/// Values as the plug-in shows them ("-6.0 dB" rather than 0.42).
pub trait ParamFormatter {
    fn text(&self, id: u32, value: f64) -> Option<String>;
    /// Keeps the plug-in's view of a value in step with what the audio
    /// thread gets (VST3's controller, VST2 and AU directly).
    fn set(&self, id: u32, value: f64);
    /// The value the plug-in has now, to follow its own window.
    fn get(&self, id: u32) -> Option<f64>;
    /// A typed value ("-6 dB") as a normalized one, if the plug-in reads it.
    fn parse(&self, id: u32, text: &str) -> Option<f64>;
}

/// The number at the start of a typed value: "-6,5 dB" is -6.5.
pub fn leading_number(text: &str) -> Option<f64> {
    let text = text.trim().replace(',', ".").replace('\u{2212}', "-");
    let end = text
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && (c == '-' || c == '+'))))
        .map_or(text.len(), |(i, _)| i);
    text[..end].parse().ok().filter(|v: &f64| v.is_finite())
}

/// Edits made in the plug-in's own window.
pub trait EditNotifications {
    fn drain(&self) -> Vec<ParamChange>;
    /// The plug-in changed its inputs, outputs or latency and wants to be
    /// started over (asked once).
    fn wants_restart(&self) -> bool {
        false
    }
}

/// A plug-in once loaded: the processor goes to the audio thread, the rest
/// stays with the window.
pub struct Loaded {
    pub parameters: Vec<ParamInfo>,
    /// Normalized values, parallel to `parameters`.
    pub values: Vec<f64>,
    pub bridge: Arc<ParamBridge>,
    pub formatter: Option<Box<dyn ParamFormatter>>,
    pub editor: Option<Box<dyn PluginEditor>>,
    pub notifications: Option<Box<dyn EditNotifications>>,
    pub state: Option<Box<dyn PluginState>>,
    /// Last: fields drop in order, and the objects above point into the
    /// plug-in.
    pub processor: Box<dyn AudioProcessor>,
}

/// One plug-in format.
pub trait PluginFormat {
    fn format(&self) -> Format;

    /// Reads what a file holds without making a working instance.
    fn probe(&self, path: &Path) -> Result<Vec<Descriptor>>;

    /// Makes an instance, on the UI thread. LADSPA's parameter ranges depend
    /// on the sample rate it is made for.
    fn instantiate(&self, descriptor: &Descriptor, sample_rate: f64) -> Result<Loaded>;
}

/// Flushes denormal numbers to zero while it lives. A decaying signal (the
/// microphone between words, a filter's tail) turns denormal, and x86 then
/// takes a slow path that small buffers do not have the time for; ARM has
/// no such penalty.
pub struct NoDenormals {
    #[cfg(target_arch = "x86_64")]
    previous: u32,
}

impl NoDenormals {
    /// Flush to zero (bit 15) and denormals are zero (bit 6).
    #[cfg(target_arch = "x86_64")]
    const FLAGS: u32 = 0x8000 | 0x0040;

    pub fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: reads and writes this thread's SSE control word; the
            // intrinsics are deprecated in favor of inline assembly only.
            #[allow(deprecated)]
            let previous = unsafe {
                let previous = std::arch::x86_64::_mm_getcsr();
                std::arch::x86_64::_mm_setcsr(previous | Self::FLAGS);
                previous
            };
            Self { previous }
        }
        #[cfg(not(target_arch = "x86_64"))]
        Self {}
    }
}

impl Drop for NoDenormals {
    fn drop(&mut self) {
        // The thread belongs to the driver between blocks.
        #[cfg(target_arch = "x86_64")]
        // SAFETY: restores the value read in `new`.
        #[allow(deprecated)]
        unsafe {
            std::arch::x86_64::_mm_setcsr(self.previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mono_chain_fills_every_plugin_channel() {
        let mut channels = vec![vec![0.25f32; 4]];
        let buffers = AudioBuffers::new(&mut channels, 4);
        let mut plugin = vec![vec![0.0f32; 8]; 2];
        buffers.spread_into(&mut plugin);
        assert_eq!(&plugin[0][..4], &[0.25; 4]);
        assert_eq!(&plugin[1][..4], &[0.25; 4]);
        assert_eq!(plugin[0][4], 0.0, "past the block stays untouched");
    }

    #[test]
    fn extra_plugin_channels_are_dropped_on_the_way_back() {
        let mut channels = vec![vec![0.0f32; 4]];
        let mut buffers = AudioBuffers::new(&mut channels, 4);
        buffers.collect_from(&[vec![1.0f32; 4], vec![-1.0f32; 4]]);
        assert_eq!(buffers.channel_ref(0), &[1.0; 4]);
    }

    #[test]
    fn frames_shrink_to_the_shortest_channel() {
        let mut channels = vec![vec![0.0f32; 8], vec![0.0f32; 3]];
        assert_eq!(AudioBuffers::new(&mut channels, 8).frames(), 3);
    }

    #[test]
    fn empty_layouts_are_harmless() {
        let mut nothing: Vec<Vec<f32>> = Vec::new();
        let mut buffers = AudioBuffers::new(&mut nothing, 4);
        buffers.spread_into(&mut []);
        buffers.collect_from(&[]);
    }

    #[test]
    fn the_inbox_keeps_the_newest_and_its_memory() {
        let inbox = ParamInbox::new();
        let capacity = inbox.pending.lock().unwrap().capacity();
        for id in 0..(capacity as u32 + 50) {
            inbox.push(ParamChange {
                id,
                value: f64::from(id),
            });
        }
        let drained = inbox.drain();
        assert_eq!(drained.len(), capacity);
        assert_eq!(drained.last().unwrap().id, capacity as u32 + 49);
        assert_eq!(inbox.pending.lock().unwrap().capacity(), capacity);
    }

    #[test]
    fn a_busy_inbox_drops_the_change_instead_of_waiting() {
        let inbox = ParamInbox::new();
        let held = inbox.pending.lock().unwrap();
        inbox.push(ParamChange { id: 7, value: 1.0 });
        assert!(held.is_empty());
        drop(held);
        assert!(inbox.drain().is_empty());
    }
}
