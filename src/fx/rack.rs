//! The chain on the audio side: the plug-ins in order, run on the
//! microphone by whatever carries it (the bridge on macOS, the ASIO stream
//! on Windows).
//!
//! The window changes the chain under the same lock the audio thread takes
//! for each block. The audio thread only tries it: while the window holds it
//! (adding, moving or re-activating plug-ins, which is rare and short), the
//! block passes unprocessed instead of waiting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use super::graph::{AudioBuffers, AudioProcessor, NoDenormals};

/// The longest block a plug-in gets; longer ones are processed in parts.
pub const MAX_BLOCK: usize = 1024;
/// The microphone is mono, and so is the chain: plug-ins that want stereo
/// get it doubled and give back their left channel.
pub const CHANNELS: usize = 1;

/// A plug-in in the chain.
pub struct Insert {
    /// Matches the window's slot.
    pub id: u64,
    pub processor: Box<dyn AudioProcessor>,
    pub bypassed: bool,
    /// Activated for the rack's rate; one that refused is skipped.
    pub active: bool,
}

pub struct Rack {
    pub inserts: Vec<Insert>,
    /// The rate the inserts are activated for; 0 before the first time.
    pub rate: f64,
    buffers: Vec<Vec<f32>>,
}

static RACK: Mutex<Rack> = Mutex::new(Rack {
    inserts: Vec::new(),
    rate: 0.0,
    buffers: Vec::new(),
});
/// The master switch, read before anything else on each block.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The rack for the window. Waits at most one block of the audio thread.
pub fn lock() -> MutexGuard<'static, Rack> {
    RACK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

impl Rack {
    /// Activates every insert for `rate`; returns the ids that refused, with
    /// the reason.
    pub fn activate(&mut self, rate: f64) -> Vec<(u64, String)> {
        if self.buffers.is_empty() {
            self.buffers = vec![vec![0.0; MAX_BLOCK]; CHANNELS];
        }
        self.rate = rate;
        let mut failed = Vec::new();
        for insert in &mut self.inserts {
            insert.processor.deactivate();
            match insert.processor.activate(rate, MAX_BLOCK, CHANNELS) {
                Ok(()) => insert.active = true,
                Err(error) => {
                    insert.active = false;
                    failed.push((insert.id, error));
                }
            }
        }
        failed
    }

    /// Makes room for blocks before the first activation, so a processor
    /// added later only needs its own.
    pub fn prepare(&mut self, rate: f64) {
        if self.buffers.is_empty() {
            self.buffers = vec![vec![0.0; MAX_BLOCK]; CHANNELS];
        }
        self.rate = rate;
    }

    fn working(&self) -> bool {
        !self.buffers.is_empty() && self.inserts.iter().any(|i| i.active && !i.bypassed)
    }

    /// Runs the chain over the first `frames` samples of the chain buffer.
    fn run_block(&mut self, frames: usize) {
        let Rack {
            inserts, buffers, ..
        } = self;
        let mut audio = AudioBuffers::new(buffers, frames);
        for insert in inserts.iter_mut().filter(|i| i.active && !i.bypassed) {
            insert.processor.process(&mut audio);
        }
        // A filter gone wild gives NaN or infinity from then on: silence is
        // better than that on the card and in every mix.
        for sample in &mut buffers[0][..frames] {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }
    }
}

/// Takes the rack for one block of the audio thread, if it is to run at
/// `rate` and nobody else has it.
fn try_rack(rate: f64) -> Option<MutexGuard<'static, Rack>> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let rack = match RACK.try_lock() {
        Ok(rack) => rack,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    (rack.rate == rate && rack.working()).then_some(rack)
}

