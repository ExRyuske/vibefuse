//! Slint front end.
//!
//! The app lives in the tray; the window is created when it is opened and
//! dropped when it is closed, so a closed window costs no memory for its
//! renderer. The device thread posts events into the UI event loop; the
//! controller below keeps the model and pushes it into the window, if there
//! is one. Nothing animates and unchanged values are not pushed, so the
//! window only draws a frame when something visible changed, and an idle
//! panel costs nothing.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::desktop::{self, autostart, instance};
use crate::device::demo::DemoBus;
use crate::device::protocol::{Inputs, Mix, MixControl, Param, RawState, meter as slot};
use crate::device::sls::{self, Names, Sls, SlsControl};
use crate::device::worker::{self, Command, Event, Pace, Restoration, Restore};
use crate::device::{self, Asio, DeviceInfo, SampleRate};
use crate::firmware::{self, Version};
use crate::i18n::{self, Language};
use crate::settings::presets::{self, Snapshot};
use crate::settings::{self, Accent, FirmwareCheck, Settings};
use crate::tr;
use crate::virtual_devices::{self, State as DevicesState};

slint::include_modules!();

/// How long device readings are ignored for a control after the user moved
/// it, so polling does not fight a drag in progress.
const EDIT_GRACE: Duration = Duration::from_millis(800);
const SAVE_DELAY: Duration = Duration::from_secs(1);
const MESSAGE_TTL: Duration = Duration::from_secs(8);
/// How long the device thread may take for its first report.
const WORKER_PATIENCE: Duration = Duration::from_secs(8);

const STATUS_SEARCHING: i32 = 0;
const STATUS_STORAGE: i32 = 1;
const STATUS_UNAVAILABLE: i32 = 2;
const STATUS_CONNECTED: i32 = 3;

/// The firmware is looked up on Arturia's site at most this often.
const FIRMWARE_CHECK_PERIOD: u64 = 24 * 60 * 60;
/// What [`parse_number`] returns for text that is not a number.
const NOT_A_NUMBER: f32 = -1.0e9;

/// Names of the device's own loopback channels, for the sources they share
/// with the software mixer (Input 1, Main 1/2 = USB 1-2, Virtual 3/4 = USB 3-4).
const LOOPBACK_NAMES: [&str; 3] = ["Input 1", "USB 1-2", "USB 3-4"];
/// The mixes as the tabs name them; the driver's names are longer.
const TAB_NAMES: [&str; sls::MIXES] = ["Monitor & Phones", "Mix 3/4", "Mix 5/6"];

/// A channel name in the current language.
fn channel_name(name: &str) -> String {
    if name == "Input 1" {
        tr!("Input 1")
    } else {
        name.to_owned()
    }
}

/// Whether `text` is just one of the names a source has anyway, in any
/// language.
fn is_default_name(source: usize, text: &str) -> bool {
    let text = text.trim().to_lowercase();
    let mut defaults = vec![sls::SOURCE_NAMES[source].to_owned()];
    defaults.extend(LOOPBACK_NAMES.get(source).map(|&name| name.to_owned()));
    if source == sls::INPUT_1 {
        defaults.extend(["Input 1".to_owned(), "Вход 1".to_owned()]);
    }
    defaults.iter().any(|d| d.to_lowercase() == text)
}

/// How the app starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// With the window open.
    Window,
    /// In the tray only (the login item).
    Tray,
    /// With a simulated device: `windows` the Windows driver with its
    /// software mixer, otherwise the bare device. Runs beside a real copy.
    Demo { windows: bool },
}

