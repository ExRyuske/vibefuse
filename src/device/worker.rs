//! Background thread that owns the device.
//!
//! It finds the device, restores the saved mix on connect, applies writes
//! and keeps the state, meters and sample rate up to date. Writes to the
//! same parameter are coalesced, so a dragged fader only sends its latest
//! value. Devices that are not live (Linux) are never polled: writes are
//! collected and applied in one session once the user pauses. On Windows
//! the driver's software mixer and ASIO settings are handled here too.

use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use super::protocol::{METER_SLOTS, RawState, Write};
use super::sls::{self, Gains, Sls};
use super::{
    Asio, Bus, Device, DeviceInfo, Found, SampleRate, read_meters, read_state, write_params,
};
use crate::tr;

const SCAN_PERIOD: Duration = Duration::from_secs(1);
const PRESENCE_PERIOD: Duration = Duration::from_secs(2);
/// Quiet time before collected writes are applied to a non-live device.
const APPLY_DELAY: Duration = Duration::from_millis(500);
const MAX_FAILURES: u32 = 3;
/// After this long without signal the meters have fallen to the floor, and
/// further all-zero readings are not reported: an idle panel stays asleep.
const SILENCE_HOLD: Duration = Duration::from_secs(3);
/// Meter period while there is no signal: a sound shows up within 0.2 s.
const SILENT_METER_PERIOD: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub enum Command {
    Write(Vec<Write>),
    /// Gains for the driver's software mixer.
    Mixer(Gains),
    SetSampleRate(u32),
    SetAsio {
        size: u32,
        safe_mode: bool,
    },
    LeaveStorageMode,
    Pace(Pace),
    /// Connect again, e.g. after the virtual devices were installed.
    Rescan,
}

/// How closely a connected device is followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// The window is on screen: meters at 20 Hz, smooth enough for peak
    /// meters and a fifth fewer frames to draw than 25 Hz.
    Visible,
    /// The window is minimized: one tick a second, so the UI notices when
    /// it is shown again.
    Hidden,
    /// No window (agent): only follow the connection.
    Background,
}

impl Pace {
    fn meters(self) -> Option<Duration> {
        match self {
            Pace::Visible => Some(Duration::from_millis(50)),
            Pace::Hidden => Some(Duration::from_secs(1)),
            Pace::Background => None,
        }
    }

    fn state(self) -> Duration {
        match self {
            Pace::Visible => Duration::from_millis(500),
            Pace::Hidden => Duration::from_secs(1),
            Pace::Background => Duration::from_secs(5),
        }
    }

