//! How VibeFuse lives on the desktop: one copy per user, the login item, the
//! tray and, on macOS, the Dock icon.

pub mod instance {
    //! One VibeFuse per user. The first start holds a lock file and listens; a
    //! later start (the user opening the app while it sits in the tray) asks it
    //! to show its window and exits. The listener is a Unix socket next to the
    //! lock, a named pipe on Windows.

    use std::fs::File;
    use std::thread;
    use std::time::{Duration, Instant};

    const SHOW: &[u8] = b"show";
    /// How long a later start keeps trying while the first one is starting up.
    const CONNECT_PATIENCE: Duration = Duration::from_secs(3);

    /// Held by the running instance for its whole life.
    pub struct Instance {
        _lock: Option<File>,
    }

    /// Becomes the running instance and calls `on_show` (on a background
    /// thread) whenever a later start asks for the window. Returns `None` when
    /// another instance runs; it has been asked to show its window if `show`.
    pub fn claim(show: bool, on_show: impl Fn() + Send + 'static) -> Option<Instance> {
        let Some(dir) = crate::settings::app_dir() else {
            // Nowhere to coordinate: just run.
            return Some(Instance { _lock: None });
        };
        let _ = std::fs::create_dir_all(&dir);
        let lock = match File::create(dir.join("instance.lock")) {
            Ok(file) => file,
            Err(_) => return Some(Instance { _lock: None }),
        };
        match lock.try_lock() {
            Ok(()) => {
                platform::listen(on_show);
                Some(Instance { _lock: Some(lock) })
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if show {
                    let start = Instant::now();
                    while platform::send(SHOW).is_err() && start.elapsed() < CONNECT_PATIENCE {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                None
            }
            // Locks are not supported here: run without the guard.
            Err(std::fs::TryLockError::Error(_)) => Some(Instance { _lock: None }),
        }
    }

    fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
        let _ = thread::Builder::new().name(name.into()).spawn(body);
    }

    #[cfg(unix)]
    mod platform {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;

        use super::*;

        fn socket() -> Option<PathBuf> {
            Some(crate::settings::app_dir()?.join("instance.sock"))
        }

        pub fn listen(on_show: impl Fn() + Send + 'static) {
            let Some(path) = socket() else { return };
            // Left over from a crash; the lock says nobody else uses it.
            let _ = std::fs::remove_file(&path);
            let Ok(listener) = UnixListener::bind(&path) else {
                return;
            };
            spawn("vibefuse-instance", move || {
                for mut stream in listener.incoming().map_while(Result::ok) {
                    let mut request = [0; 16];
                    let n = stream.read(&mut request).unwrap_or(0);
                    if request[..n].starts_with(SHOW) {
                        on_show();
                    }
                }
            });
        }

        pub fn send(request: &[u8]) -> std::io::Result<()> {
            let path = socket().ok_or(std::io::ErrorKind::NotFound)?;
            UnixStream::connect(path)?.write_all(request)
        }
    }

    #[cfg(windows)]
    mod platform {
        use std::io::Write;
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;

        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{PIPE_ACCESS_INBOUND, ReadFile};
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
            PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        };

        use super::*;

        /// Pipes are machine-wide: the user's name keeps sessions apart.
        fn name() -> String {
            let user = std::env::var("USERNAME").unwrap_or_default();
            format!(r"\\.\pipe\VibeFuse-{user}")
        }

        pub fn listen(on_show: impl Fn() + Send + 'static) {
            let wide: Vec<u16> = std::ffi::OsStr::new(&name())
                .encode_wide()
                .chain(Some(0))
                .collect();
            spawn("vibefuse-instance", move || {
                loop {
                    // SAFETY: plain Win32 calls; the handle is closed below.
                    unsafe {
                        let pipe = CreateNamedPipeW(
                            wide.as_ptr(),
                            PIPE_ACCESS_INBOUND,
                            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                            PIPE_UNLIMITED_INSTANCES,
                            0,
                            64,
                            0,
                            ptr::null(),
                        );
                        if pipe == INVALID_HANDLE_VALUE {
                            return;
                        }
                        let connected = ConnectNamedPipe(pipe, ptr::null_mut()) != 0
                            || GetLastError() == ERROR_PIPE_CONNECTED;
                        if connected {
                            let mut request = [0u8; 16];
                            let mut read = 0u32;
                            let ok = ReadFile(
                                pipe,
                                request.as_mut_ptr(),
                                request.len() as u32,
                                &mut read,
                                ptr::null_mut(),
                            ) != 0;
                            if ok && request[..read as usize].starts_with(SHOW) {
                                on_show();
                            }
                            DisconnectNamedPipe(pipe);
                        }
                        CloseHandle(pipe);
                    }
                }
            });
        }

        pub fn send(request: &[u8]) -> std::io::Result<()> {
            std::fs::OpenOptions::new()
                .write(true)
                .open(name())?
                .write_all(request)
        }
    }
}

pub mod autostart {
    //! Starting VibeFuse at login, in the tray without a window: a LaunchAgent
    //! on macOS, the `Run` key of the registry on Windows. Switching it only changes the registration; the running app
    //! is left alone.

    use std::path::PathBuf;

    use crate::tr;

    /// The argument the login item starts the app with.
    pub const BACKGROUND: &str = "--background";

    /// The detail line of the setting.
    pub fn description() -> String {
        if cfg!(target_os = "macos") {
            tr!("Starts hidden in the menu bar.")
        } else {
            tr!("Starts hidden in the tray.")
        }
    }

    fn executable() -> Result<PathBuf, String> {
        std::env::current_exe().map_err(|e| format!("no path to the program: {e}"))
    }