/// Runs the app until the user quits it from the tray. Returns at once if
/// another copy runs: that one shows its window instead.
pub fn run(start: Start) -> Result<(), slint::PlatformError> {
    let demo = match start {
        Start::Demo { windows } => Some(windows),
        _ => None,
    };
    let _instance = if demo.is_none() {
        let on_show = || {
            let _ = slint::invoke_from_event_loop(show_window);
        };
        match instance::claim(start == Start::Window, on_show) {
            Some(instance) => Some(instance),
            None => return Ok(()),
        }
    } else {
        None
    };
    // Before the device thread starts: it reports through the event loop,
    // which only exists once there is a backend.
    select_backend(start == Start::Tray)?;

    // Slint draws on the GPU (Skia on Metal on macOS, FemtoVG on OpenGL
    // elsewhere): with moving meters that costs less than half the CPU of the
    // software renderer, which redraws and copies the whole window every
    // frame on macOS. Without a GPU both draw in software by themselves.
    let (bus, restore): (Box<dyn device::Bus>, Restore) = match demo {
        Some(windows) => (
            Box::new(DemoBus::new(windows)),
            Box::new(|_, _, mixer| Restoration {
                writes: Vec::new(),
                mixer: mixer.then(Sls::default),
            }),
        ),
        None => (device::system_bus(), Box::new(settings::restore)),
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if demo.is_none() {
        // The restarted audio server starts the mixer from scratch.
        crate::virtual_devices::bridge::spawn(|| {
            let _ = slint::invoke_from_event_loop(|| {
                with(|c| {
                    c.send(Command::Rescan);
                    c.check_devices();
                });
            });
        });
    }
    let pace = if start == Start::Tray {
        Pace::Background
    } else {
        Pace::Visible
    };
    let commands = worker::spawn(bus, restore, pace, move |event| {
        slint::invoke_from_event_loop(move || {
            with(|c| c.on_event(event));
        })
        .is_ok()
    });
    CONTROLLER.with(|c| *c.borrow_mut() = Some(Controller::new(commands, demo.is_some())));
    // Looked up whenever the window opens.
    with(|c| c.look_up_devices = true);
    // The device thread reports at once unless the audio server hangs it.
    Timer::single_shot(WORKER_PATIENCE, || {
        with(|c| {
            if !c.heard_from_worker {
                c.status = STATUS_UNAVAILABLE;
                c.status_text = device::unresponsive();
                c.push();
            }
        });
    });
    let language = with(|c| c.settings.language).unwrap_or_default();
    i18n::set(language);

    let tray = Tray::new()?;
    // Slint picks the system's language by itself; a choice in the settings
    // wins once a component exists.
    i18n::set(language);
    tray.set_monochrome(cfg!(target_os = "macos"));
    tray.on_open_window(show_window);
    tray.on_hide_window(hide_window);
    tray.on_quit(|| {
        with(Controller::flush);
        let _ = slint::quit_event_loop();
    });
    with(|c| {
        c.tray = Some(tray.clone_strong());
        c.has_tray = desktop::has_tray();
    });
    if start != Start::Tray {
        show_window();
    }
    // AppKit sets up its own Apple Event handlers while launching.
    #[cfg(target_os = "macos")]
    Timer::single_shot(Duration::ZERO, || {
        crate::desktop::dock::on_reopen(show_window)
    });

    slint::run_event_loop_until_quit()?;
    with(Controller::flush);
    drop(tray);
    Ok(())
}

/// macOS: started in the tray, the app comes up without a Dock icon and
/// leaves the focus where it is.
#[cfg(target_os = "macos")]
fn select_backend(hidden: bool) -> Result<(), slint::PlatformError> {
    use slint::winit_030::winit::event_loop::EventLoop;
    use slint::winit_030::winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

    let mut builder: slint::winit_030::EventLoopBuilder = EventLoop::with_user_event();
    if hidden {
        builder
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_activate_ignoring_other_apps(false);
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(builder)
        .select()
}

#[cfg(not(target_os = "macos"))]
fn select_backend(_hidden: bool) -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new().select()
}

/// Opens the window, or brings it to the front.
fn show_window() {
    let existing = with(|c| c.app.as_ref().map(ComponentHandle::clone_strong)).flatten();
    let app = match existing {
        Some(app) => app,
        None => {
            let app = match App::new() {
                Ok(app) => app,
                Err(error) => {
                    eprintln!("the window did not open: {error}");
                    return;
                }
            };
            connect(&app);
            with(|c| c.attach(app.clone_strong()));
            app
        }
    };
    if let Err(error) = app.show() {
        eprintln!("the window did not open: {error}");
    }
    #[cfg(target_os = "macos")]
    crate::desktop::dock::show_in_dock(true);
}

/// Hides the window from the tray menu, as if it was closed.
fn hide_window() {
    if let Some(Some(app)) = with(|c| c.app.as_ref().map(ComponentHandle::clone_strong)) {
        let _ = app.hide();
    }
    close_window();
}

/// The window was closed: the app goes on in the tray, and the window is
/// dropped once Slint is done closing it.
fn close_window() {
    // Nothing would bring the window back: quit instead.
    if with(|c| !c.has_tray).unwrap_or(false) {
        with(Controller::flush);
        let _ = slint::quit_event_loop();
        return;
    }
    if let Some(Some(app)) = with(Controller::detach) {
        Timer::single_shot(Duration::ZERO, move || drop(app));
    }
    #[cfg(target_os = "macos")]
    crate::desktop::dock::show_in_dock(false);
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

/// Runs `f` on the controller. Never hold this across a file dialog: some
/// platforms run the event loop inside it.
fn with<R>(f: impl FnOnce(&mut Controller) -> R) -> Option<R> {
    CONTROLLER.with(|c| c.borrow_mut().as_mut().map(f))
}

/// Reads a number the way people type it: `-6,5`, `−6`, `-6 dB`.
fn parse_number(text: &str) -> f32 {
    let text = text
        .trim()
        .trim_end_matches(|c: char| c.is_alphabetic() || c.is_whitespace())
        .replace(',', ".")
        .replace('\u{2212}', "-");
    match text.parse::<f32>() {
        Ok(value) if value.is_finite() => value,
        _ => NOT_A_NUMBER,
    }
}

/// Wires a freshly created window to the controller.
fn connect(app: &App) {
    app.global::<Numbers>().on_parse(|text| parse_number(&text));
    let names: Vec<SharedString> = TAB_NAMES.iter().map(|&n| n.into()).collect();
    app.set_mix_names(ModelRc::new(VecModel::from(names)));
    if let Some(sources) = with(|c| c.sources.clone()) {
        app.set_sources(ModelRc::from(sources));
    }
    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.window().on_close_requested(|| {
        close_window();
        slint::CloseRequestResponse::HideWindow
    });

    let handler = |f: fn(&mut Controller)| {
        move || {
            with(f);
        }
    };
    app.on_toggle_inst(handler(|c| {
        c.inputs.inst = !c.inputs.inst;
        c.write_input(Param::InputInst, c.inputs.inst);
    }));
    app.on_toggle_phantom(handler(|c| {
        c.inputs.phantom = !c.inputs.phantom;
        c.write_input(Param::Input48V, c.inputs.phantom);
    }));
    app.on_toggle_direct(handler(|c| {
        c.inputs.direct_monitor = !c.inputs.direct_monitor;
        c.write_input(Param::DirectMonitor, c.inputs.direct_monitor);
    }));
    app.on_toggle_loopback(handler(|c| {
        c.mix_control(MixControl::Enabled(!c.mix.enabled))
    }));
    app.on_toggle_to_usb12(handler(|c| {
        c.mix_control(MixControl::ToUsb12(!c.mix.to_usb12))
    }));
    app.on_toggle_to_outputs(handler(|c| {
        c.mix_control(MixControl::ToOutputs(!c.mix.to_outputs))
    }));
    app.on_leave_storage(handler(|c| c.send(Command::LeaveStorageMode)));
    app.on_check_firmware(handler(Controller::check_firmware));
    app.on_toggle_autostart(handler(Controller::toggle_autostart));
    app.on_install_devices(handler(|c| c.change_devices(true)));
    app.on_remove_devices(handler(|c| c.change_devices(false)));
    app.on_open_firmware_page(handler(|c| {
        if let Err(error) = firmware::open_page() {
            c.fail(error);
        }
    }));
    app.on_toggle_asio_safe(handler(|c| {
        if let Some(asio) = c.asio.clone() {
            c.set_asio(asio.size, !asio.safe_mode);
        }
    }));

    app.on_asio_size_selected(|index| {
        with(|c| {
            let asio = c.asio.clone();
            if let Some((asio, &size)) = asio
                .as_ref()
                .and_then(|a| Some((a, a.sizes.get(index_of(index)?)?)))
            {
                c.set_asio(size, asio.safe_mode);
            }
        });
    });
    app.on_mix_selected(|index| {
        with(|c| {
            if let Some(index) = index_of(index).filter(|&i| i < sls::MIXES) {
                c.mix_tab = index;
            }
            c.push();
        });
    });
    app.on_source_level(|source, db| {
        with(|c| {
            if let Some(source) = source_of(source) {
                let mix = c.mix_tab;
                c.sls_control(SlsControl::Level { mix, source, db });
            }
        });
    });
    app.on_source_pan(|source, pan| {
        with(|c| {
            if let Some(source) = source_of(source) {
                let mix = c.mix_tab;
                c.sls_control(SlsControl::Pan { mix, source, pan });
            }
        });
    });
    app.on_source_mono(|source| {
        with(|c| {
            if let Some(source) = source_of(source) {
                let mix = c.mix_tab;
                let mono = !c.sls.mixes[mix].sources[source].mono;
                c.sls_control(SlsControl::Mono { mix, source, mono });
            }
        });
    });
    app.on_source_mute(|source| {
        with(|c| {
            if let Some(source) = source_of(source) {
                let mix = c.mix_tab;
                let muted = !c.sls.mixes[mix].sources[source].muted;
                c.sls_control(SlsControl::Mute { mix, source, muted });
            }
        });
    });
    app.on_renamed(|source, text| {
        with(|c| {
            if let Some(source) = source_of(source) {
                c.rename(source, &text);
            }
        });
    });
    app.on_mix_level_changed(|db| {
        with(|c| {
            let mix = c.mix_tab;
            c.sls_control(SlsControl::Output { mix, db });
        });
    });
    app.on_level_changed(|strip, db| {
        let control = match strip {
            0 => MixControl::Input1Level(db),
            1 => MixControl::Usb12Level(db),
            2 => MixControl::Usb34Level(db),
            _ => MixControl::OutputLevel(db),
        };
        with(|c| c.mix_control(control));
    });
    app.on_pan_changed(|pan| {
        with(|c| c.mix_control(MixControl::Input1Pan(pan)));
    });
    app.on_mute_clicked(|strip| {
        with(|c| {
            let control = match strip {
                0 => MixControl::Input1Mute(!c.mix.input1.muted),
                1 => MixControl::Usb12Mute(!c.mix.usb12.muted),
                _ => MixControl::Usb34Mute(!c.mix.usb34.muted),
            };
            c.mix_control(control);
        });
    });
    app.on_accent_selected(|index| {
        let Some(index) = index_of(index) else { return };
        let accent = Accent::PRESETS
            .get(index)
            .copied()
            .unwrap_or(Accent::Custom);
        with(|c| {
            c.settings.accent = accent;
            c.schedule_save();
            c.push();
        });
    });
    app.on_accent_hex_edited(|text| {
        with(|c| {
            let color = parse_hex_color(&text);
            // Digits on the way to six are fine while typing.
            let digits = text.trim().trim_start_matches('#');
            let typing = digits.len() < 6 && digits.chars().all(|c| c.is_ascii_hexdigit());
            c.accent_hex_valid = color.is_some() || typing;
            if let Some([r, g, b]) = color {
                c.settings.custom_accent = format!("#{r:02x}{g:02x}{b:02x}");
                c.settings.accent = Accent::Custom;
                c.schedule_save();
            }
            c.push();
        });
    });
    app.on_language_selected(|index| {
        if let Some(&language) = index_of(index).and_then(|i| Language::ALL.get(i)) {
            with(|c| c.set_language(language));
        }
    });
    app.on_rate_selected(|index| {
        with(|c| {
            if let Some(&rate) = index_of(index).and_then(|i| c.rates.get(i)) {
                c.send(Command::SetSampleRate(rate));
            }
            c.push();
        });
    });

    app.on_import_settings(|| {
        let Some(path) = rfd::FileDialog::new()
            .set_title(tr!("Import settings"))
            .add_filter(tr!("MiniFuse preset"), &[presets::EXTENSION])
            .add_filter(tr!("MiniFuse Control Center settings"), &["xml"])
            .pick_file()
        else {
            return;
        };
        with(|c| c.import(path));
    });
    app.on_export_settings(|| {
        let name = with(|c| c.suggested_name()).unwrap_or_default();
        let Some(path) = rfd::FileDialog::new()
            .set_title(tr!("Export settings"))
            .add_filter(tr!("MiniFuse preset"), &[presets::EXTENSION])
            .set_file_name(name)
            .save_file()
        else {
            return;
        };
        with(|c| c.export(path));
    });
}

fn index_of(index: i32) -> Option<usize> {
    usize::try_from(index).ok()
}

fn source_of(index: i32) -> Option<usize> {
    index_of(index).filter(|&i| i < sls::SOURCES)
}

enum FirmwareState {
    Unknown,
    Checking,
    Known(Version),
    Failed(String),
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// Peak meter ballistics: instant attack, steady fall.

const FLOOR_DB: f32 = -60.0;
const FALL_DB_PER_SECOND: f32 = 26.0;
/// Displayed values move in these steps, so a steady signal causes no
/// redraws at all.
const STEP_DB: f32 = 0.5;

#[derive(Debug, Clone, Copy)]
struct Meter {
    level_db: f32,
    updated_at: Instant,
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            level_db: FLOOR_DB,
            updated_at: Instant::now(),
        }
    }
}

fn amplitude_to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        FLOOR_DB
    } else {
        (20.0 * amplitude.log10()).max(FLOOR_DB)
    }
}

