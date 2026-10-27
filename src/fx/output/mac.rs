//! The effects' stream on macOS: a private aggregate device of the MiniFuse
//! (its clock leads) and the chosen output, which follows through drift
//! correction. Its IO callback takes Input 1, runs the chain and plays the
//! result on the output's first two channels, in one block: nothing is
//! buffered on the way.
//!
//! The audio server can hang (see [`crate::virtual_devices`]), so all of it
//! runs on a thread of its own, which also looks every second whether the
//! devices are still the ones it started with.

use std::ffi::c_void;
use std::sync::{Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use super::{Output, Status};
use crate::device::coreaudio::{
    self, AudioBuffer, AudioBufferList, AudioObjectId, Cf, IoProcId, OsStatus, SCOPE_INPUT,
    SCOPE_OUTPUT,
};
use crate::device::hal_mixer::BRIDGE_UID;
use crate::fx::rack;
use crate::tr;

const AGGREGATE_UID: &str = "cc.yeru.vibefuse.effects";
/// 2.7 ms at 48 kHz.
const BUFFER_FRAMES: u32 = 128;
/// Medium quality of the resampler that follows the card's clock.
const DRIFT_QUALITY: i32 = 0x40;
/// How often the devices are looked at.
const LOOK: Duration = Duration::from_secs(1);
/// Pause before trying again after the stream failed to start.
const RETRY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Shared {
    wanted: Option<String>,
    /// The window asked for the list of outputs.
    refresh: bool,
    outputs: Vec<Output>,
    status: Status,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    wanted: None,
    refresh: false,
    outputs: Vec::new(),
    status: Status {
        rate: None,
        problem: None,
    },
});
static WAKE: Condvar = Condvar::new();
static THREAD: Once = Once::new();

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn start_thread() {
    THREAD.call_once(|| {
        let _ = thread::Builder::new()
            .name("vibefuse-effects".into())
            .spawn(run);
    });
}

/// The output to play on (a device UID), or `None` to stop.
pub fn set(output: Option<&str>) {
    let mut shared = shared();
    if shared.wanted.as_deref() != output {
        shared.wanted = output.map(str::to_owned);
        WAKE.notify_all();
    }
    drop(shared);
    if output.is_some() {
        start_thread();
    }
}

pub fn tick() {}

/// Asks for the list of outputs; it arrives in [`outputs`] shortly.
pub fn refresh() {
    shared().refresh = true;
    WAKE.notify_all();
    start_thread();
}

pub fn outputs() -> Vec<Output> {
    shared().outputs.clone()
}

pub fn status() -> Status {
    shared().status.clone()
}

/// The devices to offer: not our own private ones, nor the bridge.
fn list() -> Vec<Output> {
    let mut outputs: Vec<Output> = coreaudio::output_devices()
        .into_iter()
        .filter(|(uid, _)| !uid.starts_with("cc.yeru.vibefuse.") && uid != BRIDGE_UID)
        .map(|(id, name)| Output { id, name })
        .collect();
    outputs.sort_by_key(|o| o.name.to_lowercase());
    outputs
}

