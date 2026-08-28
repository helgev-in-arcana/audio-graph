//! Persistent cache of installed plugin module metadata between runs.
//!
//! # Caching Behavior
//!
//! Determining whether a module is an effect or instrument requires inspecting
//! format-specific metadata (`PClassInfo2::subCategories` for VST3, feature tags
//! for CLAP) by loading the library and invoking its entry point. To avoid the
//! performance cost and instability of dynamically loading every module on every
//! scan, module metadata is cached to disk in `plugins.json`.
//!
//! # Cache Invalidation
//!
//! Cache entries are validated against a [`Stamp`] consisting of the module's
//! latest file modification time and total byte size. For directory bundles (such
//! as VST3 bundles), the stamp traverses the bundle to record the newest file
//! modification time and aggregate size.
//!
//! Modules that fail to load are recorded as failed along with their stamp to
//! prevent repeated failed load attempts on subsequent scans.
//!
//! # Storage
//!
//! The cache is stored adjacent to the configuration file as `plugins.json`.
//! It is treated as derived data separate from user configuration.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::format::Format;

/// High-level classification of a plugin module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Effect,
    Instrument,
    /// Not yet scanned or failed to open.
    Unknown,
}

/// One class a module exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Class {
    /// Stable identity (VST3 class id in hex, or CLAP reverse-DNS id).
    pub id: String,
    pub name: String,
    /// Format-specific classification tags joined with `|`.
    pub category: String,
    pub is_instrument: bool,
}

/// A scanned plugin module and its exported classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    pub path: PathBuf,
    pub format: Format,
    /// File timestamp and size recorded when the module was scanned.
    pub stamp: Stamp,
    pub classes: Vec<Class>,
    /// Error message if the module failed to open or scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Module {
    /// Returns the file name of the module path.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }

    pub fn kind(&self) -> Kind {
        if self.error.is_some() || self.classes.is_empty() {
            return Kind::Unknown;
        }
        if self.classes.iter().any(|c| c.is_instrument) {
            Kind::Instrument
        } else {
            Kind::Effect
        }
    }
}

/// Modification timestamp (seconds since Unix epoch) and total byte size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub modified: u64,
    pub size: u64,
}

/// Computes the stamp for a file or directory bundle at `path`.
///
/// For directory bundles, walks the tree to find the newest modification
/// timestamp and sums total file sizes. Unreadable entries are skipped.
pub fn stamp_of(path: &Path) -> Stamp {
    fn secs(time: SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    let mut stamp = Stamp::default();
    let Ok(meta) = std::fs::metadata(path) else {
        return stamp;
    };
    if let Ok(modified) = meta.modified() {
        stamp.modified = secs(modified);
    }
    if !meta.is_dir() {
        stamp.size = meta.len();
        return stamp;
    }

    // A bundle. Depth is not bounded because a VST3 bundle is a fixed shallow
    // shape — `Contents/<arch>/` and a few resource folders — and a symlink
    // loop inside one would break the loader long before it got here.
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if let Ok(modified) = meta.modified() {
                stamp.modified = stamp.modified.max(secs(modified));
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                stamp.size = stamp.size.saturating_add(meta.len());
            }
        }
    }
    stamp
}

/// The whole cache, as the file holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Cache {
    modules: Vec<Module>,
}

/// Where the cache lives: beside the settings file, whatever the settings file
/// turned out to be — which is what keeps a test's cache in the test's own
/// directory.
pub fn cache_path() -> Option<PathBuf> {
    Some(crate::config::config_path()?.with_file_name("plugins.json"))
}

/// Reads the cached module list from disk without performing a scan.
///
/// Returns an empty list if the cache file is missing or unreadable.
pub fn cached() -> Vec<Module> {
    let Some(path) = cache_path() else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Cache>(&bytes) {
        Ok(cache) => cache.modules,
        Err(e) => {
            log::warn!(
                "audio-graph: {} is not a readable cache: {e}",
                path.display()
            );
            Vec::new()
        }
    }
}

/// Refreshes the cache by rescanning new or modified modules and saving results.
///
/// Only modules whose stamp has changed or which are not in the cache are rescanned.
/// Modules no longer found in scanned directories are removed.
///
/// # Safety / Threading
/// Dynamically loads plugin libraries; should be called off the UI thread after
/// initializing the calling thread with [`crate::init_thread`].
pub fn refresh() -> Vec<Module> {
    let known = cached();
    let mut out = Vec::new();

    for (format, path) in crate::scan::installed_modules() {
        let stamp = stamp_of(&path);
        // Unchanged since we looked: keep what we know, including the fact that
        // it could not be opened.
        if let Some(hit) = known
            .iter()
            .find(|m| m.path == path && m.stamp == stamp && m.format == format)
        {
            out.push(hit.clone());
            continue;
        }
        out.push(scan_one(format, &path, stamp));
    }

    // Sorted so the file is stable between runs.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    if let Err(e) = store(&out) {
        log::warn!("audio-graph: the plugin cache could not be saved: {e}");
    }
    out
}

/// Scans a single module and constructs a [`Module`] entry.
fn scan_one(format: Format, path: &Path, stamp: Stamp) -> Module {
    match crate::scan::scan_module_as(format, path) {
        Ok(classes) => Module {
            path: path.to_path_buf(),
            format,
            stamp,
            classes: classes
                .into_iter()
                .map(|c| Class {
                    id: c.id,
                    name: c.name,
                    category: c.category,
                    is_instrument: c.is_instrument,
                })
                .collect(),
            error: None,
        },
        Err(e) => {
            log::warn!("audio-graph: {} could not be scanned: {e}", path.display());
            Module {
                path: path.to_path_buf(),
                format,
                stamp,
                classes: Vec::new(),
                error: Some(e.to_string()),
            }
        }
    }
}

/// Writes the module cache to disk atomically via a temporary file.
fn store(modules: &[Module]) -> Result<(), String> {
    let path = cache_path().ok_or_else(|| "no config directory on this platform".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let cache = Cache {
        modules: modules.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&cache).map_err(|e| format!("encoding cache: {e}"))?;

    let temp = path.with_extension("json.new");
    std::fs::write(&temp, &json).map_err(|e| format!("writing {}: {e}", temp.display()))?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("replacing {}: {e}", path.display())
    })
}

/// Removes the on-disk cache file so subsequent refreshes rescan all modules.
pub fn forget() -> Result<(), String> {
    let Some(path) = cache_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", path.display())),
    }
}
