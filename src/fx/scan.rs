//! Finding plug-in files.
//!
//! Probing loads a plug-in into this process, and a broken one can take the
//! process down with it. So results are cached (a file that has not changed
//! is not opened again), and the file being probed is written down first: if
//! the app dies on it, the next start skips it until it changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::graph::PluginFormat;
use super::{Descriptor, Format};

/// Where each format installs its plug-ins.
#[cfg(target_os = "macos")]
pub fn default_paths(format: Format) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let library = |dir: &str| {
        vec![
            PathBuf::from("/Library/Audio/Plug-Ins").join(dir),
            home.join("Library/Audio/Plug-Ins").join(dir),
        ]
    };
    match format {
        Format::Vst3 => library("VST3"),
        Format::Vst2 => library("VST"),
        Format::Ladspa => {
            let mut paths: Vec<PathBuf> = std::env::var_os("LADSPA_PATH")
                .map(|value| std::env::split_paths(&value).collect())
                .unwrap_or_default();
            paths.extend(library("LADSPA"));
            paths.extend(
                [
                    ".ladspa",
                    "/usr/local/lib/ladspa",
                    "/opt/homebrew/lib/ladspa",
                ]
                .into_iter()
                .map(|dir| home.join(dir)),
            );
            paths
        }
        // Audio Units are registered with the system, not looked for.
        Format::Au => Vec::new(),
    }
}

/// Where each format installs its plug-ins. VST2 never had one place, so
/// its list is longer.
#[cfg(windows)]
pub fn default_paths(format: Format) -> Vec<PathBuf> {
    let var = |name: &str, fallback: &str| {
        PathBuf::from(std::env::var_os(name).unwrap_or_else(|| fallback.into()))
    };
    let program_files = var("ProgramFiles", r"C:\Program Files");
    let common = var("CommonProgramFiles", r"C:\Program Files\Common Files");
    let local = var("LOCALAPPDATA", "");
    match format {
        Format::Vst3 => vec![common.join("VST3"), local.join(r"Programs\Common\VST3")],
        Format::Vst2 => vec![
            program_files.join("VSTPlugins"),
            program_files.join(r"Steinberg\VSTPlugins"),
            common.join("VST2"),
            common.join(r"Steinberg\VST2"),
            local.join(r"Programs\Common\VST2"),
            PathBuf::from(r"C:\VSTPlugins"),
        ],
        // LADSPA has no place on Windows: only the folders the user adds.
        Format::Ladspa | Format::Au => Vec::new(),
    }
}

/// Files under `roots` that look like plug-ins of `format`. Bundles are
/// directories (`.vst3`, `.vst` on macOS) and are not entered.
pub fn candidates(format: Format, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        walk(root, format.extensions(), 0, &mut found);
    }
    found
}

fn walk(dir: &Path, extensions: &[&str], depth: usize, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 6;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        let matches = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.iter().any(|x| e.eq_ignore_ascii_case(x)));
        if matches {
            out.push(path);
        } else if path.is_dir() {
            walk(&path, extensions, depth + 1, out);
        }
    }
}

/// What earlier scans found, by file.
#[derive(Default)]
pub struct ScanCache {
    /// Where the cache and the trace of a running scan live; `None` keeps the
    /// cache in memory.
    dir: Option<PathBuf>,
    files: HashMap<(Format, PathBuf), CachedFile>,
    /// The file the last scan died on, to tell once.
    crashed: Option<PathBuf>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CachedFile {
    format: Format,
    path: PathBuf,
    /// Seconds since the epoch; 0 when unknown.
    modified: u64,
    /// Empty for a file that is no plug-in (VST2 folders hold other DLLs).
    plugins: Vec<Descriptor>,
    /// The app died probing it: skipped until it changes.
    #[serde(default)]
    crashed: bool,
}

/// A format and the folders to look in.
pub type ScanJob = (Box<dyn PluginFormat>, Vec<PathBuf>);

const CACHE_FILE: &str = "plugin-cache.json";
/// Holds the file being probed while a scan runs.
const MARKER_FILE: &str = "plugin-scan.json";

impl ScanCache {
    pub fn load() -> Self {
        Self::load_from(if cfg!(test) {
            None
        } else {
            crate::settings::app_dir()
        })
    }

    fn load_from(dir: Option<PathBuf>) -> Self {
        let mut cache = Self {
            dir,
            ..Self::default()
        };
        // A broken cache costs a fresh scan, nothing more.
        if let Some(text) = cache
            .file(CACHE_FILE)
            .and_then(|path| std::fs::read_to_string(path).ok())
        {
            let files: Vec<CachedFile> = serde_json::from_str(&text).unwrap_or_default();
            cache.files = files
                .into_iter()
                .map(|file| ((file.format, file.path.clone()), file))
                .collect();
        }
        let Some(marker) = cache.file(MARKER_FILE) else {
            return cache;
        };
        let Ok(text) = std::fs::read_to_string(&marker) else {
            return cache;
        };
        let _ = std::fs::remove_file(&marker);
        let Ok((format, path)) = serde_json::from_str::<(Format, PathBuf)>(&text) else {
            return cache;
        };
        let file = CachedFile {
            format,
            modified: modified(&path),
            path: path.clone(),
            plugins: Vec::new(),
            crashed: true,
        };
        cache.files.insert((format, path.clone()), file);
        cache.crashed = Some(path);
        cache
    }