fn run() {
    let mut running: Option<Running> = None;
    let mut retry: Option<Instant> = None;
    // What the last round worked on: anything else is new.
    let mut seen: Option<String> = None;
    loop {
        let (wanted, refresh) = {
            let mut shared = shared();
            if !shared.refresh && shared.wanted == seen {
                shared = WAKE
                    .wait_timeout(shared, LOOK)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
            (shared.wanted.clone(), std::mem::take(&mut shared.refresh))
        };
        seen = wanted.clone();
        if refresh {
            let outputs = list();
            shared().outputs = outputs;
        }
        if running
            .as_ref()
            .is_some_and(|r| Some(&r.uid) != wanted.as_ref() || !r.healthy())
        {
            running = None;
            retry = None;
        }
        let Some(uid) = wanted else {
            retry = None;
            shared().status = Status::default();
            continue;
        };
        if running.is_some() || retry.is_some_and(|at| Instant::now() < at) {
            continue;
        }
        let status = match Running::start(&uid) {
            Ok(started) => {
                let rate = started.rate;
                running = Some(started);
                retry = None;
                Status {
                    rate: Some(rate),
                    problem: None,
                }
            }
            Err(problem) => {
                retry = Some(Instant::now() + RETRY);
                Status {
                    rate: None,
                    problem: Some(problem),
                }
            }
        };
        shared().status = status;
    }
}

/// Where a channel is in a buffer list.
#[derive(Clone, Copy)]
struct Channel {
    buffer: usize,
    offset: usize,
}

/// The `index`th channel of buffers with these channel counts, the first
/// `skip` buffers left out.
fn channel(buffers: &[u32], skip: usize, index: usize) -> Option<Channel> {
    let mut first = 0;
    for (buffer, &channels) in buffers.iter().enumerate() {
        let channels = channels as usize;
        if index < first + channels {
            return Some(Channel {
                buffer: buffer + skip,
                offset: index - first,
            });
        }
        first += channels;
    }
    None
}

struct Layout {
    input: Option<Channel>,
    outputs: [Option<Channel>; 2],
    rate: f64,
}

struct Running {
    uid: String,
    card: AudioObjectId,
    output: AudioObjectId,
    rate: f64,
    aggregate: AudioObjectId,
    io: IoProcId,
    /// Read by the IO callback until it stops.
    _layout: Box<Layout>,
}

impl Running {
    fn start(uid: &str) -> Result<Self, String> {
        let card = coreaudio::find_device("").ok_or_else(|| tr!("Connect your MiniFuse 1"))?;
        let output =
            coreaudio::device_by_uid(uid).ok_or_else(|| tr!("The output is not connected"))?;
        let rate = coreaudio::nominal_rate(card).ok_or("the MiniFuse has no rate")?;
        let card_uid = coreaudio::device_uid(card).ok_or("the MiniFuse has no UID")?;
        let card_ref = Cf::string(&card_uid);
        let mut subdevices = vec![Cf::dictionary(&[("uid", &card_ref)])];
        if output != card {
            // Both run at the card's rate; the output follows its clock.
            let _ = coreaudio::set_nominal_rate(output, rate);
            subdevices.push(Cf::dictionary(&[
                ("uid", &Cf::string(uid)),
                ("drift", &Cf::number(1)),
                ("drift quality", &Cf::number(DRIFT_QUALITY)),
            ]));
        }
        let description = Cf::dictionary(&[
            ("uid", &Cf::string(AGGREGATE_UID)),
            ("name", &Cf::string("VibeFuse Effects")),
            ("private", &Cf::number(1)),
            ("stacked", &Cf::number(0)),
            ("master", &card_ref),
            ("clock", &card_ref),
            (
                "subdevices",
                &Cf::array(&subdevices.iter().collect::<Vec<_>>()),
            ),
        ]);
        let aggregate = coreaudio::create_aggregate(&description)?;
        let card_in = coreaudio::stream_channels(card, SCOPE_INPUT);
        let card_out = coreaudio::stream_channels(card, SCOPE_OUTPUT);
        // The output's buffers follow the card's in the aggregate.
        let (out, skip) = if output == card {
            (card_out, 0)
        } else {
            (
                coreaudio::stream_channels(output, SCOPE_OUTPUT),
                card_out.len(),
            )
        };
        let layout = Box::new(Layout {
            input: channel(&card_in, 0, 0),
            outputs: [channel(&out, skip, 0), channel(&out, skip, 1)],
            rate,
        });
        let _ = coreaudio::set_buffer_frames(aggregate, BUFFER_FRAMES);
        let client = (&raw const *layout).cast_mut().cast::<c_void>();
        match coreaudio::start_io(aggregate, io, client) {
            Ok(io) => Ok(Self {
                uid: uid.to_owned(),
                card,
                output,
                rate,
                aggregate,
                io,
                _layout: layout,
            }),
            Err(error) => {
                coreaudio::destroy_aggregate(aggregate);
                Err(error)
            }
        }
    }

    /// The devices are still the ones it started with.
    fn healthy(&self) -> bool {
        coreaudio::find_device("") == Some(self.card)
            && coreaudio::nominal_rate(self.card) == Some(self.rate)
            && coreaudio::device_by_uid(&self.uid) == Some(self.output)
            && coreaudio::device_uid(self.aggregate).as_deref() == Some(AGGREGATE_UID)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        coreaudio::stop_io(self.aggregate, self.io);
        coreaudio::destroy_aggregate(self.aggregate);
    }
}

/// One channel's samples in a buffer list.
struct Samples {
    data: *mut f32,
    stride: usize,
    frames: usize,
}

impl Samples {
    fn of(buffers: &[AudioBuffer], channel: Option<Channel>) -> Option<Self> {
        let channel = channel?;
        let buffer = buffers.get(channel.buffer)?;
        let stride = buffer.channels as usize;
        if buffer.data.is_null() || channel.offset >= stride {
            return None;
        }
        Some(Self {
            // SAFETY: the offset is within one frame of the buffer.
            data: unsafe { buffer.data.cast::<f32>().add(channel.offset) },
            stride,
            frames: buffer.bytes as usize / 4 / stride,
        })
    }

    fn get(&self, i: usize) -> f32 {
        // SAFETY: callers keep `i` below `frames`.
        unsafe { *self.data.add(i * self.stride) }
    }

    fn set(&self, i: usize, value: f32) {
        if i < self.frames {
            // SAFETY: checked against `frames`.
            unsafe { *self.data.add(i * self.stride) = value };
        }
    }
}

/// Runs on the aggregate's IO thread.
unsafe extern "C" fn io(
    _device: AudioObjectId,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    output: *mut AudioBufferList,
    _output_time: *const c_void,
    client: *mut c_void,
) -> OsStatus {
    // SAFETY: `client` is the layout owned by `Running`, which outlives the
    // callback; the buffer lists are CoreAudio's for this cycle.
    let (layout, inputs, outputs) = unsafe {
        (
            &*client.cast::<Layout>(),
            AudioBufferList::buffers(input),
            AudioBufferList::buffers(output),
        )
    };
    let Some(from) = Samples::of(inputs, layout.input) else {
        return 0;
    };
    let to = layout.outputs.map(|channel| Samples::of(outputs, channel));
    let play = |i: usize, sample: f32| {
        for to in to.iter().flatten() {
            to.set(i, sample);
        }
    };
    // Without the chain (busy, switched off in between), the microphone
    // still plays as it is.
    if !rack::process_with(from.frames, layout.rate, |i| from.get(i), play) {
        for i in 0..from.frames {
            play(i, from.get(i));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_found_past_the_cards_buffers() {
        // A stereo card, then a device with one 4-channel buffer.
        assert!(matches!(
            channel(&[4], 1, 1),
            Some(Channel {
                buffer: 1,
                offset: 1
            })
        ));
        assert!(matches!(
            channel(&[1, 1], 0, 1),
            Some(Channel {
                buffer: 1,
                offset: 0
            })
        ));
        assert!(channel(&[2], 0, 2).is_none());
    }
}