/// Runs the chain over `frames` samples read from `input` and written to
/// `output`, on the audio thread. Returns false, having touched nothing,
/// when the chain does not run: the caller passes the signal on itself.
#[cfg_attr(windows, allow(dead_code))]
pub fn process_with(
    frames: usize,
    rate: f64,
    input: impl Fn(usize) -> f32,
    mut output: impl FnMut(usize, f32),
) -> bool {
    let Some(mut rack) = try_rack(rate) else {
        return false;
    };
    let _flush = NoDenormals::new();
    let mut start = 0;
    while start < frames {
        let count = (frames - start).min(MAX_BLOCK);
        for (i, sample) in rack.buffers[0][..count].iter_mut().enumerate() {
            *sample = input(start + i);
        }
        rack.run_block(count);
        for (i, &sample) in rack.buffers[0][..count].iter().enumerate() {
            output(start + i, sample);
        }
        start += count;
    }
    true
}

/// [`process_with`] in place.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn process(samples: &mut [f32], rate: f64) -> bool {
    let Some(mut rack) = try_rack(rate) else {
        return false;
    };
    let _flush = NoDenormals::new();
    for chunk in samples.chunks_mut(MAX_BLOCK) {
        let count = chunk.len();
        rack.buffers[0][..count].copy_from_slice(chunk);
        rack.run_block(count);
        chunk.copy_from_slice(&rack.buffers[0][..count]);
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fx::Result;

    /// Multiplies by a factor; the tests run it in place of a plug-in.
    pub struct Gain(pub f32);

    impl AudioProcessor for Gain {
        fn activate(&mut self, _: f64, _: usize, _: usize) -> Result<()> {
            Ok(())
        }

        fn process(&mut self, buffers: &mut AudioBuffers<'_>) {
            for channel in 0..buffers.channel_count() {
                for sample in buffers.channel(channel) {
                    *sample *= self.0;
                }
            }
        }
    }

    /// The rack is one per process: tests that use it take turns.
    pub static TURN: Mutex<()> = Mutex::new(());

    #[test]
    fn the_chain_runs_in_order_at_its_rate_only() {
        let _turn = TURN.lock().unwrap_or_else(|p| p.into_inner());
        {
            let mut rack = lock();
            rack.inserts = vec![
                Insert {
                    id: 1,
                    processor: Box::new(Gain(2.0)),
                    bypassed: false,
                    active: false,
                },
                Insert {
                    id: 2,
                    processor: Box::new(Gain(10.0)),
                    bypassed: true,
                    active: false,
                },
            ];
            assert!(rack.activate(48_000.0).is_empty());
        }
        set_enabled(true);

        // Longer than a block: processed in parts.
        let mut samples = vec![0.25; MAX_BLOCK + 3];
        assert!(process(&mut samples, 48_000.0));
        assert!(samples.iter().all(|&s| s == 0.5), "bypassed insert ran");

        let mut other = vec![0.25; 8];
        assert!(!process(&mut other, 44_100.0), "ran at another rate");
        assert_eq!(other, vec![0.25; 8]);

        let mut out = [0.0; 4];
        assert!(process_with(4, 48_000.0, |i| i as f32, |i, s| out[i] = s));
        assert_eq!(out, [0.0, 2.0, 4.0, 6.0]);

        set_enabled(false);
        let mut off = vec![0.25; 8];
        assert!(!process(&mut off, 48_000.0));
        lock().inserts.clear();
    }

    #[test]
    fn a_busy_rack_lets_the_block_through() {
        let _turn = TURN.lock().unwrap_or_else(|p| p.into_inner());
        {
            let mut rack = lock();
            rack.inserts = vec![Insert {
                id: 1,
                processor: Box::new(Gain(f32::NAN)),
                bypassed: false,
                active: false,
            }];
            rack.activate(48_000.0);
        }
        set_enabled(true);
        let held = lock();
        let mut samples = vec![0.25; 8];
        assert!(!process(&mut samples, 48_000.0));
        drop(held);
        // NaN from a plug-in reaches nobody.
        assert!(process(&mut samples, 48_000.0));
        assert_eq!(samples, vec![0.0; 8]);
        set_enabled(false);
        lock().inserts.clear();
    }
}