    /// The file that ended the last scan, once.
    pub fn take_crashed(&mut self) -> Option<PathBuf> {
        self.crashed.take()
    }

    /// Looks through every job's folders. `fresh` opens every file again,
    /// except the ones that crashed a scan (that would crash this one).
    pub fn scan(&mut self, jobs: &[ScanJob], fresh: bool) -> Vec<Descriptor> {
        let marker = self.file(MARKER_FILE);
        let mut previous = std::mem::take(&mut self.files);
        let mut plugins = Vec::new();
        for (backend, roots) in jobs {
            let format = backend.format();
            for path in candidates(format, roots) {
                let key = (format, path);
                // Folders may be inside one another.
                if self.files.contains_key(&key) {
                    continue;
                }
                let modified = modified(&key.1);
                let file = match previous.remove(&key) {
                    Some(cached) if cached.modified == modified && (cached.crashed || !fresh) => {
                        cached
                    }
                    _ => probe(backend.as_ref(), &key.1, modified, marker.as_deref()),
                };
                plugins.extend(file.plugins.iter().cloned());
                self.files.insert(key, file);
            }
        }
        if let Some(marker) = &marker {
            let _ = std::fs::remove_file(marker);
        }
        self.save();
        plugins
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(name))
    }

    fn save(&self) {
        let Some(path) = self.file(CACHE_FILE) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let files: Vec<&CachedFile> = self.files.values().collect();
        if let Ok(text) = serde_json::to_string(&files) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// Opens a file and reads its plug-ins, leaving a trace in case it crashes.
fn probe(
    backend: &dyn PluginFormat,
    path: &Path,
    modified: u64,
    marker: Option<&Path>,
) -> CachedFile {
    let format = backend.format();
    if let Some(marker) = marker
        && let Ok(text) = serde_json::to_string(&(format, path))
    {
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(marker, text);
    }
    CachedFile {
        format,
        path: path.to_path_buf(),
        modified,
        plugins: backend.probe(path).unwrap_or_default(),
        crashed: false,
    }
}

fn modified(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::fx::Result;
    use crate::fx::graph::Loaded;

    /// A format that only counts how often it was opened.
    struct Counting(Rc<Cell<usize>>);

    impl PluginFormat for Counting {
        fn format(&self) -> Format {
            Format::Vst3
        }

        fn probe(&self, path: &Path) -> Result<Vec<Descriptor>> {
            self.0.set(self.0.get() + 1);
            Ok(vec![Descriptor {
                format: Format::Vst3,
                path: path.to_path_buf(),
                uid: "00".repeat(16),
                name: path.file_stem().unwrap().to_string_lossy().into_owned(),
                vendor: String::new(),
                is_instrument: false,
            }])
        }

        fn instantiate(&self, _: &Descriptor, _: f64) -> Result<Loaded> {
            unreachable!("scanning makes no instances")
        }
    }

    fn setup(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("vibefuse-test-scan-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let plugins = root.join("plugins");
        let cache = root.join("cache");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        for file in ["a.vst3", "b.vst3"] {
            std::fs::write(plugins.join(file), b"").unwrap();
        }
        (plugins, cache)
    }

    fn jobs(plugins: &Path) -> (Vec<ScanJob>, Rc<Cell<usize>>) {
        let probes = Rc::new(Cell::new(0));
        let backend: Box<dyn PluginFormat> = Box::new(Counting(Rc::clone(&probes)));
        (vec![(backend, vec![plugins.to_path_buf()])], probes)
    }

    fn names(found: &[Descriptor]) -> Vec<String> {
        let mut names: Vec<String> = found.iter().map(|d| d.name.clone()).collect();
        names.sort();
        names
    }

    #[test]
    fn unchanged_files_are_not_opened_again() {
        let (plugins, cache_dir) = setup("unchanged");
        let (jobs, probes) = jobs(&plugins);
        let mut cache = ScanCache::load_from(Some(cache_dir.clone()));
        assert_eq!(names(&cache.scan(&jobs, false)), ["a", "b"]);
        let mut restarted = ScanCache::load_from(Some(cache_dir));
        assert_eq!(names(&restarted.scan(&jobs, false)), ["a", "b"]);
        assert_eq!(probes.get(), 2);
        restarted.scan(&jobs, true);
        assert_eq!(probes.get(), 4, "a fresh scan opens everything");
    }

    #[test]
    fn the_file_that_crashed_the_scan_is_skipped() {
        let (plugins, cache_dir) = setup("crashed");
        let (jobs, probes) = jobs(&plugins);
        let culprit = plugins.join("a.vst3");
        std::fs::write(
            cache_dir.join(MARKER_FILE),
            serde_json::to_string(&(Format::Vst3, &culprit)).unwrap(),
        )
        .unwrap();
        let mut cache = ScanCache::load_from(Some(cache_dir.clone()));
        assert_eq!(cache.take_crashed(), Some(culprit));
        assert_eq!(cache.take_crashed(), None);
        assert_eq!(names(&cache.scan(&jobs, true)), ["b"]);
        assert_eq!(probes.get(), 1);
        assert!(!cache_dir.join(MARKER_FILE).exists());
    }
}
