//! Plugin discovery and module inspection.
//!
//! Everything here is format-agnostic on the outside: one list of directories,
//! one list of modules, one [`ClassInfo`]. The differences the two backends
//! have — a VST3 module exports several classes of which only some are
//! instantiable, a CLAP module exports plugins and nothing else — are resolved
//! on this side of the boundary rather than by the caller.

use std::path::{Path, PathBuf};

use plugin_host_api::Result;

use crate::catalogue;
use crate::format::{FORMATS, Format};

/// Information describing a single plugin class exported by a module.
///
/// The union of what the two formats say about themselves, narrowed to what a
/// browser and a saved binding actually need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub format: Format,
    /// Stable identity, and the authority a saved binding is resolved by. A
    /// VST3 class id in platform-independent hex, or a CLAP reverse-DNS id —
    /// opaque either way.
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// The format's own classification, joined with `|`: VST3 subcategories,
    /// or CLAP feature tags. For display and filtering only.
    pub category: String,
    pub is_instrument: bool,
    /// The module it was found in.
    pub path: PathBuf,
}

/// Serialized reference used to locate a plugin across sessions and machines.
///
/// The path is a hint and the id is the authority, because plugin folders
/// differ between machines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRef {
    pub format: Format,
    pub id: String,
    pub path_hint: PathBuf,
    pub display_name: String,
}

impl ClassInfo {
    pub fn reference(&self) -> PluginRef {
        PluginRef {
            format: self.format,
            id: self.id.clone(),
            path_hint: self.path.clone(),
            display_name: self.name.clone(),
        }
    }
}

/// Every directory a scan should look in, as the user's settings have it.
///
/// The supplied directory list is the whole answer: which folders to look in
/// is the caller's to say, including the ones a platform conventionally keeps
/// plugins in. Nothing here adds any of its own.
///
/// Each directory is paired with every format, because the user pointed at a
/// folder of plugins and not at a folder of VST3s.
///
/// Directories that do not exist are dropped: a folder can be on a drive that
/// is not plugged in today, and a scan should be quiet about that rather than
/// fail.
pub fn plugin_directories(directories: &[PathBuf]) -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for dir in directories {
        if !dir.is_dir() {
            continue;
        }
        for format in FORMATS {
            out.push((format, dir.clone()));
        }
    }
    // A list that names the same folder twice should not list every plugin in
    // it twice.
    out.sort();
    out.dedup();
    out
}

/// Returns all plugin modules of the given `format` found in `dir` or its immediate subdirectories.
pub fn find_modules(format: Format, dir: &Path) -> Vec<PathBuf> {
    match format {
        Format::Vst3 => vst3_host::find_modules(dir),
        Format::Clap => clap_host::find_modules(dir),
    }
}

/// Every module of every format in every directory a scan covers.
///
/// Paths only: enumerating the classes inside means loading third-party code,
/// which is a decision the caller should make deliberately.
pub fn installed_modules(directories: &[PathBuf]) -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for (format, dir) in plugin_directories(directories) {
        for path in find_modules(format, &dir) {
            out.push((format, path));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Inspects `path` to list exported plugin classes and unloads the module.
///
/// The format is taken from the extension; a path that is neither is an error
/// rather than a guess.
pub fn scan_module(path: &Path) -> Result<Vec<ClassInfo>> {
    let format = Format::from_path(path).ok_or_else(|| {
        plugin_host_api::HostError::ModuleLoad(format!("{} is not a plugin module", path.display()))
    })?;
    scan_module_as(format, path)
}

/// As [`scan_module`], for a caller that already knows the format.
pub fn scan_module_as(format: Format, path: &Path) -> Result<Vec<ClassInfo>> {
    match format {
        Format::Vst3 => {
            let module = vst3_host::Module::open(path)?;
            Ok(module
                .audio_modules()?
                .into_iter()
                .map(|c| ClassInfo {
                    format,
                    is_instrument: c.is_instrument(),
                    id: c.cid.to_hex(),
                    name: c.name,
                    vendor: c.vendor,
                    version: c.version,
                    category: c.subcategories,
                    path: path.to_path_buf(),
                })
                .collect())
        }
        Format::Clap => {
            let module = clap_host::Module::open(path)?;
            Ok(module
                .classes()?
                .into_iter()
                .map(|c| ClassInfo {
                    format,
                    is_instrument: c.is_instrument(),
                    category: c.features.join("|"),
                    id: c.id,
                    name: c.name,
                    vendor: c.vendor,
                    version: c.version,
                    path: path.to_path_buf(),
                })
                .collect())
        }
    }
}

/// Where a saved [`PluginRef`] may be found, most likely first, without
/// loading anything.
///
/// The id is the authority and the path only a hint, so a project that moved
/// between machines still opens. The hint comes first when the file is still
/// there; after it, every module the catalogue says exports the id, those
/// whose file is unchanged since they were scanned ahead of those it has
/// changed under.
///
/// Nothing here opens a module, and nothing searches the disk: a module the
/// catalogue has not seen is not a candidate. Asking every module on the
/// machine whether it is the one would load third-party code by the hundred,
/// on whatever thread is restoring the project — the DAW's main thread — and
/// a scan belongs on a thread of its own. A reference with no candidate stays
/// unresolved until a scan has found its module.
///
/// The caller loads the candidates in turn with the id and keeps the first
/// that takes, which is also what checks the hint: a file at the hint's path
/// that no longer exports the id fails to load and the next candidate is
/// tried.
pub fn reference_candidates(reference: &PluginRef, known: &[catalogue::Module]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if reference.path_hint.exists() {
        out.push(reference.path_hint.clone());
    }
    let mut listed: Vec<(bool, &PathBuf)> = known
        .iter()
        .filter(|module| {
            module.format == reference.format
                && module.error.is_none()
                && module.classes.iter().any(|class| class.id == reference.id)
                && module.path.exists()
        })
        .map(|module| {
            (
                catalogue::stamp_of(&module.path) != module.stamp,
                &module.path,
            )
        })
        .collect();
    // Stable, so modules with the same freshness keep the catalogue's order.
    listed.sort_by_key(|&(stale, _)| stale);
    for (_, path) in listed {
        if !out.contains(path) {
            out.push(path.clone());
        }
    }
    out
}

/// Suppress an unused-import warning where only one format is compiled.
#[allow(dead_code)]
const _: [Format; 2] = FORMATS;