    fn rate(self) -> Option<Duration> {
        match self {
            Pace::Visible => Some(Duration::from_secs(1)),
            Pace::Hidden => Some(Duration::from_secs(5)),
            Pace::Background => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Searching,
    Storage,
    Unavailable(String),
    Connected {
        info: DeviceInfo,
        state: RawState,
        live: bool,
        /// The driver's software mixer is available (Windows).
        mixer: bool,
    },
    State(RawState),
    Meters([f32; METER_SLOTS]),
    /// Software mixer meters in dB.
    MixerMeters(sls::Meters),
    SampleRate(Option<SampleRate>),
    Asio(Option<Asio>),
    Disconnected,
    Error(String),
}

/// What a freshly connected device gets.
#[derive(Debug, Clone, Default)]
pub struct Restoration {
    pub writes: Vec<Write>,
    /// Software mixer to load, when the device has one.
    pub mixer: Option<Sls>,
}

/// Decides how to bring a freshly connected device to the saved settings;
/// the flag tells whether it has the software mixer.
pub type Restore = Box<dyn Fn(&DeviceInfo, &RawState, bool) -> Restoration + Send>;

/// Starts the worker. It stops once `emit` returns `false` or every command
/// sender is dropped.
pub fn spawn(
    bus: Box<dyn Bus>,
    restore: Restore,
    pace: Pace,
    emit: impl FnMut(Event) -> bool + Send + 'static,
) -> Sender<Command> {
    let (sender, commands) = mpsc::channel();
    thread::Builder::new()
        .name("vibefuse-device".into())
        .spawn(move || Worker::new(bus, restore, pace, commands, emit).run())
        .expect("failed to start the device thread");
    sender
}

#[derive(PartialEq)]
enum Status {
    Searching,
    Storage,
    Unavailable(String),
}

struct Worker<E> {
    bus: Box<dyn Bus>,
    restore: Restore,
    commands: Receiver<Command>,
    emit: E,
    device: Option<Box<dyn Device>>,
    status: Option<Status>,
    state: Option<RawState>,
    rate: Option<SampleRate>,
    asio: Option<Asio>,
    pending: Vec<Write>,
    /// Software mixer meters are switched on in the driver.
    metering: bool,
    apply_at: Option<Instant>,
    pace: Pace,
    silent_since: Option<Instant>,
    last_error: Option<String>,
    failures: u32,
    next_scan: Instant,
    next_meters: Instant,
    next_state: Instant,
    next_rate: Instant,
    next_presence: Instant,
    alive: bool,
}

impl<E: FnMut(Event) -> bool> Worker<E> {
    fn new(
        bus: Box<dyn Bus>,
        restore: Restore,
        pace: Pace,
        commands: Receiver<Command>,
        emit: E,
    ) -> Self {
        let now = Instant::now();
        Self {
            bus,
            restore,
            commands,
            emit,
            device: None,
            status: None,
            state: None,
            rate: None,
            asio: None,
            pending: Vec::new(),
            metering: false,
            apply_at: None,
            pace,
            silent_since: None,
            last_error: None,
            failures: 0,
            next_scan: now,
            next_meters: now,
            next_state: now,
            next_rate: now,
            next_presence: now,
            alive: true,
        }
    }

    fn emit(&mut self, event: Event) {
        if !(self.emit)(event) {
            self.alive = false;
        }
    }

    fn report(&mut self, message: String) {
        if self.last_error.as_ref() != Some(&message) {
            self.last_error = Some(message.clone());
            self.emit(Event::Error(message));
        }
    }

    fn live(&self) -> bool {
        self.device.as_ref().is_some_and(|device| device.is_live())
    }

    fn has_mixer(&self) -> bool {
        self.device
            .as_ref()
            .is_some_and(|device| device.has_mixer())
    }

    fn deadline(&self) -> Instant {
        if self.device.is_none() {
            return self.next_scan;
        }
        let mut next = if self.live() {
            let polls = self.next_state;
            if self.pace.meters().is_some() {
                polls.min(self.next_meters)
            } else {
                polls
            }
        } else {
            self.next_presence
        };
        if self.pace.rate().is_some() {
            next = next.min(self.next_rate);
        }
        self.apply_at.map_or(next, |at| next.min(at))
    }

    fn run(mut self) {
        while self.alive {
            let timeout = self.deadline().saturating_duration_since(Instant::now());
            let batch = match self.commands.recv_timeout(timeout) {
                Ok(command) => {
                    let mut batch = vec![command];
                    batch.extend(self.commands.try_iter());
                    batch
                }
                Err(RecvTimeoutError::Timeout) => Vec::new(),
                Err(RecvTimeoutError::Disconnected) => return,
            };
            // A bug met while talking to the device costs the connection,
            // not the app: the device is opened again from scratch.
            let step = panic::catch_unwind(AssertUnwindSafe(|| {
                if !batch.is_empty() {
                    self.execute(batch);
                }
                self.tick(Instant::now());
            }));
            if step.is_err() {
                self.disconnect();
                self.report(tr!("Reconnected to the MiniFuse 1 after an error."));
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        if self.device.is_none() {
            if now >= self.next_scan {
                self.next_scan = now + SCAN_PERIOD;
                self.scan();
            }
            return;
        }
        if self.apply_at.is_some_and(|at| now >= at) {
            self.apply();
        }
        if self.live() {
            if let Some(period) = self.pace.meters()
                && now >= self.next_meters
            {
                let silent = self
                    .silent_since
                    .is_some_and(|since| now.duration_since(since) > SILENCE_HOLD);
                self.next_meters = now
                    + if silent {
                        period.max(SILENT_METER_PERIOD)
                    } else {
                        period
                    };
                self.poll_meters();
            }
            if now >= self.next_state {
                self.next_state = now + self.pace.state();
                self.poll_state();
            }
        } else if now >= self.next_presence {
            self.next_presence = now + PRESENCE_PERIOD;
            if !self
                .device
                .as_mut()
                .is_some_and(|device| device.is_attached())
            {
                self.disconnect();
            }
        }
        if let Some(period) = self.pace.rate()
            && self.device.is_some()
            && now >= self.next_rate
        {
            self.next_rate = now + period;
            self.poll_rate();
            self.poll_asio();
        }
    }

    fn set_status(&mut self, status: Status) {
        if self.status.as_ref() != Some(&status) {
            let event = match &status {
                Status::Searching => Event::Searching,
                Status::Storage => Event::Storage,
                Status::Unavailable(reason) => Event::Unavailable(reason.clone()),
            };
            self.status = Some(status);
            self.emit(event);
        }
    }

    fn scan(&mut self) {
        match self.bus.scan() {
            Ok(Found::Audio(device)) => self.connect(device),
            Ok(Found::Storage) => self.set_status(Status::Storage),
            Ok(Found::Unavailable(reason)) => self.set_status(Status::Unavailable(reason)),
            Ok(Found::Nothing) => self.set_status(Status::Searching),
            Err(error) => {
                self.set_status(Status::Searching);
                self.report(tr!("MiniFuse 1 is not responding: {}", error));
            }
        }
    }

    fn connect(&mut self, mut device: Box<dyn Device>) {
        let info = device.info().clone();
        let mixer = device.has_mixer();
        let restore = &self.restore;
        let mut connected = None;
        let mut restoration = Restoration::default();
        let result = device.session(&mut |link| {
            let state = read_state(link)?;
            restoration = restore(&info, &state, mixer);
            connected = Some(if restoration.writes.is_empty() {
                state
            } else {
                write_params(link, &restoration.writes)?;
                read_state(link)?
            });
            Ok(())
        });
        match (result, connected) {
            (Ok(()), Some(state)) => {
                if let Some(sls) = restoration.mixer.filter(|_| mixer) {
                    for property in sls.all_gains().properties() {
                        if let Err(error) = device.mixer_set(&property) {
                            self.report(tr!("The mixer is not responding: {}", error));
                            break;
                        }
                    }
                }
                let live = device.is_live();
                let now = Instant::now();
                self.device = Some(device);
                self.state = Some(state);
                self.rate = None;
                self.asio = None;
                self.metering = false;
                self.status = None;
                self.last_error = None;
                self.failures = 0;
                self.next_meters = now;
                self.next_state = now + self.pace.state();
                self.next_rate = now;
                self.next_presence = now + PRESENCE_PERIOD;
                self.emit(Event::Connected {
                    info,
                    state,
                    live,
                    mixer,
                });
            }
            (Err(error), _) => self.report(tr!("MiniFuse 1 is not responding: {}", error)),
            (Ok(()), None) => {}
        }
    }

    fn disconnect(&mut self) {
        if self.device.take().is_some() {
            self.state = None;
            self.rate = None;
            self.asio = None;
            self.metering = false;
            self.pending.clear();
            self.apply_at = None;
            self.next_scan = Instant::now() + SCAN_PERIOD;
            self.emit(Event::Disconnected);
        }
    }

    fn failed(&mut self, error: super::Error) {
        self.failures += 1;
        if error.gone || self.failures >= MAX_FAILURES {
            self.disconnect();
        } else {
            self.report(tr!("MiniFuse 1 is not responding: {}", error));
        }
    }

    fn poll_meters(&mut self) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let mut meters = None;
        let result = device.session(&mut |link| {
            meters = Some(read_meters(link)?);
            Ok(())
        });
        let mixer = if self.pace == Pace::Visible {
            self.poll_mixer_meters()
        } else {
            self.switch_metering(false);
            None
        };
        match (result, meters) {
            (Ok(()), Some(meters)) => {
                self.failures = 0;
                let now = Instant::now();
                let quiet = mixer.as_ref().is_none_or(sls::Meters::is_silent);
                if meters.iter().any(|&m| m > 0.0) || !quiet {
                    self.silent_since = None;
                } else {
                    let since = *self.silent_since.get_or_insert(now);
                    // A hidden window still needs its tick to notice it is back.
                    if self.pace == Pace::Visible && now.duration_since(since) > SILENCE_HOLD {
                        return;
                    }
                }
                if let Some(mixer) = mixer {
                    self.emit(Event::MixerMeters(mixer));
                }
                self.emit(Event::Meters(meters));
            }
            (Err(error), _) => self.failed(error),
            (Ok(()), None) => {}
        }
    }

    /// The driver only measures the software mixer while switched on, so
    /// metering follows the window.
    fn switch_metering(&mut self, on: bool) {
        if self.metering == on || !self.has_mixer() {
            return;
        }
        let Some(device) = self.device.as_mut() else {
            return;
        };
        match device.mixer_set(&sls::meters_switch(on)) {
            Ok(()) => self.metering = on,
            Err(error) => self.report(tr!("The mixer meters are not available: {}", error)),
        }
    }

    fn poll_mixer_meters(&mut self) -> Option<sls::Meters> {
        if !self.has_mixer() {
            return None;
        }
        self.switch_metering(true);
        if !self.metering {
            return None;
        }
        let mut reply = sls::meters_query();
        let device = self.device.as_mut()?;
        device.mixer_get(&mut reply).ok()?;
        sls::parse_meters(&reply)
    }

    fn poll_state(&mut self) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let mut state = None;
        let result = device.session(&mut |link| {
            state = Some(read_state(link)?);
            Ok(())
        });
        match (result, state) {
            (Ok(()), Some(state)) => {
                self.failures = 0;
                self.publish_state(state);
            }
            (Err(error), _) => self.failed(error),
            (Ok(()), None) => {}
        }
    }

    fn publish_state(&mut self, state: RawState) {
        if self.state != Some(state) {
            self.state = Some(state);
            self.emit(Event::State(state));
        }
    }

    fn poll_rate(&mut self) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let rate = device.sample_rate();
        if rate != self.rate {
            self.rate = rate.clone();
            self.emit(Event::SampleRate(rate));
        }
    }

    fn poll_asio(&mut self) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let asio = device.asio();
        if asio != self.asio {
            self.asio = asio.clone();
            self.emit(Event::Asio(asio));
        }
    }

