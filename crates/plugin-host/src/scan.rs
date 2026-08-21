//! Finding plugins, and describing what a module contains.
//!
//! Everything here is format-agnostic on the outside: one list of directories,
//! one list of modules, one `ClassInfo`. The differences the two backends have
//! — a VST3 module exports several classes of which only some are instantiable,
//! a CLAP module exports plugins and nothing else — are resolved on this side of
//! the boundary rather than by the caller.

use std::path::{Path, PathBuf};

use plugin_host_api::Result;

use crate::format::{FORMATS, Format};

/// One plugin a module offers.
///
/// The union of what the two formats say about themselves, narrowed to what a
/// browser and a saved binding actually need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub format: Format,
    /// Stable identity, and the authority a saved binding is resolved by
    /// (§8.3). A VST3 class id in platform-independent hex, or a CLAP
    /// reverse-DNS id — opaque either way.
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// The format's own classification, joined with `|`: VST3 subcategories, or
    /// CLAP feature tags. For display and filtering only.
    pub category: String,
    pub is_instrument: bool,
    /// The module it was found in.
    pub path: PathBuf,
}

/// Where a plugin was last found, as saved into a project.
///
/// The path is a hint and the id is the authority, because plugin folders
/// differ between machines (§8.3).
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

/// Directories the OS conventionally keeps plugins in, every format together.
pub fn default_plugin_directories() -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for dir in vst3_host::default_plugin_directories() {
        out.push((Format::Vst3, dir));
    }
    for dir in clap_host::default_plugin_directories() {
        out.push((Format::Clap, dir));
    }
    out
}

/// Modules of `format` in `dir` and one level below it.
pub fn find_modules(format: Format, dir: &Path) -> Vec<PathBuf> {
    match format {
        Format::Vst3 => vst3_host::find_modules(dir),
        Format::Clap => clap_host::find_modules(dir),
    }
}

/// Every module of every format in the conventional directories.
///
/// Paths only: enumerating the classes inside means loading third-party code,
/// which is a decision the caller should make deliberately.
pub fn installed_modules() -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for (format, dir) in default_plugin_directories() {
        for path in find_modules(format, &dir) {
            out.push((format, path));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Load `path` far enough to list what it offers, then unload it.
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

/// Re-find a plugin whose recorded path no longer exists.
///
/// The id is the authority and the path only a hint, so a project that moved
/// between machines still opens (§8.3). Loads each candidate module to ask it,
/// which is why the hint is tried first.
pub fn resolve_reference(reference: &PluginRef) -> Option<PathBuf> {
    if reference.path_hint.exists()
        && scan_module_as(reference.format, &reference.path_hint)
            .is_ok_and(|classes| classes.iter().any(|c| c.id == reference.id))
    {
        return Some(reference.path_hint.clone());
    }

    for (format, dir) in default_plugin_directories() {
        if format != reference.format {
            continue;
        }
        for candidate in find_modules(format, &dir) {
            let Ok(classes) = scan_module_as(format, &candidate) else {
                continue;
            };
            if classes.iter().any(|c| c.id == reference.id) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Suppress an unused-import warning where only one format is compiled.
#[allow(dead_code)]
const _: [Format; 2] = FORMATS;
