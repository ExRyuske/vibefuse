//! Loading a VST3 module and getting its factory. A module is a bundle,
//! `Name.vst3`, with the binary in `Contents/MacOS` or
//! `Contents/x86_64-win`; old Windows plug-ins are a single file.
//!
//! A module, once loaded, stays loaded. Plug-ins leave work behind on
//! system queues (license and update checks) that calls into them after
//! their instance is gone; with the module shut down that crashes the app.

#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vst3::ComPtr;
use vst3::Steinberg::IPluginFactory;

use crate::fx::Result;

type GetPluginFactory = unsafe extern "system" fn() -> *mut IPluginFactory;
type ExitFn = unsafe extern "system" fn() -> bool;

/// A loaded module. Instances made from it must go before it does.
pub struct Module {
    // Fields drop in order: the factory before the exit call, the library
    // last.
    factory: Option<ComPtr<IPluginFactory>>,
    path: PathBuf,
    exit: Option<ExitFn>,
    #[cfg(target_os = "macos")]
    _bundle: mac::Bundle,
    _library: libloading::Library,
}

// SAFETY: the factory is only called on the window's thread; the audio
// thread's instance merely keeps the module loaded, and the last reference
// goes on the window's thread too.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

/// Every module loaded so far.
static LOADED: Mutex<Vec<Arc<Module>>> = Mutex::new(Vec::new());

impl Module {
    /// The module at `path`, loaded the first time.
    pub fn shared(path: &Path) -> Result<Arc<Self>> {
        let mut loaded = LOADED.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(module) = loaded.iter().find(|m| m.path == path) {
            return Ok(Arc::clone(module));
        }
        let module = Arc::new(Self::load(path)?);
        loaded.push(Arc::clone(&module));
        Ok(module)
    }

    /// `path` is the `.vst3` bundle, or the binary of an old plug-in.
    fn load(path: &Path) -> Result<Self> {
        let binary = binary(path)?;
        let fail = |reason: &str| format!("{}: {reason}", path.display());
        // SAFETY: loading foreign code is what hosting plug-ins is.
        let library =
            unsafe { libloading::Library::new(&binary) }.map_err(|e| fail(&e.to_string()))?;
        #[cfg(target_os = "macos")]
        let bundle = mac::Bundle::open(path);
        // SAFETY: the entry points' signatures are VST3's; the entry is
        // optional and must come before the factory.
        unsafe {
            #[cfg(target_os = "macos")]
            if let Ok(entry) =
                library.get::<unsafe extern "C" fn(*const c_void) -> bool>(b"bundleEntry\0")
                && !entry(bundle.as_ptr())
            {
                return Err(fail("the module did not start"));
            }
            #[cfg(windows)]
            if let Ok(entry) = library.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0")
                && !entry()
            {
                return Err(fail("the module did not start"));
            }
        }
        let exit_name: &[u8] = if cfg!(target_os = "macos") {
            b"bundleExit\0"
        } else {
            b"ExitDll\0"
        };
        // SAFETY: as above.
        let exit = unsafe { library.get::<ExitFn>(exit_name).ok().map(|s| *s) };
        // SAFETY: as above; the factory comes with a reference for us.
        let factory = unsafe {
            let get = library
                .get::<GetPluginFactory>(b"GetPluginFactory\0")
                .map_err(|_| fail("not a VST3 plug-in"))?;
            ComPtr::from_raw(get()).ok_or_else(|| fail("the module has no factory"))?
        };
        Ok(Self {
            factory: Some(factory),
            path: path.to_path_buf(),
            exit,
            #[cfg(target_os = "macos")]
            _bundle: bundle,
            _library: library,
        })
    }

    pub fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory.as_ref().expect("taken only when dropped")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // The exit call comes once no pointer into the module is left.
        drop(self.factory.take());
        if let Some(exit) = self.exit {
            // SAFETY: once, after the factory, before the library unloads.
            unsafe { exit() };
        }
    }
}

/// The binary for this system inside a bundle.
fn binary(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    #[cfg(target_os = "macos")]
    let (dir, extension) = (path.join("Contents").join("MacOS"), None);
    #[cfg(all(windows, target_arch = "x86_64"))]
    let (dir, extension) = (path.join("Contents").join("x86_64-win"), Some("vst3"));
    #[cfg(all(windows, target_arch = "aarch64"))]
    let (dir, extension) = (path.join("Contents").join("arm64-win"), Some("vst3"));
    let stem = path.file_stem().unwrap_or_default();
    let mut named = dir.join(stem);
    if let Some(extension) = extension {
        named.set_extension(extension);
    }
    if named.is_file() {
        return Ok(named);
    }
    std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .find(|p| {
            p.is_file()
                && extension.is_none_or(|x| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case(x))
                })
        })
        .ok_or_else(|| format!("no binary in {}", dir.display()))
}

#[cfg(target_os = "macos")]
mod mac {
    //! `bundleEntry` takes the module's `CFBundleRef`: plug-ins find their
    //! resources through it.

    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: *const c_void,
            path: *const u8,
            length: isize,
            directory: u8,
        ) -> *const c_void;
        fn CFBundleCreate(allocator: *const c_void, url: *const c_void) -> *const c_void;
        fn CFRelease(object: *const c_void);
    }

    pub struct Bundle(*const c_void);

    impl Bundle {
        pub fn open(path: &Path) -> Self {
            let bytes = path.as_os_str().as_bytes();
            // SAFETY: plain CoreFoundation calls; both results are owned.
            unsafe {
                let url = CFURLCreateFromFileSystemRepresentation(
                    std::ptr::null(),
                    bytes.as_ptr(),
                    bytes.len() as isize,
                    1,
                );
                if url.is_null() {
                    return Self(std::ptr::null());
                }
                let bundle = CFBundleCreate(std::ptr::null(), url);
                CFRelease(url);
                Self(bundle)
            }
        }

        pub fn as_ptr(&self) -> *const c_void {
            self.0
        }
    }

    impl Drop for Bundle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: owned since `open`.
                unsafe { CFRelease(self.0) };
            }
        }
    }
}