impl Meter {
    fn feed(&mut self, amplitude: f32, now: Instant) {
        let db = amplitude_to_db(amplitude);
        let dt = now.duration_since(self.updated_at).as_secs_f32();
        self.updated_at = now;
        self.level_db = if db >= self.level_db {
            db
        } else {
            (self.level_db - FALL_DB_PER_SECOND * dt).max(db)
        };
    }

    /// Level for display, quantized to `STEP_DB`.
    fn display_db(&self) -> f32 {
        (self.level_db / STEP_DB).round() * STEP_DB
    }
}

/// Meter value as shown: the bar ends at -60 dB and moves in 0.5 dB steps,
/// so quiet channels cause no redraws.
fn meter_display(db: f32) -> f32 {
    (db.max(FLOOR_DB) * 2.0).round() / 2.0
}

/// What a source's meter shows: the driver measures it before the mixer, the
/// meter is after the fader of the selected mix, as in OBS. A channel then
/// never shows louder than its knob.
fn after_fader(db: f32, source: &sls::Source) -> f32 {
    meter_display(db + source.level_db)
}

struct Controller {
    /// The window, while it is open.
    app: Option<App>,
    tray: Option<Tray>,
    /// The desktop shows the tray icon.
    has_tray: bool,
    commands: Sender<Command>,
    demo: bool,
    settings: Settings,
    status: i32,
    status_text: String,
    device: Option<DeviceInfo>,
    live: bool,
    /// The input's level is known, also when the device is not live.
    input_meter: bool,
    inputs: Inputs,
    inputs_edited: Option<Instant>,
    mix: Mix,
    mix_edited: Option<Instant>,
    raw: Option<RawState>,
    meters: [Meter; 3],
    meters_paused: bool,
    rate: Option<SampleRate>,
    rates: Vec<u32>,
    /// The Windows driver's software mixer is available.
    mixer: bool,
    sls: Sls,
    mix_tab: usize,
    sls_meters: sls::Meters,
    sources: Rc<VecModel<SourceView>>,
    names: Names,
    autostart: bool,
    devices: DevicesState,
    /// Tells the latest lookup of the virtual devices from older ones.
    devices_lookup: u64,
    /// Off in tests, which set the state themselves.
    look_up_devices: bool,
    devices_busy: bool,
    /// The device thread has reported at least once.
    heard_from_worker: bool,
    /// The last text typed as the accent color was one.
    accent_hex_valid: bool,
    asio: Option<Asio>,
    asio_sizes: Vec<u32>,
    firmware: FirmwareState,
    message: Option<(String, Instant)>,
    unsaved: bool,
    save_timer: Timer,
    /// Takes the message away again.
    message_timer: Timer,
}

fn recent(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|at| now.duration_since(at) < EDIT_GRACE)
}

fn format_rate(rate: u32) -> SharedString {
    if rate.is_multiple_of(1000) {
        tr!("{} kHz", rate / 1000).into()
    } else {
        tr!(
            "{} kHz",
            i18n::decimal(format!("{:.1}", f64::from(rate) / 1000.0))
        )
        .into()
    }
}

/// `#rgb`, `rgb`, `#rrggbb` or `rrggbb` as red, green and blue.
fn parse_hex_color(text: &str) -> Option<[u8; 3]> {
    let text = text.trim();
    let digits: Vec<u8> = text
        .strip_prefix('#')
        .unwrap_or(text)
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    match digits[..] {
        [r, g, b] => Some([r * 17, g * 17, b * 17]),
        [r1, r2, g1, g2, b1, b2] => Some([r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2]),
        _ => None,
    }
}

/// Text on a light accent is dark, on a dark one white.
fn ink_on(rgb: [u8; 3]) -> slint::Color {
    let linear = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2]);
    // Where black and white contrast equally with the color.
    if luminance > 0.179 {
        slint::Color::from_rgb_u8(0x1d, 0x1d, 0x1f)
    } else {
        slint::Color::from_rgb_u8(0xff, 0xff, 0xff)
    }
}

fn pan_label(pan: f32) -> SharedString {
    match ((pan - 0.5) * 200.0).round() as i32 {
        0 => "C".into(),
        v if v < 0 => format!("L{}", -v).into(),
        v => format!("R{v}").into(),
    }
}