    #[cfg(not(windows))]
    fn write(path: &std::path::Path, text: &str) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    #[cfg(not(windows))]
    fn remove(path: Option<PathBuf>) -> Result<(), String> {
        match path {
            Some(path) if path.exists() => std::fs::remove_file(path).map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }

    #[cfg(target_os = "macos")]
    mod platform {
        use super::*;

        const LABEL: &str = "cc.yeru.vibefuse.login";

        fn plist() -> Option<PathBuf> {
            Some(
                dirs::home_dir()?
                    .join("Library/LaunchAgents")
                    .join(format!("{LABEL}.plist")),
            )
        }

        pub fn is_enabled() -> bool {
            plist().is_some_and(|path| path.exists())
        }

        /// launchd reads the file at the next login; loading it now would start
        /// a second copy.
        pub fn enable() -> Result<(), String> {
            let path = plist().ok_or("no home folder")?;
            let exe = executable()?;
            let escape = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            };
            let text = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
    <dict>
    	<key>Label</key>
    	<string>{LABEL}</string>
    	<key>ProgramArguments</key>
    	<array>
    		<string>{}</string>
    		<string>{BACKGROUND}</string>
    	</array>
    	<key>RunAtLoad</key>
    	<true/>
    	<key>LimitLoadToSessionType</key>
    	<string>Aqua</string>
    	<key>ProcessType</key>
    	<string>Interactive</string>
    </dict>
    </plist>
    "#,
                escape(&exe.to_string_lossy())
            );
            write(&path, &text)
        }

        pub fn disable() -> Result<(), String> {
            remove(plist())
        }
    }

    #[cfg(windows)]
    mod platform {
        use std::process::{Command, Stdio};

        use super::*;

        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        const VALUE: &str = "VibeFuse";

        fn reg(args: &[&str]) -> Result<(), String> {
            use std::os::windows::process::CommandExt;
            /// `CREATE_NO_WINDOW`: no console flashes up.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let output = Command::new("reg")
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|e| format!("reg: {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
        }

        pub fn is_enabled() -> bool {
            reg(&["query", KEY, "/v", VALUE]).is_ok()
        }

        pub fn enable() -> Result<(), String> {
            let command = format!("\"{}\" {BACKGROUND}", executable()?.display());
            reg(&[
                "add", KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &command, "/f",
            ])
        }

        pub fn disable() -> Result<(), String> {
            if is_enabled() {
                reg(&["delete", KEY, "/v", VALUE, "/f"])
            } else {
                Ok(())
            }
        }
    }

    pub use platform::{disable, enable, is_enabled};
}

#[cfg(target_os = "macos")]
pub mod dock {
    //! macOS: the Dock icon is there only while the window is, and opening the
    //! app again (Finder, Spotlight, Launchpad) while it sits in the menu bar
    //! brings the window back. winit handles neither, so this talks to AppKit.

    use std::sync::OnceLock;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, NSObject};
    use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send, sel};

    /// `NSApplicationActivationPolicyRegular` and `…Accessory`.
    const POLICY_REGULAR: isize = 0;
    const POLICY_ACCESSORY: isize = 1;
    /// The "reopen application" Apple Event: `kCoreEventClass`, `kAEReopenApplication`.
    const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
    const REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");

    static ON_REOPEN: OnceLock<fn()> = OnceLock::new();

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "VibeFuseReopenHandler"]
        struct ReopenHandler;

        impl ReopenHandler {
            #[unsafe(method(handleReopen:withReplyEvent:))]
            fn handle_reopen(&self, _event: &AnyObject, _reply: &AnyObject) {
                if let Some(reopen) = ON_REOPEN.get() {
                    reopen();
                }
            }
        }
    );

    /// The app object; AppKit may only be used from the main thread (tests run
    /// elsewhere).
    fn application() -> Option<Retained<AnyObject>> {
        MainThreadMarker::new()?;
        let class = AnyClass::get(c"NSApplication")?;
        // SAFETY: `sharedApplication` returns the app object, creating it if needed.
        unsafe { msg_send![class, sharedApplication] }
    }

    /// Shows or hides the Dock icon; showing it also brings the app to the front.
    pub fn show_in_dock(visible: bool) {
        let Some(app) = application() else { return };
        let policy = if visible {
            POLICY_REGULAR
        } else {
            POLICY_ACCESSORY
        };
        // SAFETY: documented NSApplication methods, on the main thread.
        unsafe {
            let _: bool = msg_send![&*app, setActivationPolicy: policy];
            if visible {
                let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
            }
        }
    }

    /// Calls `reopen` when the app is opened again while it runs. Call once the
    /// event loop runs: AppKit installs its own handler while launching.
    pub fn on_reopen(reopen: fn()) {
        if ON_REOPEN.set(reopen).is_err() {
            return;
        }
        let Some(class) = AnyClass::get(c"NSAppleEventManager") else {
            return;
        };
        // SAFETY: a plain NSObject subclass; the handler lives as long as the
        // process (forgotten below), as the event manager does not retain it.
        unsafe {
            let handler: Retained<ReopenHandler> = msg_send![ReopenHandler::alloc(), init];
            let manager: Option<Retained<AnyObject>> = msg_send![class, sharedAppleEventManager];
            if let Some(manager) = manager {
                let _: () = msg_send![
                    &*manager,
                    setEventHandler: &*handler,
                    andSelector: sel!(handleReopen:withReplyEvent:),
                    forEventClass: CORE_EVENT_CLASS,
                    andEventID: REOPEN_APPLICATION
                ];
            }
            std::mem::forget(handler);
        }
    }
}