    fn execute(&mut self, batch: Vec<Command>) {
        let mut gains = Gains::default();
        for command in batch {
            match command {
                Command::Mixer(more) => gains.extend(more),
                Command::SetAsio { size, safe_mode } => {
                    if let Some(device) = self.device.as_mut()
                        && let Err(error) = device.set_asio(size, safe_mode)
                    {
                        self.report(tr!("Couldn't change the ASIO settings: {}", error));
                    }
                    self.next_rate = Instant::now() + Duration::from_millis(200);
                }
                Command::Write(writes) => {
                    for (param, channel, value) in writes {
                        match self
                            .pending
                            .iter_mut()
                            .find(|w| w.0 == param && w.1 == channel)
                        {
                            Some(write) => write.2 = value,
                            None => self.pending.push((param, channel, value)),
                        }
                    }
                    let delay = if self.live() {
                        Duration::ZERO
                    } else {
                        APPLY_DELAY
                    };
                    self.apply_at = Some(Instant::now() + delay);
                }
                Command::SetSampleRate(rate) => self.set_rate(rate),
                Command::LeaveStorageMode => {
                    if let Err(error) = self.bus.leave_storage_mode() {
                        self.report(tr!("Couldn't switch to audio mode: {}", error));
                    }
                    self.next_scan = Instant::now() + Duration::from_secs(2);
                }
                Command::Pace(pace) => {
                    self.pace = pace;
                    self.next_meters = Instant::now();
                }
                Command::Rescan => {
                    self.disconnect();
                    self.next_scan = Instant::now();
                }
            }
        }
        if !gains.is_empty() {
            self.apply_mixer(&gains);
        }
    }