impl Controller {
    fn new(commands: Sender<Command>, demo: bool) -> Self {
        Self {
            app: None,
            tray: None,
            has_tray: true,
            commands,
            demo,
            settings: Settings::load(),
            status: STATUS_SEARCHING,
            status_text: String::new(),
            device: None,
            live: true,
            input_meter: true,
            inputs: Inputs::default(),
            inputs_edited: None,
            mix: Mix::default(),
            mix_edited: None,
            raw: None,
            meters: [Meter::default(); 3],
            meters_paused: false,
            rate: None,
            rates: Vec::new(),
            mixer: false,
            sls: Sls::default(),
            mix_tab: 0,
            sls_meters: sls::Meters::default(),
            sources: Rc::new(VecModel::default()),
            names: Names::default(),
            autostart: autostart::is_enabled(),
            devices: DevicesState::Checking,
            devices_lookup: 0,
            look_up_devices: false,
            devices_busy: false,
            heard_from_worker: false,
            accent_hex_valid: true,
            asio: None,
            asio_sizes: Vec::new(),
            firmware: FirmwareState::Unknown,
            message: None,
            unsaved: false,
            save_timer: Timer::default(),
            message_timer: Timer::default(),
        }
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Takes over a freshly opened window.
    fn attach(&mut self, app: App) {
        self.app = Some(app);
        if let Some(tray) = &self.tray {
            tray.set_window_open(true);
        }
        // Models the old window had are pushed to the new one.
        self.rates.clear();
        self.asio_sizes.clear();
        self.meters_paused = false;
        self.send(Command::Pace(Pace::Visible));
        if self.status == STATUS_CONNECTED {
            self.firmware_on_connect();
        }
        self.check_devices();
        self.push();
    }

    /// Lets go of the closed window and follows the device at the slowest
    /// pace. Returns the window, to be dropped once it is closed.
    fn detach(&mut self) -> Option<App> {
        self.flush();
        self.send(Command::Pace(Pace::Background));
        if let Some(tray) = &self.tray {
            tray.set_window_open(false);
        }
        self.app.take()
    }

    fn serial(&self) -> Option<String> {
        self.device.as_ref().map(|info| info.serial.clone())
    }

    fn notify(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
        self.message_timer
            .start(TimerMode::SingleShot, MESSAGE_TTL, || {
                with(|c| {
                    c.message = None;
                    c.push();
                });
            });
    }

    fn on_event(&mut self, event: Event) {
        let now = Instant::now();
        if !self.heard_from_worker && self.status == STATUS_UNAVAILABLE {
            // Was waiting for a hung audio server.
            self.status = STATUS_SEARCHING;
        }
        self.heard_from_worker = true;
        match event {
            Event::Searching => self.status = STATUS_SEARCHING,
            Event::Storage => self.status = STATUS_STORAGE,
            Event::Unavailable(reason) => {
                self.status = STATUS_UNAVAILABLE;
                self.status_text = reason;
            }
            Event::Connected {
                info,
                state,
                live,
                input_meter,
                mixer,
            } => {
                self.status = STATUS_CONNECTED;
                self.live = live;
                self.input_meter = input_meter;
                self.mixer = mixer;
                self.raw = Some(state);
                self.inputs = state.inputs();
                self.mix = state.mix();
                // The same mixer the worker has just loaded into the driver.
                let saved = if self.demo {
                    Snapshot::default()
                } else {
                    self.settings.snapshot_for(&info, &state)
                };
                self.sls = saved.sls;
                self.names = saved.names;
                self.meters = [Meter::default(); 3];
                self.sls_meters = sls::Meters::default();
                self.message = None;
                self.device = Some(info);
                self.remember_mix();
                if self.app.is_some() {
                    self.firmware_on_connect();
                }
            }
            Event::MixerMeters(meters) => {
                self.sls_meters = meters;
                return self.push_meters();
            }
            Event::Asio(asio) => self.asio = asio,
            Event::State(state) => {
                self.raw = Some(state);
                self.sync_from_device(now);
            }
            Event::Meters(values) => {
                let changed = self.sync_from_device(now);
                self.meters[0].feed(values[slot::INPUT_1], now);
                self.meters[1].feed(values[slot::LOOPBACK_LEFT], now);
                self.meters[2].feed(values[slot::LOOPBACK_RIGHT], now);
                // Nobody looks at a minimized window: poll slowly until it is
                // restored.
                let minimized = self
                    .app
                    .as_ref()
                    .is_some_and(|app| app.window().is_minimized());
                if self.app.is_some() && minimized != self.meters_paused {
                    self.meters_paused = minimized;
                    self.send(Command::Pace(if minimized {
                        Pace::Hidden
                    } else {
                        Pace::Visible
                    }));
                }
                if !changed {
                    return self.push_meters();
                }
            }
            Event::SampleRate(rate) => self.rate = rate,
            Event::Disconnected => {
                self.status = STATUS_SEARCHING;
                self.device = None;
                self.raw = None;
                self.rate = None;
                self.asio = None;
                self.flush();
            }
            Event::Error(message) => self.notify(message),
        }
        self.push();
    }

    /// Adopts the device state for controls the user is not touching.
    /// Says whether anything changed.
    fn sync_from_device(&mut self, now: Instant) -> bool {
        let Some(raw) = self.raw else { return false };
        let mut changed = false;
        if !recent(self.inputs_edited, now) {
            changed |= raw.inputs() != self.inputs;
            self.inputs = raw.inputs();
        }
        if !recent(self.mix_edited, now) {
            let mix = raw.mix();
            if !mix.approx_eq(&self.mix) {
                self.mix = mix;
                self.remember_mix();
                changed = true;
            }
        }
        changed
    }

    fn write_input(&mut self, param: Param, on: bool) {
        self.inputs_edited = Some(Instant::now());
        self.send(Command::Write(vec![(param, 0, u16::from(on))]));
        self.push();
    }

    fn mix_control(&mut self, control: MixControl) {
        let writes = self.mix.apply(control);
        self.mix_edited = Some(Instant::now());
        self.send(Command::Write(writes));
        self.remember_mix();
        self.push();
    }

    fn sls_control(&mut self, control: SlsControl) {
        let gains = self.sls.apply(control);
        if self.mixer {
            self.send(Command::Mixer(gains));
        }
        self.remember_sls();
        self.push();
    }

    fn remember_sls(&mut self) {
        let Some(serial) = self.serial() else { return };
        if self.settings.mixers.get(&serial) != Some(&self.sls) {
            self.settings.mixers.insert(serial, self.sls);
            self.schedule_save();
        }
    }

    fn set_asio(&mut self, size: u32, safe_mode: bool) {
        self.send(Command::SetAsio { size, safe_mode });
        if let Some(asio) = &mut self.asio {
            asio.size = size;
            asio.safe_mode = safe_mode;
        }
        self.push();
    }

    fn installed_firmware(&self) -> Option<Version> {
        Version::parse(&self.device.as_ref()?.firmware)
    }

    /// Uses the last check, or asks Arturia again once a day.
    fn firmware_on_connect(&mut self) {
        if matches!(self.firmware, FirmwareState::Checking) {
            return;
        }
        let cached = self.settings.firmware.as_ref().and_then(|check| {
            let fresh = unix_now().saturating_sub(check.checked_at) < FIRMWARE_CHECK_PERIOD;
            fresh.then(|| Version::parse(&check.latest)).flatten()
        });
        match cached {
            Some(latest) => self.firmware = FirmwareState::Known(latest),
            // The demo never goes online by itself.
            None if !self.demo => self.check_firmware(),
            None => {}
        }
    }

    fn check_firmware(&mut self) {
        if matches!(self.firmware, FirmwareState::Checking) {
            return;
        }
        self.firmware = FirmwareState::Checking;
        std::thread::spawn(move || {
            let result = firmware::fetch_latest();
            let _ = slint::invoke_from_event_loop(move || {
                with(|c| c.on_firmware(result));
            });
        });
        self.push();
    }

    fn on_firmware(&mut self, result: Result<Version, String>) {
        match result {
            Ok(latest) => {
                self.firmware = FirmwareState::Known(latest);
                self.settings.firmware = Some(FirmwareCheck {
                    checked_at: unix_now(),
                    latest: latest.to_string(),
                });
                self.schedule_save();
                if self
                    .installed_firmware()
                    .is_some_and(|installed| latest > installed)
                {
                    self.notify(tr!("Firmware {} is available. See Settings.", latest));
                }
            }
            Err(error) => self.firmware = FirmwareState::Failed(error),
        }
        self.push();
    }

    /// Status line of the firmware check and whether an update is out.
    fn firmware_text(&self) -> (String, bool) {
        let installed = self.installed_firmware();
        match (&self.firmware, installed) {
            (FirmwareState::Checking, _) => (tr!("Checking Arturia's website…"), false),
            (FirmwareState::Failed(error), _) => (tr!("Couldn't check: {}", error), false),
            (FirmwareState::Known(latest), Some(installed)) if *latest > installed => (
                tr!(
                    "Version {} is available, you have {}. Update it with Arturia's software.",
                    latest,
                    installed
                ),
                true,
            ),
            (FirmwareState::Known(_), Some(installed)) => {
                (tr!("Version {} is up to date.", installed), false)
            }
            (FirmwareState::Known(latest), None) => (tr!("Latest version: {}.", latest), false),
            (FirmwareState::Unknown, Some(installed)) => (tr!("Version {}.", installed), false),
            (FirmwareState::Unknown, None) => (tr!("Not checked yet."), false),
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            inputs: self.inputs,
            mix: self.mix,
            sls: self.sls,
            names: self.names.clone(),
        }
    }

    fn rename(&mut self, source: usize, text: &str) {
        let text = if is_default_name(source, text) {
            ""
        } else {
            text
        };
        self.names.set(source, text);
        self.remember_names();
        self.push();
    }

    fn remember_names(&mut self) {
        let Some(serial) = self.serial() else { return };
        if self.settings.names.get(&serial) != Some(&self.names) {
            self.settings.names.insert(serial, self.names.clone());
            self.schedule_save();
        }
    }

    /// How a source is labelled where its own name would be `channel`.
    fn label(&self, source: usize, channel: &str) -> Label {
        let channel = channel_name(channel);
        match self.names.get(source) {
            Some(name) => Label {
                text: name.into(),
                channel: channel.into(),
                custom: true,
            },
            None => Label {
                text: channel.as_str().into(),
                channel: channel.into(),
                custom: false,
            },
        }
    }

    /// Installs or removes the virtual devices. macOS asks for the
    /// administrator's password meanwhile, so this runs on its own thread.
    fn change_devices(&mut self, install: bool) {
        if self.devices_busy {
            return;
        }
        self.devices_busy = true;
        std::thread::spawn(move || {
            let result = if install {
                virtual_devices::install()
            } else {
                virtual_devices::remove()
            };
            let _ = slint::invoke_from_event_loop(move || {
                with(|c| c.on_devices_changed(install, result));
            });
        });
        self.push();
    }

    /// Looks the virtual devices up on another thread: the audio server
    /// answers that, and it may hang.
    fn check_devices(&mut self) {
        if !self.look_up_devices {
            return;
        }
        self.devices_lookup += 1;
        let lookup = self.devices_lookup;
        virtual_devices::state_in_background(move |state| {
            let _ = slint::invoke_from_event_loop(move || {
                with(|c| {
                    if c.devices_lookup == lookup {
                        c.devices = state;
                        c.push();
                    }
                });
            });
        });
    }

    fn on_devices_changed(&mut self, installed: bool, result: Result<(), String>) {
        self.devices_busy = false;
        self.check_devices();
        match result {
            Ok(()) => {
                self.notify(if installed {
                    tr!("Virtual outputs installed")
                } else {
                    tr!("Virtual outputs removed")
                });
                // The device comes back with or without the mixer.
                self.send(Command::Rescan);
            }
            Err(error) if error == virtual_devices::CANCELED => {}
            Err(error) => self.notify(tr!("Failed: {}", error)),
        }
        self.push();
    }

    fn set_language(&mut self, language: Language) {
        if self.settings.language != language {
            self.settings.language = language;
            self.schedule_save();
        }
        i18n::set(language);
        // Texts made here are made again in the new language.
        self.rates.clear();
        self.push();
    }

    fn toggle_autostart(&mut self) {
        let result = if self.autostart {
            autostart::disable()
        } else {
            autostart::enable()
        };
        if let Err(error) = result {
            self.notify(tr!("Couldn't change the login item: {}", error));
        }
        self.autostart = autostart::is_enabled();
        self.push();
    }

    fn remember_mix(&mut self) {
        // With the driver's mixer the device's own loopback mixer is left alone.
        if self.mixer {
            return;
        }
        let Some(serial) = self.serial() else { return };
        if self.settings.mixes.get(&serial) != Some(&self.mix) {
            self.settings.mixes.insert(serial, self.mix);
            self.schedule_save();
        }
    }

    /// Saves shortly after the last change, so a dragged fader writes the
    /// file once.
    fn schedule_save(&mut self) {
        self.unsaved = true;
        self.save_timer
            .start(TimerMode::SingleShot, SAVE_DELAY, || {
                with(Controller::flush);
            });
    }

    fn flush(&mut self) {
        self.save_timer.stop();
        if !std::mem::take(&mut self.unsaved) || self.demo {
            return;
        }
        if let Err(error) = self.settings.save() {
            self.notify(tr!("Couldn't save the settings: {}", error));
        }
    }

    fn suggested_name(&self) -> String {
        format!("MiniFuse 1.{}", presets::EXTENSION)
    }

    fn import(&mut self, path: PathBuf) {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => return self.fail(tr!("Couldn't read the file: {}", error)),
        };
        let is_prefs = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"));
        let preset = if is_prefs {
            let serial = self.serial().unwrap_or_default();
            presets::parse_official_prefs(&text, &serial, self.snapshot())
                .ok_or_else(|| tr!("The file has no settings for this device"))
        } else {
            presets::parse_mfp(&text, self.snapshot()).map_err(|error| error.to_string())
        };
        let preset = match preset {
            Ok(preset) => preset,
            Err(error) => return self.fail(error),
        };
        if let Some(serial) = self.serial()
            && !preset.other.is_empty()
        {
            self.settings
                .preset_extras
                .insert(serial, preset.other.clone());
            self.schedule_save();
        }
        if preset.hardware + preset.mixer + preset.names == 0 {
            return self.fail(tr!("The file has no MiniFuse 1 settings"));
        }
        let snapshot = preset.snapshot;
        if preset.names > 0 {
            self.names = snapshot.names.clone();
            self.remember_names();
        }
        if preset.hardware > 0 {
            let now = Instant::now();
            self.inputs = snapshot.inputs;
            self.inputs_edited = Some(now);
            let mut writes = self.inputs.writes();
            if !self.mixer {
                self.mix = snapshot.mix;
                self.mix_edited = Some(now);
                writes.extend(self.mix.all_writes());
            }
            self.send(Command::Write(writes));
            self.remember_mix();
        }
        if preset.mixer > 0 {
            self.sls = snapshot.sls;
            if self.mixer {
                self.send(Command::Mixer(self.sls.all_gains()));
            }
            self.remember_sls();
        }
        self.flush();
        self.notify(if preset.hardware == 0 && preset.mixer > 0 && !self.mixer {
            tr!("Mixer settings saved. They take effect once the virtual outputs are installed.")
        } else {
            tr!("Settings imported")
        });
        self.push();
    }

