#![cfg_attr(windows, windows_subsystem = "windows")]

// The device transports, the virtual devices and the plug-in windows exist
// for these two systems only.
#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("VibeFuse runs on macOS and Windows");

mod desktop;
mod device;
mod firmware;
mod fx;
mod i18n;
mod settings;
mod ui;
mod virtual_devices;

use std::process::ExitCode;

use desktop::autostart;

use device::protocol::{self, Param};
use device::sls;
use device::{Found, read_meters, read_param, read_state};

const USAGE: &str = "\
VibeFuse, a control panel for the Arturia MiniFuse 1

  vibefuse                 open the window
  vibefuse --background    start in the tray without a window (the login item does this)
  vibefuse --dump          print the state of the device
  vibefuse --demo          open the window with a simulated device
  vibefuse --demo windows  the same with the Windows driver (software mixer, ASIO)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str);
    if !matches!(mode, None | Some(autostart::BACKGROUND)) {
        attach_console();
    }
    let result = match mode {
        None => ui::run(ui::Start::Window).map_err(|e| e.to_string()),
        Some(autostart::BACKGROUND) => ui::run(ui::Start::Tray).map_err(|e| e.to_string()),
        Some("--demo") => {
            let windows = match args.get(1).map(String::as_str) {
                None => cfg!(windows),
                Some("windows") => true,
                Some("mac" | "macos") => false,
                Some(other) => return usage_error(other),
            };
            ui::run(ui::Start::Demo { windows }).map_err(|e| e.to_string())
        }
        Some("--dump") => dump(),
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => return usage_error(other),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(argument: &str) -> ExitCode {
    eprintln!("unknown argument: {argument}\n\n{USAGE}");
    ExitCode::FAILURE
}

/// The GUI build has no console on Windows; command-line modes borrow the
/// one they were started from.
fn attach_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 call without pointers.
    unsafe {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn dump() -> Result<(), String> {
    // The audio server answers most of this; a hung one must not hang here.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
        eprintln!("{}", device::unresponsive());
        std::process::exit(1);
    });
    let mut bus = device::system_bus();
    let mut device = match bus.scan().map_err(|e| e.to_string())? {
        Found::Audio(device) => device,
        Found::Nothing => return Err("no MiniFuse 1 found".to_owned()),
        Found::Storage => {
            return Err("the MiniFuse 1 is in storage mode; switch it to audio mode".to_owned());
        }
        Found::Unavailable(reason) => return Err(reason),
    };
    let info = device.info().clone();
    println!(
        "{} · SN {} · firmware {}",
        info.product, info.serial, info.firmware
    );
    let live = device.is_live();
    device
        .session(&mut |link| {
            let raw = read_state(link)?;
            let words: Vec<String> = raw.0.iter().map(|w| format!("{w:04X}")).collect();
            println!("state:    {}", words.join(" "));
            println!("inputs:   {:?}", raw.inputs());
            println!("loopback:  {:#?}", raw.mix());
            println!(
                "loopback (request 0xA2): {}",
                read_param(link, Param::Loopback, 0)?
            );
            if live {
                let meters = read_meters(link)?;
                let db = |a: f32| {
                    if a > 0.0 {
                        20.0 * a.log10()
                    } else {
                        f32::NEG_INFINITY
                    }
                };
                println!(
                    "meters:   input 1 {:.1} dBFS · loopback L {:.1} / R {:.1} dBFS",
                    db(meters[protocol::meter::INPUT_1]),
                    db(meters[protocol::meter::LOOPBACK_LEFT]),
                    db(meters[protocol::meter::LOOPBACK_RIGHT]),
                );
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    match device.sample_rate() {
        Some(rate) => println!(
            "rate:     {} (available: {:?})",
            rate.current.map_or("—".to_owned(), |r| format!("{r} Hz")),
            rate.available
        ),
        None => println!("rate:     unknown"),
    }
    if device.has_mixer() {
        let kind = if cfg!(windows) {
            "Arturia's driver"
        } else {
            "virtual outputs (VibeFuse Mixer plug-in)"
        };
        println!("mixer:    {kind}");
        let _ = device.mixer_set(&sls::meters_switch(true));
        let mut reply = sls::meters_query();
        if device.mixer_get(&mut reply).is_ok()
            && let Some(meters) = sls::parse_meters(&reply)
        {
            for (name, [l, r]) in sls::MIX_NAMES.iter().zip(meters.mixes) {
                println!("  {name}: L {l:.1} / R {r:.1} dB");
            }
        }
    } else if cfg!(windows) {
        println!("mixer:    none in this driver (update the MiniFuse Audio Driver)");
    } else {
        println!("virtual outputs: {:?}", virtual_devices::state());
    }
    #[cfg(target_os = "macos")]
    if let Some(plugin) = device::coreaudio::plugin(device::hal_mixer::BUNDLE_ID) {
        match device::hal_mixer::status(plugin) {
            Ok(status) => println!(
                "plug-in:  version {}, devices in use: {}, bridge {}, {} Hz",
                status.version,
                status.running,
                if status.bridge { "running" } else { "stopped" },
                status.rate
            ),
            Err(error) => println!("plug-in:  not responding: {error}"),
        }
    }
    if let Some(asio) = device.asio() {
        println!(
            "ASIO:     buffer {} (available: {:?}), Safe Mode {}",
            asio.size,
            asio.sizes,
            if asio.safe_mode { "on" } else { "off" }
        );
    }
    #[cfg(windows)]
    match fx::asio::describe() {
        Ok(text) => println!("effects:  {text}"),
        Err(error) => println!("effects:  ASIO not available: {error}"),
    }
    Ok(())
}