    fn apply_mixer(&mut self, gains: &Gains) {
        let Some(device) = self.device.as_mut().filter(|device| device.has_mixer()) else {
            return;
        };
        let result = gains
            .properties()
            .iter()
            .try_for_each(|property| device.mixer_set(property));
        match result {
            Ok(()) => self.failures = 0,
            Err(error) => self.failed(error),
        }
    }

    fn apply(&mut self) {
        self.apply_at = None;
        let writes = std::mem::take(&mut self.pending);
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let mut state = None;
        let result = device.session(&mut |link| {
            write_params(link, &writes)?;
            state = Some(read_state(link)?);
            Ok(())
        });
        match (result, state) {
            (Ok(()), Some(state)) => {
                self.failures = 0;
                self.publish_state(state);
            }
            (Err(error), _) => self.failed(error),
            (Ok(()), None) => {}
        }
    }

    fn set_rate(&mut self, rate: u32) {
        let Some(device) = self.device.as_mut() else {
            return;
        };
        let result = device.set_sample_rate(rate);
        if let Err(error) = result {
            self.report(tr!("Couldn't change the sample rate: {}", error));
        }
        self.next_rate = Instant::now() + Duration::from_millis(200);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::demo::DemoBus;
    use crate::device::protocol::{Mix, MixControl, Param};

    fn next(events: &mpsc::Receiver<Event>, want: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = events
                .recv_timeout(left)
                .expect("the expected event did not come");
            if want(&event) {
                return event;
            }
        }
    }