    fn export(&mut self, path: PathBuf) {
        let other = self
            .serial()
            .and_then(|serial| self.settings.preset_extras.get(&serial).cloned())
            .unwrap_or_default();
        let text = presets::to_mfp(&self.snapshot(), &other);
        match std::fs::write(&path, text) {
            Ok(()) => self.notify(tr!("Settings exported")),
            Err(error) => self.notify(tr!("Couldn't save the file: {}", error)),
        }
        self.push();
    }

    /// Updates the mixer rows of the selected mix; unchanged rows are left
    /// alone so they do not redraw.
    fn push_sources(&self) {
        let mix = &self.sls.mixes[self.mix_tab];
        let rows = mix.sources.iter().enumerate().map(|(i, source)| {
            let [left, right] = self.sls_meters.sources[i];
            let label = self.label(i, sls::SOURCE_NAMES[i]);
            let stereo = i != sls::INPUT_1;
            let pan = source.pan(stereo);
            SourceView {
                name: label.text,
                channel: label.channel,
                custom: label.custom,
                level: source.level_db,
                pan,
                pan_label: pan_label(pan),
                stereo,
                mono: stereo && source.mono,
                muted: source.muted,
                meter_left: after_fader(left, source),
                meter_right: after_fader(right, source),
            }
        });
        for (i, row) in rows.enumerate() {
            if i >= self.sources.row_count() {
                self.sources.push(row);
            } else if self.sources.row_data(i).as_ref() != Some(&row) {
                self.sources.set_row_data(i, row);
            }
        }
    }

    /// The part of [`Self::push`] that meter readings change, up to 20 times
    /// a second while the window is open: nothing else is touched.
    fn push_meters(&self) {
        let Some(app) = &self.app else { return };
        app.set_meter_in(self.meters[0].display_db());
        app.set_meter_left(self.meters[1].display_db());
        app.set_meter_right(self.meters[2].display_db());
        let [left, right] = self.sls_meters.mixes[self.mix_tab];
        app.set_mix_meter_left(meter_display(left));
        app.set_mix_meter_right(meter_display(right));
        let mix = &self.sls.mixes[self.mix_tab];
        for (i, [left, right]) in self.sls_meters.sources.into_iter().enumerate() {
            let Some(mut row) = self.sources.row_data(i) else {
                continue;
            };
            let source = &mix.sources[i];
            let (left, right) = (after_fader(left, source), after_fader(right, source));
            if row.meter_left != left || row.meter_right != right {
                row.meter_left = left;
                row.meter_right = right;
                self.sources.set_row_data(i, row);
            }
        }
    }

    fn fail(&mut self, message: String) {
        self.notify(message);
        self.push();
    }

    /// Pushes the model into the window, if it is open. Unchanged values
    /// cost nothing.
    fn push(&mut self) {
        // Also without a window: the input is heard from the tray too.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        crate::virtual_devices::bridge::monitor_input(
            self.mixer && self.status == STATUS_CONNECTED && self.sls.input_monitored(),
        );
        let Some(app) = self.app.as_ref().map(ComponentHandle::clone_strong) else {
            return;
        };
        app.set_status(self.status);
        app.set_status_text(self.status_text.as_str().into());
        let (firmware_text, update) = self.firmware_text();
        if let Some(info) = &self.device {
            app.set_device_name(info.product.as_str().into());
            let mut detail = if info.serial.is_empty() {
                tr!("Firmware {}", info.firmware)
            } else {
                tr!("SN {} · firmware {}", info.serial, info.firmware)
            };
            if let (true, FirmwareState::Known(latest)) = (update, &self.firmware) {
                detail = format!("{detail} · {}", tr!("{} available", latest));
            }
            app.set_device_detail(detail.into());
        }
        app.set_live(self.live);
        app.set_input_meter(self.input_meter);
        app.set_firmware_text(firmware_text.into());
        app.set_firmware_update(update);
        app.set_firmware_busy(matches!(self.firmware, FirmwareState::Checking));

