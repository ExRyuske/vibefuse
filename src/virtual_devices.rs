//! The virtual devices of the software mixer: the audio server plug-in on
//! macOS, a PipeWire mixer on Linux, set up from here and connected to the
//! card by the [`bridge`]. On Windows Arturia's driver has them.
//!
//! Looking them up asks the audio server, which can hang, so the window
//! does it on a thread of its own ([`state_in_background`]).

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::tr;

/// The error of an install the user cancelled at the password prompt.
pub const CANCELED: &str = "canceled";

/// Runs `f` on a thread of its own and gives up waiting after `limit`; the
/// thread is left behind if it hangs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("vibefuse-audio-server".into())
        .spawn(move || {
            let _ = sender.send(f());
        })
        .ok()?;
    receiver.recv_timeout(limit).ok()
}

/// Calls `done` with the state from another thread, or with
/// [`State::Unresponsive`] if the audio server has not answered in time
/// (and once more with the state if it answers later).
pub fn state_in_background(done: impl Fn(State) + Send + Sync + 'static) {
    let done = std::sync::Arc::new(done);
    let late = done.clone();
    let _ = thread::Builder::new()
        .name("vibefuse-devices".into())
        .spawn(move || {
            let (sender, receiver) = mpsc::channel();
            let _ = thread::Builder::new()
                .name("vibefuse-audio-server".into())
                .spawn(move || {
                    let _ = sender.send(state());
                });
            match receiver.recv_timeout(ANSWER_TIMEOUT) {
                Ok(state) => done(state),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    done(State::Unresponsive);
                    if let Ok(state) = receiver.recv() {
                        late(state);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        });
}

/// How long the audio server gets to answer before it counts as hung.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
pub enum State {
    /// This system has no use for them (Windows).
    Unsupported,
    Missing,
    /// An older version is installed.
    Outdated,
    /// Installed, but the audio system has not loaded them (yet).
    NotLoaded,
    Ready,
    /// Something they need is missing, e.g. PipeWire.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Unavailable(String),
    /// Not looked up yet.
    Checking,
    /// The audio server does not answer.
    Unresponsive,
}

impl State {
    /// What the settings page says, the install button's text (empty for
    /// none) and whether they can be removed.
    pub fn describe(&self) -> (String, String, bool) {
        match self {
            State::Unsupported => (String::new(), String::new(), false),
            State::Missing => (
                tr!("Extra playback and recording devices for apps."),
                tr!("Install…"),
                false,
            ),
            State::Outdated => (tr!("An older version is installed."), tr!("Update…"), true),
            State::NotLoaded => (
                tr!("Installed, but the system has not loaded them."),
                tr!("Reinstall…"),
                true,
            ),
            State::Ready => (
                tr!("Installed. In apps, choose the devices named “VibeFuse”."),
                String::new(),
                true,
            ),
            State::Unavailable(reason) => (reason.clone(), String::new(), false),
            State::Checking => (String::new(), String::new(), false),
            // Installing restarts the audio server, which may cure it.
            State::Unresponsive => (unresponsive(), tr!("Reinstall…"), false),
        }
    }
}

/// What to tell when the audio server hangs.
pub fn unresponsive() -> String {
    tr!("The system's audio service is not responding. Restart your Mac if this lasts.")
}

pub use platform::{install, remove, state};

#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{ANSWER_TIMEOUT, CANCELED, State, unresponsive, within};
    use crate::device::hal_mixer;
    use crate::tr;

    const INSTALLED: &str = "/Library/Audio/Plug-Ins/HAL/VibeFuseMixer.driver";
    const BINARY: &[u8] = include_bytes!(concat!(
        env!("OUT_DIR"),
        "/VibeFuseMixer.driver/Contents/MacOS/VibeFuseMixer"
    ));
    const INFO: &[u8] = include_bytes!(concat!(
        env!("OUT_DIR"),
        "/VibeFuseMixer.driver/Contents/Info.plist"
    ));
    const SIGNATURE: &[u8] = include_bytes!(concat!(
        env!("OUT_DIR"),
        "/VibeFuseMixer.driver/Contents/_CodeSignature/CodeResources"
    ));
    /// The bundle's files, relative to `Contents`.
    const FILES: [(&str, &[u8]); 3] = [
        ("MacOS/VibeFuseMixer", BINARY),
        ("Info.plist", INFO),
        ("_CodeSignature/CodeResources", SIGNATURE),
    ];
    /// How long the restarted audio server may take to load the plug-in.
    const LOAD_TIMEOUT: Duration = Duration::from_secs(20);

    pub fn state() -> State {
        let contents = Path::new(INSTALLED).join("Contents");
        if !contents.exists() {
            return State::Missing;
        }
        let current = FILES
            .iter()
            .all(|(path, bytes)| std::fs::read(contents.join(path)).is_ok_and(|b| b == *bytes));
        if !current {
            State::Outdated
        } else if hal_mixer::find().is_some() {
            State::Ready
        } else {
            State::NotLoaded
        }
    }

    /// Writes the bundle where the administrator's copy picks it up.
    fn stage() -> Result<PathBuf, String> {
        let dir = dirs::cache_dir()
            .ok_or("no cache folder")?
            .join("VibeFuse")
            .join("VibeFuseMixer.driver");
        let _ = std::fs::remove_dir_all(&dir);
        for (path, bytes) in FILES {
            let path = dir.join("Contents").join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        }
        use std::os::unix::fs::PermissionsExt;
        let binary = dir.join("Contents").join(FILES[0].0);
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        Ok(dir)
    }

    fn shell_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', r"'\''"))
    }

    /// Runs a shell script as root; macOS asks for the password.
    fn as_administrator(script: &str, prompt: &str) -> Result<(), String> {
        let escape = |s: &str| s.replace('\\', r"\\").replace('"', "\\\"");
        let source = format!(
            "do shell script \"{}\" with prompt \"{}\" with administrator privileges",
            escape(script),
            escape(prompt)
        );
        let output = Command::new("osascript")
            .args(["-e", &source])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("osascript: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let reason = String::from_utf8_lossy(&output.stderr);
        // -128: the user pressed Cancel.
        if reason.contains("-128") {
            Err(CANCELED.to_owned())
        } else {
            Err(reason.trim().to_owned())
        }
    }

    /// Restarting the audio server makes it load or forget the plug-in;
    /// launchd starts it again at once. The AirPlay helper goes with it:
    /// it registers its devices again with every new audio server without
    /// dropping the old ones, and a few restarts in a row pile up enough of
    /// them to hang the server. A fresh helper starts from none.
    /// killall fails if the helper is not running, which is fine.
    const RESTART_AUDIO: &str = "( /usr/bin/killall -9 coreaudiod AirPlayXPCHelper || true )";

    pub fn install() -> Result<(), String> {
        let staged = stage()?;
        let script = format!(
            "/bin/rm -rf {target} && /usr/bin/ditto {source} {target} && /usr/sbin/chown -R root:wheel {target} && {RESTART_AUDIO}",
            source = shell_quote(&staged.to_string_lossy()),
            target = shell_quote(INSTALLED),
        );
        as_administrator(
            &script,
            &tr!("VibeFuse wants to install its virtual outputs."),
        )?;
        let _ = std::fs::remove_dir_all(staged);
        let loaded = within(LOAD_TIMEOUT + ANSWER_TIMEOUT, || {
            let start = Instant::now();
            while start.elapsed() < LOAD_TIMEOUT {
                if hal_mixer::find().is_some() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            false
        });
        match loaded {
            Some(true) => Ok(()),
            Some(false) => Err(tr!("the system did not load the virtual outputs")),
            None => Err(unresponsive()),
        }
    }

    pub fn remove() -> Result<(), String> {
        let script = format!("/bin/rm -rf {} && {RESTART_AUDIO}", shell_quote(INSTALLED));
        as_administrator(
            &script,
            &tr!("VibeFuse wants to remove its virtual outputs."),
        )
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::State;
    use crate::device::pipewire;

    pub fn state() -> State {
        pipewire::state()
    }

    pub fn install() -> Result<(), String> {
        pipewire::install()
    }

    pub fn remove() -> Result<(), String> {
        pipewire::remove()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::State;

    pub fn state() -> State {
        State::Unsupported
    }

    pub fn install() -> Result<(), String> {
        Err("Arturia's driver has the virtual outputs".to_owned())
    }

    pub fn remove() -> Result<(), String> {
        install()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod bridge {
    //! Connects the software mixer to the MiniFuse (macOS, Linux).
    //!
    //! macOS: the plug-in lives inside the audio server and cannot reach the
    //! card, so this app runs both in a private aggregate device: the card's
    //! inputs go to the plug-in's hidden bridge device, and the "Monitor &
    //! Phones" mix the plug-in returns goes to the card's outputs, on top of
    //! what programs play there directly. It runs while the card is connected
    //! and some program uses a virtual device or the card's input is heard in
    //! that mix; everything is driven by CoreAudio notifications, so it costs
    //! nothing otherwise.
    //!
    //! Linux: PipeWire mixes by itself; this keeps the mixer linked to the
    //! card's ports whenever they appear (see [`crate::device::pipewire`]).
    //!
    //! Only the running instance of the app (see [`crate::desktop::instance`])
    //! starts it.

    use std::thread;

    /// Starts the bridge thread. `on_restart` runs on it when the audio server
    /// came back from a restart (macOS), which loses the mixer's settings.
    pub fn spawn(on_restart: impl Fn() + Send + 'static) {
        let _ = thread::Builder::new()
            .name("vibefuse-bridge".into())
            .spawn(move || {
                // A bug here stops the bridge, not the app; it starts again.
                loop {
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        #[cfg(target_os = "macos")]
                        mac::run(&on_restart);
                        #[cfg(target_os = "linux")]
                        {
                            let _ = &on_restart;
                            crate::device::pipewire::keep_links();
                        }
                    }));
                    if run.is_ok() {
                        return;
                    }
                    thread::sleep(std::time::Duration::from_secs(1));
                }
            });
    }

    /// Keeps the bridge running while the card's input is heard in the
    /// "Monitor & Phones" mix (macOS): with no program playing, nothing else
    /// would carry the input to the outputs.
    #[cfg(target_os = "macos")]
    pub fn monitor_input(on: bool) {
        mac::monitor_input(on);
    }

    #[cfg(target_os = "macos")]
    mod mac {
        use std::ffi::c_void;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
        use std::thread;
        use std::time::{Duration, Instant};

        use crate::device::coreaudio::{
            self, AudioBufferList, AudioObjectId, Cf, HARDWARE_DEVICES, IoProcId, NOMINAL_RATE,
            OsStatus, PLUGIN_LIST, PropertyAddress, SCOPE_INPUT, SCOPE_OUTPUT, SERVICE_RESTARTED,
            SYSTEM_OBJECT,
        };
        use crate::device::hal_mixer::{self, BRIDGE_UID, BUNDLE_ID, PROPERTY_RUNNING};

        const AGGREGATE_UID: &str = "cc.yeru.vibefuse.bridge";
        /// 5.3 ms at 48 kHz: short enough for monitoring, few enough wake-ups.
        const BUFFER_FRAMES: u32 = 256;
        /// Medium quality of the resampler that follows the card's clock.
        const DRIFT_QUALITY: i32 = 0x40;
        /// Pause before the bridge stops once nobody uses a virtual device, so a
        /// program skipping between tracks does not restart it each time.
        const STOP_DELAY: Duration = Duration::from_secs(5);

        static MONITOR_INPUT: AtomicBool = AtomicBool::new(false);
        /// Wakes the running bridge.
        static WAKE: Mutex<Option<Sender<bool>>> = Mutex::new(None);

        pub fn monitor_input(on: bool) {
            if MONITOR_INPUT.swap(on, Ordering::Relaxed) != on
                && let Some(wake) = WAKE.lock().ok().as_deref().and_then(Option::as_ref)
            {
                let _ = wake.send(false);
            }
        }

        pub fn run(on_restart: &impl Fn()) {
            let (sender, events) = mpsc::channel();
            if let Ok(mut wake) = WAKE.lock() {
                *wake = Some(sender.clone());
            }
            // Lives as long as the process: the listeners keep pointing at it.
            let sender: &'static Sender<bool> = Box::leak(Box::new(sender));
            let client = (sender as *const Sender<bool>).cast_mut().cast::<c_void>();
            for selector in [HARDWARE_DEVICES, PLUGIN_LIST, SERVICE_RESTARTED] {
                let _ = coreaudio::add_listener(
                    SYSTEM_OBJECT,
                    coreaudio::address(selector),
                    changed,
                    client,
                );
            }
            Bridge::new(client).serve(&events, on_restart);
        }

        /// Tells the bridge something changed, and whether the server restarted.
        unsafe extern "C" fn changed(
            _object: AudioObjectId,
            count: u32,
            addresses: *const PropertyAddress,
            client: *mut c_void,
        ) -> OsStatus {
            // SAFETY: `client` is the leaked sender of `run`; CoreAudio passes
            // `count` addresses.
            let (sender, addresses) = unsafe {
                (
                    &*client.cast::<Sender<bool>>(),
                    std::slice::from_raw_parts(addresses, count as usize),
                )
            };
            let restarted = addresses.iter().any(|a| a.selector == SERVICE_RESTARTED);
            let _ = sender.send(restarted);
            0
        }

        /// What the bridge needs from the system.
        #[derive(Clone, Copy, PartialEq)]
        struct Wanted {
            card: AudioObjectId,
            bridge: AudioObjectId,
            rate: u64,
        }

        /// Where a channel the IO proc copies is in its buffer list.
        #[derive(Clone, Copy)]
        struct Channel {
            buffer: usize,
            offset: usize,
        }

        fn channel(buffers: &[u32], index: usize) -> Option<Channel> {
            let mut first = 0;
            for (buffer, &channels) in buffers.iter().enumerate() {
                let channels = channels as usize;
                if index < first + channels {
                    return Some(Channel {
                        buffer,
                        offset: index - first,
                    });
                }
                first += channels;
            }
            None
        }

        struct Layout {
            card_inputs: [Option<Channel>; 2],
            card_outputs: [Option<Channel>; 2],
            /// The bridge device's buffers follow the card's.
            bridge_input: usize,
            bridge_output: usize,
        }

        struct Running {
            wanted: Wanted,
            aggregate: AudioObjectId,
            io: IoProcId,
            /// Read by the IO proc until it stops.
            _layout: Box<Layout>,
        }

        struct Bridge {
            client: *mut c_void,
            /// Objects whose properties we listen to: (object, selector).
            listening: Vec<(AudioObjectId, u32)>,
            running: Option<Running>,
            idle_since: Option<Instant>,
        }

        impl Bridge {
            fn new(client: *mut c_void) -> Self {
                Self {
                    client,
                    listening: Vec::new(),
                    running: None,
                    idle_since: None,
                }
            }

            fn serve(&mut self, events: &Receiver<bool>, on_restart: impl Fn()) {
                // Also subscribes to the plug-in and the card, so look once now.
                self.update();
                loop {
                    let timeout = match self.idle_since {
                        Some(since) => STOP_DELAY.saturating_sub(since.elapsed()),
                        None => Duration::from_secs(3600),
                    };
                    match events.recv_timeout(timeout) {
                        Ok(mut restarted) => {
                            // Notifications come in bursts; settle first.
                            thread::sleep(Duration::from_millis(100));
                            restarted |= events.try_iter().any(|r| r);
                            if restarted {
                                self.forget();
                                on_restart();
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    self.update();
                }
            }

            /// The audio server restarted: the devices and listeners of before
            /// are gone, and their ids may now name other objects, so they are
            /// dropped without a call.
            fn forget(&mut self) {
                if let Some(running) = self.running.take() {
                    // The IO thread went with the server; leaking keeps it safe
                    // in case it did not.
                    std::mem::forget(running._layout);
                }
                self.listening.clear();
                self.idle_since = None;
            }

            /// Listens to exactly these properties.
            fn listen(&mut self, wanted: Vec<(AudioObjectId, u32)>) {
                for &(object, selector) in &self.listening {
                    if !wanted.contains(&(object, selector)) {
                        coreaudio::remove_listener(
                            object,
                            coreaudio::address(selector),
                            changed,
                            self.client,
                        );
                    }
                }
                for &(object, selector) in &wanted {
                    if !self.listening.contains(&(object, selector)) {
                        let _ = coreaudio::add_listener(
                            object,
                            coreaudio::address(selector),
                            changed,
                            self.client,
                        );
                    }
                }
                self.listening = wanted;
            }

            fn wanted(&mut self) -> Option<Wanted> {
                let plugin = coreaudio::plugin(BUNDLE_ID);
                let card = coreaudio::find_device("");
                let mut listen = Vec::new();
                listen.extend(plugin.map(|p| (p, PROPERTY_RUNNING)));
                listen.extend(card.map(|c| (c, NOMINAL_RATE)));
                self.listen(listen);
                let status = hal_mixer::status(plugin?).ok()?;
                let needed = status.running > 0 || MONITOR_INPUT.load(Ordering::Relaxed);
                if status.version != hal_mixer::VERSION || !needed {
                    return None;
                }
                let card = card?;
                Some(Wanted {
                    card,
                    bridge: coreaudio::device_by_uid(BRIDGE_UID)?,
                    rate: coreaudio::nominal_rate(card)? as u64,
                })
            }

            fn update(&mut self) {
                // A restarted server took the aggregate device with it, even if
                // its notification got lost.
                if self.running.as_ref().is_some_and(|r| {
                    coreaudio::device_uid(r.aggregate).as_deref() != Some(AGGREGATE_UID)
                }) {
                    self.forget();
                }
                let wanted = self.wanted();
                let current = self.running.as_ref().map(|r| r.wanted);
                if wanted == current {
                    self.idle_since = None;
                    return;
                }
                if wanted.is_none() && current.is_some() {
                    // Nobody listens: stop after a pause.
                    let since = *self.idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() < STOP_DELAY {
                        return;
                    }
                }
                self.idle_since = None;
                self.stop();
                if let Some(wanted) = wanted {
                    match start(wanted) {
                        Ok(running) => self.running = Some(running),
                        Err(error) => {
                            eprintln!("the bridge to the MiniFuse did not start: {error}")
                        }
                    }
                }
            }

            fn stop(&mut self) {
                if let Some(running) = self.running.take() {
                    coreaudio::stop_io(running.aggregate, running.io);
                    coreaudio::destroy_aggregate(running.aggregate);
                }
            }
        }

        impl Drop for Bridge {
            /// The IO proc reads the layout until it is stopped, also when a
            /// panic ends the bridge.
            fn drop(&mut self) {
                self.stop();
            }
        }

        fn start(wanted: Wanted) -> Result<Running, String> {
            let card_uid = coreaudio::device_uid(wanted.card).ok_or("the MiniFuse has no UID")?;
            // Everything in the plug-in runs at the card's rate.
            let _ = coreaudio::set_nominal_rate(wanted.bridge, wanted.rate as f64);
            let card = Cf::string(&card_uid);
            let description = Cf::dictionary(&[
                ("uid", &Cf::string(AGGREGATE_UID)),
                ("name", &Cf::string("VibeFuse Bridge")),
                ("private", &Cf::number(1)),
                ("stacked", &Cf::number(0)),
                ("master", &card),
                ("clock", &card),
                (
                    "subdevices",
                    &Cf::array(&[
                        &Cf::dictionary(&[("uid", &card)]),
                        &Cf::dictionary(&[
                            ("uid", &Cf::string(BRIDGE_UID)),
                            ("drift", &Cf::number(1)),
                            ("drift quality", &Cf::number(DRIFT_QUALITY)),
                        ]),
                    ]),
                ),
            ]);
            let aggregate = coreaudio::create_aggregate(&description)?;
            let card_in = coreaudio::stream_channels(wanted.card, SCOPE_INPUT);
            let card_out = coreaudio::stream_channels(wanted.card, SCOPE_OUTPUT);
            let layout = Box::new(Layout {
                card_inputs: [channel(&card_in, 0), channel(&card_in, 1)],
                card_outputs: [channel(&card_out, 0), channel(&card_out, 1)],
                bridge_input: card_in.len(),
                bridge_output: card_out.len(),
            });
            let _ = coreaudio::set_buffer_frames(aggregate, BUFFER_FRAMES);
            let client = (&raw const *layout).cast_mut().cast::<c_void>();
            match coreaudio::start_io(aggregate, io, client) {
                Ok(io) => Ok(Running {
                    wanted,
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

        /// Samples of one channel in a buffer list.
        struct Samples {
            data: *mut f32,
            stride: usize,
            frames: usize,
        }

        impl Samples {
            fn of(buffers: &[coreaudio::AudioBuffer], channel: Option<Channel>) -> Option<Self> {
                let channel = channel?;
                let buffer = buffers.get(channel.buffer)?;
                if buffer.data.is_null() || buffer.channels == 0 {
                    return None;
                }
                let stride = buffer.channels as usize;
                if channel.offset >= stride {
                    return None;
                }
                Some(Self {
                    // SAFETY: the offset is within one frame of the buffer.
                    data: unsafe { buffer.data.cast::<f32>().add(channel.offset) },
                    stride,
                    frames: buffer.bytes as usize / 4 / stride,
                })
            }

            fn interleaved(
                buffers: &[coreaudio::AudioBuffer],
                index: usize,
                offset: usize,
            ) -> Option<Self> {
                Self::of(
                    buffers,
                    Some(Channel {
                        buffer: index,
                        offset,
                    }),
                )
            }
        }

        /// Runs on the aggregate's IO thread: copies samples, nothing else.
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
            // IO proc; the buffer lists are CoreAudio's for this cycle.
            let (layout, inputs, outputs) = unsafe {
                (
                    &*client.cast::<Layout>(),
                    AudioBufferList::buffers(input),
                    AudioBufferList::buffers(output),
                )
            };
            for side in 0..2 {
                // Card input → bridge output (the plug-in's hardware inputs).
                if let Some(to) = Samples::interleaved(outputs, layout.bridge_output, side) {
                    let from = Samples::of(inputs, layout.card_inputs[side]);
                    for i in 0..to.frames {
                        let x = match &from {
                            // SAFETY: indices stay within each buffer's frames.
                            Some(from) if i < from.frames => unsafe {
                                *from.data.add(i * from.stride)
                            },
                            _ => 0.0,
                        };
                        // SAFETY: as above.
                        unsafe { *to.data.add(i * to.stride) = x };
                    }
                }
                // Bridge input (the Monitor mix) → card output.
                if let (Some(from), Some(to)) = (
                    Samples::interleaved(inputs, layout.bridge_input, side),
                    Samples::of(outputs, layout.card_outputs[side]),
                ) {
                    for i in 0..to.frames.min(from.frames) {
                        // SAFETY: indices stay within each buffer's frames.
                        unsafe { *to.data.add(i * to.stride) = *from.data.add(i * from.stride) };
                    }
                }
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hung_call_is_given_up_on() {
        assert_eq!(within(Duration::from_secs(5), || 7), Some(7));
        let hung = within(Duration::from_millis(20), || {
            thread::sleep(Duration::from_secs(2));
            7
        });
        assert_eq!(hung, None);
    }
}