    #[test]
    fn connects_restores_and_writes() {
        let saved = Mix {
            enabled: true,
            output_db: -12.0,
            ..Mix::default()
        };
        let restore: Restore = Box::new(move |_, state, _| Restoration {
            writes: if saved.approx_eq(&state.mix()) {
                Vec::new()
            } else {
                saved.all_writes()
            },
            mixer: None,
        });
        let (tx, events) = mpsc::channel();
        let commands = spawn(
            Box::new(DemoBus::new(false)),
            restore,
            Pace::Visible,
            move |e| tx.send(e).is_ok(),
        );

        let Event::Connected {
            state, live, mixer, ..
        } = next(&events, |e| matches!(e, Event::Connected { .. }))
        else {
            unreachable!()
        };
        assert!(live);
        assert!(!mixer);
        assert!(
            saved.approx_eq(&state.mix()),
            "restored mix: {:?}",
            state.mix()
        );

        let mut mix = state.mix();
        let writes = mix.apply(MixControl::Usb12Mute(true));
        commands.send(Command::Write(writes)).unwrap();
        commands
            .send(Command::Write(vec![(Param::Input48V, 0, 1)]))
            .unwrap();
        next(
            &events,
            |e| matches!(e, Event::State(state) if state.mix().usb12.muted && state.inputs().phantom),
        );
    }

    #[test]
    fn loads_the_software_mixer_and_meters_it() {
        let restore: Restore = Box::new(|_, _, mixer| Restoration {
            writes: Vec::new(),
            mixer: mixer.then(Sls::default),
        });
        let (tx, events) = mpsc::channel();
        let commands = spawn(
            Box::new(DemoBus::new(true)),
            restore,
            Pace::Visible,
            move |e| tx.send(e).is_ok(),
        );
        let Event::Connected { mixer, .. } =
            next(&events, |e| matches!(e, Event::Connected { .. }))
        else {
            unreachable!()
        };
        assert!(mixer);
        // The factory mix: Main 1/2 on Monitor & Phones, Virtual 5/6 on
        // Loopback Mix 5/6.
        next(
            &events,
            |e| matches!(e, Event::MixerMeters(m) if m.mixes[0][0] > -40.0 && m.mixes[2][0] > -40.0),
        );
        let mut sls = Sls::default();
        let gains = sls.apply(sls::SlsControl::Mute {
            mix: 2,
            source: 3,
            muted: true,
        });
        commands.send(Command::Mixer(gains)).unwrap();
        next(
            &events,
            |e| matches!(e, Event::MixerMeters(m) if m.mixes[2][0] < -100.0),
        );
        // The demo driver starts at 128 samples in safe mode.
        commands
            .send(Command::SetAsio {
                size: 64,
                safe_mode: false,
            })
            .unwrap();
        next(
            &events,
            |e| matches!(e, Event::Asio(Some(a)) if a.size == 64 && !a.safe_mode),
        );
    }
}