        app.set_inst(self.inputs.inst);
        app.set_phantom(self.inputs.phantom);
        app.set_direct(self.inputs.direct_monitor);
        let input = self.label(sls::INPUT_1, sls::SOURCE_NAMES[sls::INPUT_1]);
        if app.get_input_label() != input {
            app.set_input_label(input);
        }
        let labels: Vec<Label> = LOOPBACK_NAMES
            .iter()
            .enumerate()
            .map(|(source, name)| self.label(source, name))
            .collect();
        if app.get_loopback_labels().iter().ne(labels.iter().cloned()) {
            app.set_loopback_labels(ModelRc::new(VecModel::from(labels)));
        }
        app.set_autostart(self.autostart);
        app.set_autostart_text(autostart::description().into());
        let languages: Vec<SharedString> = Language::ALL.iter().map(|l| l.name().into()).collect();
        if app.get_languages().iter().ne(languages.iter().cloned()) {
            app.set_languages(ModelRc::new(VecModel::from(languages)));
        }
        let language = Language::ALL
            .iter()
            .position(|&l| l == self.settings.language);
        app.set_language_index(language.map_or(0, |i| i as i32));
        let accents: Vec<SharedString> = Accent::PRESETS.iter().map(|a| a.name().into()).collect();
        if app.get_accent_names().iter().ne(accents.iter().cloned()) {
            app.set_accent_names(ModelRc::new(VecModel::from(accents)));
        }
        let theme = app.global::<Theme>();
        let preset = Accent::PRESETS
            .iter()
            .position(|&a| a == self.settings.accent);
        theme.set_accent_index(preset.unwrap_or(Accent::PRESETS.len()) as i32);
        let custom = parse_hex_color(&self.settings.custom_accent).unwrap_or([0xff, 0x8a, 0x3d]);
        theme.set_custom_accent(slint::Color::from_rgb_u8(custom[0], custom[1], custom[2]));
        theme.set_on_accent(if preset.is_some() {
            slint::Color::from_rgb_u8(0xff, 0xff, 0xff)
        } else {
            ink_on(custom)
        });
        app.set_accent_hex(self.settings.custom_accent.trim_start_matches('#').into());
        app.set_accent_hex_valid(self.accent_hex_valid);
        let (mut text, install, removable) = self.devices.describe();
        if self.devices_busy {
            text = if cfg!(target_os = "macos") {
                tr!("Waiting for the administrator password…")
            } else {
                tr!("Working…")
            };
        }
        app.set_devices_supported(self.devices != DevicesState::Unsupported);
        app.set_devices_text(text.into());
        app.set_devices_install(install.into());
        app.set_devices_removable(removable);
        app.set_devices_busy(self.devices_busy);

        let mix = &self.mix;
        app.set_loopback(mix.enabled);
        app.set_in_level(mix.input1.level_db);
        app.set_in_pan(mix.input1.pan);
        app.set_in_pan_label(pan_label(mix.input1.pan));
        app.set_in_muted(mix.input1.muted);
        app.set_usb12_level(mix.usb12.level_db);
        app.set_usb12_muted(mix.usb12.muted);
        app.set_usb34_level(mix.usb34.level_db);
        app.set_usb34_muted(mix.usb34.muted);
        app.set_out_level(mix.output_db);
        app.set_to_usb12(mix.to_usb12);
        app.set_to_outputs(mix.to_outputs);

        app.set_mixer(self.mixer);
        app.set_mix_index(self.mix_tab as i32);
        app.set_mix_level(self.sls.mixes[self.mix_tab].output_db);
        self.push_sources();
        self.push_meters();

        app.set_asio(self.asio.is_some());
        if let Some(asio) = &self.asio {
            if asio.sizes != self.asio_sizes {
                let names: Vec<SharedString> =
                    asio.sizes.iter().map(|s| s.to_string().into()).collect();
                app.set_asio_sizes(ModelRc::new(VecModel::from(names)));
                self.asio_sizes = asio.sizes.clone();
            }
            let index = asio.sizes.iter().position(|&s| s == asio.size);
            app.set_asio_index(index.map_or(-1, |i| i as i32));
            app.set_asio_safe(asio.safe_mode);
        }

        let rates = self
            .rate
            .as_ref()
            .map(|r| r.available.clone())
            .unwrap_or_default();
        if rates != self.rates {
            let names: Vec<SharedString> = rates.iter().map(|&r| format_rate(r)).collect();
            app.set_rates(ModelRc::new(VecModel::from(names)));
            self.rates = rates;
        }
        let current = self.rate.as_ref().and_then(|r| r.current);
        let index = current
            .and_then(|current| self.rates.iter().position(|&r| r == current))
            .map_or(-1, |i| i as i32);
        app.set_rate_index(index);
        app.set_rate_settable(self.rate.as_ref().is_some_and(|r| r.settable));

        let message = self
            .message
            .as_ref()
            .map(|(text, _)| text.as_str())
            .unwrap_or_default();
        app.set_message(message.into());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementRoot};

    use super::*;
    use crate::device::protocol::{STATE_WORDS, Write};

    fn setup() -> (App, mpsc::Receiver<Command>) {
        setup_with(false)
    }

    /// A connected device; `mixer`: with the Windows driver's software mixer.
    fn setup_with(mixer: bool) -> (App, mpsc::Receiver<Command>) {
        i_slint_backend_testing::init_no_event_loop();
        let app = App::new().unwrap();
        crate::i18n::set(Language::English);
        let (tx, rx) = mpsc::channel();
        CONTROLLER.with(|c| *c.borrow_mut() = Some(Controller::new(tx, true)));
        connect(&app);
        with(|c| c.attach(app.clone_strong()));
        let mut state = RawState([0; STATE_WORDS]);
        Mix {
            enabled: true,
            ..Mix::default()
        }
        .all_writes()
        .into_iter()
        .for_each(|w| state.apply(w));
        let info = DeviceInfo {
            product: "MiniFuse 1".into(),
            serial: "TEST".into(),
            firmware: "1.5.0".into(),
        };
        with(|c| {
            c.on_event(Event::Connected {
                info,
                state,
                live: true,
                input_meter: true,
                mixer,
            })
        });
        (app, rx)
    }

    /// The interactive element with this label (texts share their label).
    fn control(app: &App, label: &str) -> Option<ElementHandle> {
        app.root_element()
            .query_descendants()
            .match_predicate({
                let label = label.to_owned();
                move |e| {
                    e.accessible_label().is_some_and(|l| l == label)
                        && e.accessible_role() != Some(AccessibleRole::Text)
                }
            })
            .find_first()
    }

    fn writes(rx: &mpsc::Receiver<Command>) -> Vec<Write> {
        rx.try_iter()
            .flat_map(|command| match command {
                Command::Write(writes) => writes,
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn buttons_send_writes_and_light_up() {
        let (app, rx) = setup();
        let inst = control(&app, "INST").unwrap();
        assert_eq!(inst.accessible_checked(), Some(false));
        inst.invoke_accessible_default_action();
        assert_eq!(writes(&rx), vec![(Param::InputInst, 0, 1)]);
        assert_eq!(inst.accessible_checked(), Some(true));

        let mute = control(&app, "Mute USB 1-2").unwrap();
        assert_eq!(mute.accessible_checked(), Some(false));
        mute.invoke_accessible_default_action();
        assert_eq!(mute.accessible_checked(), Some(true));
        assert_eq!(
            writes(&rx),
            vec![(Param::MixMute, 2, 1), (Param::MixMute, 3, 1)]
        );
        control(&app, "Mix → outputs")
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(writes(&rx), vec![(Param::MixToOutputs, 0, 1)]);
    }

    #[test]
    fn faders_send_levels() {
        let (app, rx) = setup();
        let mix = control(&app, "Mix").unwrap();
        mix.set_accessible_value("-6");
        assert_eq!(
            writes(&rx),
            vec![(Param::MixOutputLevel, 0, (-6i16 * 256) as u16)]
        );
        assert_eq!(mix.accessible_value().as_deref(), Some("-6.0"));
        mix.invoke_accessible_increment_action();
        assert_eq!(mix.accessible_value().as_deref(), Some("-5.5"));
        mix.set_accessible_value("20");
        assert_eq!(
            mix.accessible_value().as_deref(),
            Some("0.0"),
            "clamped to the range"
        );

        let usb = control(&app, "USB 3-4").unwrap();
        usb.set_accessible_value("0");
        let raw = (crate::device::protocol::stereo_to_raw(0.0)) as u16;
        assert_eq!(writes(&rx).last(), Some(&(Param::MixLevelRight, 5, raw)));

        control(&app, "Pan Input 1")
            .unwrap()
            .set_accessible_value("0");
        let sent = writes(&rx);
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1],
            (
                Param::MixLevelRight,
                0,
                crate::device::protocol::LEVEL_OFF as u16
            )
        );
    }

    #[test]
    fn loopback_switch_hides_the_mixer() {
        let (app, rx) = setup();
        assert!(control(&app, "USB 1-2").is_some());
        let switch = control(&app, "Loopback").unwrap();
        switch.invoke_accessible_default_action();
        assert_eq!(writes(&rx), vec![(Param::Loopback, 0, 0)]);
        assert_eq!(switch.accessible_checked(), Some(false));
        assert!(control(&app, "USB 1-2").is_none());
    }

    #[test]
    fn device_changes_show_up_and_rates_are_selectable() {
        let (app, rx) = setup();
        let mut state = RawState([0; STATE_WORDS]);
        state.apply((Param::Input48V, 0, 1));
        with(|c| c.on_event(Event::State(state)));
        assert_eq!(
            control(&app, "+48V").unwrap().accessible_checked(),
            Some(true)
        );
        assert!(
            control(&app, "USB 1-2").is_none(),
            "loopback is off in this state"
        );

        let rate = SampleRate {
            current: Some(48_000),
            available: vec![44_100, 48_000, 96_000],
            settable: true,
        };
        with(|c| c.on_event(Event::SampleRate(Some(rate))));
        let combo = control(&app, "Sample rate").unwrap();
        assert_eq!(combo.accessible_value().as_deref(), Some("48 kHz"));
        app.invoke_rate_selected(2);
        assert!(
            rx.try_iter()
                .any(|c| matches!(c, Command::SetSampleRate(96_000)))
        );
    }

    fn mixer_gains(rx: &mpsc::Receiver<Command>) -> sls::Gains {
        let mut gains = sls::Gains::default();
        for command in rx.try_iter() {
            if let Command::Mixer(more) = command {
                gains.extend(more);
            }
        }
        gains
    }

    #[test]
    fn numbers_are_read_as_typed() {
        assert_eq!(parse_number("-6,5"), -6.5);
        assert_eq!(parse_number(" \u{2212}12 dB"), -12.0);
        assert_eq!(parse_number("3 дБ"), 3.0);
        assert_eq!(parse_number("abc"), NOT_A_NUMBER);
        assert_eq!(parse_number(""), NOT_A_NUMBER);
    }

    #[test]
    fn typed_levels_reach_the_device() {
        let (app, rx) = setup();
        let value = control(&app, "USB 1-2, dB").unwrap();
        value.set_accessible_value("-6,5");
        let raw = crate::device::protocol::stereo_to_raw(-6.5) as u16;
        assert_eq!(writes(&rx).last(), Some(&(Param::MixLevelRight, 3, raw)));
        assert_eq!(value.accessible_value().as_deref(), Some("-6.5"));
        value.set_accessible_value("ерунда");
        assert!(writes(&rx).is_empty());
        value.set_accessible_value("40");
        assert_eq!(value.accessible_value().as_deref(), Some("12.0"));
    }

    #[test]
    fn clicking_a_value_lets_you_type_it() {
        use slint::LogicalPosition;
        use slint::platform::{Key, PointerEventButton, WindowEvent};

        let (app, rx) = setup();
        app.window().set_size(slint::LogicalSize::new(700.0, 520.0));
        let value = control(&app, "Mix, dB").unwrap();
        let (p, size) = (value.absolute_position(), value.size());
        let at = LogicalPosition::new(p.x + size.width / 2.0, p.y + size.height / 2.0);
        let window = app.window();
        let button = PointerEventButton::Left;
        window.dispatch_event(WindowEvent::PointerPressed {
            position: at,
            button,
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position: at,
            button,
        });
        let mut keys: Vec<slint::SharedString> =
            ["\u{2212}", "1", "2", ",", "5"].map(Into::into).to_vec();
        keys.push(Key::Return.into());
        for text in keys {
            window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            window.dispatch_event(WindowEvent::KeyReleased { text });
        }
        assert_eq!(
            writes(&rx),
            vec![(Param::MixOutputLevel, 0, (-12.5f32 * 256.0) as i16 as u16)]
        );
        assert_eq!(value.accessible_value().as_deref(), Some("-12.5"));
    }

    #[test]
    fn software_mixer_sends_gains() {
        let (app, rx) = setup_with(true);
        assert!(
            control(&app, "Loopback").is_none(),
            "the device's loopback mixer stays hidden"
        );
        control(&app, "Main 1/2")
            .unwrap()
            .set_accessible_value("-6");
        let mut expected = Sls::default();
        assert_eq!(
            mixer_gains(&rx),
            expected.apply(SlsControl::Level {
                mix: 0,
                source: 1,
                db: -6.0,
            })
        );

        // One pan moves both channels of a stereo source.
        let pan = control(&app, "Pan Main 1/2").unwrap();
        pan.set_accessible_value("0.25");
        assert_eq!(mixer_gains(&rx).crosspoints.len(), 8);
        let main = with(|c| c.sls.mixes[0].sources[1]).unwrap();
        assert_eq!((main.pan_left, main.pan_right), (0.0, 0.5));
        assert_eq!(pan.accessible_value().as_deref(), Some("0.25"));
        assert_eq!(app.get_sources().row_data(1).unwrap().pan_label, "L50");

        let tab = control(&app, "Mix 5/6").unwrap();
        tab.invoke_accessible_default_action();
        assert_eq!(tab.accessible_checked(), Some(true));
        let mute = control(&app, "Mute Virtual 5/6").unwrap();
        assert_eq!(mute.accessible_checked(), Some(false));
        mute.invoke_accessible_default_action();
        let gains = mixer_gains(&rx);
        assert_eq!(gains.crosspoints.len(), 4);
        assert!(gains.crosspoints.iter().all(|p| p.2 == 0));
        assert_eq!(mute.accessible_checked(), Some(true));

        control(&app, "Output, dB")
            .unwrap()
            .set_accessible_value("-12,5");
        assert_eq!(mixer_gains(&rx).outputs.len(), 2);
        assert_eq!(with(|c| c.sls.mixes[2].output_db), Some(-12.5));

        let meters = sls::Meters {
            sources: [[-20.0, -30.0]; sls::SOURCES],
            mixes: [[-6.0, -7.0]; sls::MIXES],
        };
        with(|c| c.on_event(Event::MixerMeters(meters)));
        assert_eq!(
            control(&app, "Virtual 7/8 R")
                .unwrap()
                .accessible_value()
                .as_deref(),
            Some("-30.0")
        );
        assert_eq!(app.get_mix_meter_left(), -6.0);
        // The meters are after the fader.
        control(&app, "Virtual 7/8, dB")
            .unwrap()
            .set_accessible_value("-10");
        assert_eq!(
            control(&app, "Virtual 7/8 R")
                .unwrap()
                .accessible_value()
                .as_deref(),
            Some("-40.0")
        );
    }

    #[test]
    fn arrow_keys_move_the_focused_control() {
        let (app, rx) = setup_with(true);
        let press = |key: slint::platform::Key| {
            use slint::platform::WindowEvent;
            app.window()
                .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
            app.window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        };
        // Tab reaches the fader of Input 1 first, then its pan.
        press(slint::platform::Key::Tab);
        press(slint::platform::Key::DownArrow);
        assert_eq!(with(|c| c.sls.mixes[0].sources[0].level_db), Some(-0.5));
        press(slint::platform::Key::PageDown);
        assert_eq!(with(|c| c.sls.mixes[0].sources[0].level_db), Some(-6.5));
        press(slint::platform::Key::Tab);
        press(slint::platform::Key::RightArrow);
        assert_eq!(with(|c| c.sls.mixes[0].sources[0].pan_left), Some(0.55));
        assert!(!mixer_gains(&rx).crosspoints.is_empty());
    }

    #[test]
    fn mono_sums_a_stereo_source() {
        let (app, rx) = setup_with(true);
        assert!(
            control(&app, "Mono Input 1").is_none(),
            "the input is mono anyway"
        );
        let mono = control(&app, "Mono Main 1/2").unwrap();
        assert_eq!(mono.accessible_checked(), Some(false));
        mono.invoke_accessible_default_action();
        assert_eq!(mono.accessible_checked(), Some(true));
        let main = with(|c| c.sls.mixes[0].sources[1]).unwrap();
        assert_eq!((main.pan_left, main.pan_right), (0.5, 0.5));
        assert_eq!(mixer_gains(&rx).crosspoints.len(), 8);
        control(&app, "Pan Main 1/2")
            .unwrap()
            .set_accessible_value("0.25");
        let main = with(|c| c.sls.mixes[0].sources[1]).unwrap();
        assert_eq!((main.pan_left, main.pan_right), (0.25, 0.25));
    }

    #[test]
    fn the_accent_color_is_a_setting() {
        let (app, _rx) = setup();
        app.set_settings_open(true);
        assert_eq!(app.global::<Theme>().get_accent_index(), 0);
        control(&app, "Teal")
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(with(|c| c.settings.accent), Some(Accent::Teal));
        assert_eq!(app.global::<Theme>().get_accent_index(), 2);

        // A color of one's own, typed as HEX.
        let hex = control(&app, "Accent color, HEX").unwrap();
        hex.set_accessible_value("#12AB34");
        assert_eq!(with(|c| c.settings.accent), Some(Accent::Custom));
        assert_eq!(
            with(|c| c.settings.custom_accent.clone()).as_deref(),
            Some("#12ab34")
        );
        let theme = app.global::<Theme>();
        assert_eq!(theme.get_accent_index(), 5);
        assert_eq!(
            theme.get_accent(),
            slint::Color::from_rgb_u8(0x12, 0xab, 0x34)
        );
        assert_eq!(app.get_accent_hex(), "12ab34");
        assert!(app.get_accent_hex_valid());
        // Typos keep the color and mark the field; unfinished digits do not.
        hex.set_accessible_value("12ab3");
        assert!(app.get_accent_hex_valid());
        hex.set_accessible_value("12ab3x");
        assert!(!app.get_accent_hex_valid());
        assert_eq!(app.get_accent_hex(), "12ab34");
        // A light color gets dark text.
        hex.set_accessible_value("fe0");
        assert_eq!(
            theme.get_on_accent(),
            slint::Color::from_rgb_u8(0x1d, 0x1d, 0x1f)
        );
        // Back to a preset, which keeps the own color for later.
        control(&app, "Orange")
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(theme.get_accent_index(), 0);
        assert_eq!(
            theme.get_on_accent(),
            slint::Color::from_rgb_u8(0xff, 0xff, 0xff)
        );
        control(&app, "Custom")
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(theme.get_accent_index(), 5);
        assert_eq!(
            theme.get_accent(),
            slint::Color::from_rgb_u8(0xff, 0xee, 0x00)
        );
    }

    #[test]
    fn asio_settings_follow_the_driver() {
        let (app, rx) = setup_with(true);
        app.set_settings_open(true);
        assert!(control(&app, "ASIO buffer size").is_none());
        let asio = Asio {
            sizes: vec![64, 128, 256],
            size: 128,
            safe_mode: true,
        };
        with(|c| c.on_event(Event::Asio(Some(asio))));
        let combo = control(&app, "ASIO buffer size").unwrap();
        assert_eq!(combo.accessible_value().as_deref(), Some("128"));
        app.invoke_asio_size_selected(2);
        control(&app, "Safe Mode")
            .unwrap()
            .invoke_accessible_default_action();
        let sent: Vec<(u32, bool)> = rx
            .try_iter()
            .filter_map(|command| match command {
                Command::SetAsio { size, safe_mode } => Some((size, safe_mode)),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![(256, true), (256, false)]);
        assert_eq!(combo.accessible_value().as_deref(), Some("256"));
    }

    #[test]
    fn channels_can_be_renamed() {
        let (app, _rx) = setup_with(true);
        let name = control(&app, "Name of Virtual 5/6").unwrap();
        assert_eq!(name.accessible_value().as_deref(), Some("Virtual 5/6"));
        name.set_accessible_value("  Zoom ");
        assert_eq!(name.accessible_value().as_deref(), Some("Zoom"));
        let row = app.get_sources().row_data(3).unwrap();
        assert!(row.custom);
        assert_eq!(row.channel, "Virtual 5/6");
        assert!(control(&app, "Mute Zoom").is_some());
        assert_eq!(
            with(|c| c
                .settings
                .names
                .get("TEST")
                .and_then(|n| n.get(3).map(str::to_owned))),
            Some(Some("Zoom".to_owned()))
        );
        // The same source in the device's loopback mixer shares the name.
        control(&app, "Name of Input 1")
            .unwrap()
            .set_accessible_value("Голос");
        assert_eq!(app.get_input_label().text, "Голос");
        assert_eq!(app.get_loopback_labels().row_data(0).unwrap().text, "Голос");
        // Typing the channel's own name drops the custom one.
        name.set_accessible_value("virtual 5/6");
        assert_eq!(with(|c| c.names.get(3).map(str::to_owned)), Some(None));
        assert!(!app.get_sources().row_data(3).unwrap().custom);
        let exported = with(|c| presets::to_mfp(&c.snapshot(), &[])).unwrap();
        assert!(exported.contains("SLS_Input_Name_0\nГолос\n"));
        assert!(exported.contains("SLS_VirtualInput_Name_1\n\n"));
    }

    #[test]
    fn virtual_devices_are_offered_in_settings() {
        let (app, _rx) = setup();
        app.set_settings_open(true);
        with(|c| {
            c.devices = DevicesState::Missing;
            c.push();
        });
        assert!(control(&app, "Install…").is_some());
        assert!(control(&app, "Remove").is_none());
        with(|c| {
            c.devices = DevicesState::Ready;
            c.push();
        });
        assert!(control(&app, "Install…").is_none());
        assert!(control(&app, "Remove").is_some());
        with(|c| {
            c.devices = DevicesState::Unsupported;
            c.push();
        });
        assert!(!app.get_devices_supported());
        assert!(control(&app, "Remove").is_none());
        assert!(control(&app, "Start at login").is_some());
    }

    #[test]
    fn closing_the_window_keeps_the_app_in_the_tray() {
        let (app, rx) = setup();
        let _ = rx.try_iter().count();
        app.window()
            .dispatch_event(slint::platform::WindowEvent::CloseRequested);
        assert_eq!(with(|c| c.app.is_none()), Some(true));
        assert!(
            rx.try_iter()
                .any(|c| matches!(c, Command::Pace(Pace::Background)))
        );
        // Opening it again builds a new window with the current state.
        show_window();
        let name = with(|c| c.app.as_ref().map(|app| app.get_device_name())).flatten();
        assert_eq!(name.as_deref(), Some("MiniFuse 1"));
        assert!(
            rx.try_iter()
                .any(|c| matches!(c, Command::Pace(Pace::Visible)))
        );
        // "Hide VibeFuse" in the tray menu does the same as closing.
        hide_window();
        assert_eq!(with(|c| c.app.is_none()), Some(true));
    }

    #[test]
    fn texts_follow_the_language() {
        let (app, _rx) = setup();
        crate::i18n::set(Language::Russian);
        with(|c| c.push());
        assert_eq!(app.get_input_label().text, "Вход 1");
        assert!(app.get_device_detail().contains("прошивка"));
        assert!(control(&app, "Выключить звук: USB 1-2").is_some());
        crate::i18n::set(Language::English);
        with(|c| c.push());
        assert_eq!(app.get_input_label().text, "Input 1");
        assert!(control(&app, "Mute USB 1-2").is_some());
    }

    #[test]
    fn newer_firmware_is_announced() {
        let (app, _rx) = setup();
        with(|c| c.on_firmware(Ok(Version::parse("1.6.0").unwrap())));
        assert!(app.get_firmware_update());
        assert!(app.get_device_detail().contains("1.6.0 available"));
        assert!(app.get_message().contains("1.6.0"));
        with(|c| c.on_firmware(Ok(Version::parse("1.5.0").unwrap())));
        assert!(!app.get_firmware_update());
        assert!(app.get_firmware_text().contains("up to date"));
    }

    #[test]
    fn rises_at_once_and_falls_steadily() {
        let start = Instant::now();
        let mut meter = Meter {
            updated_at: start,
            ..Meter::default()
        };
        meter.feed(1.0, start);
        assert_eq!(meter.display_db(), 0.0);
        meter.feed(0.0, start + Duration::from_millis(500));
        assert_eq!(meter.display_db(), -13.0);
        meter.feed(0.0, start + Duration::from_secs(10));
        assert_eq!(meter.display_db(), FLOOR_DB);
    }
}
