//! Plugin discovery and module inspection.
//!
//! Provides format-agnostic functions for finding plugin binaries in configured
//! directories, scanning exported classes, and resolving saved references.

use std::path::{Path, PathBuf};

use plugin_host_api::Result;

use crate::format::{FORMATS, Format};

/// Information describing a single plugin class exported by a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub format: Format,
    /// Stable plugin identifier (e.g. VST3 class ID in hex or CLAP reverse-DNS ID).
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// Format-specific classification string (e.g. subcategories or feature tags) joined with `|`.
    pub category: String,
    pub is_instrument: bool,
    /// Path to the module file or bundle.
    pub path: PathBuf,
}

/// Serialized reference used to locate a plugin across sessions and machines.
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

/// Returns the standard platform default directories for installed plugins.
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

/// Returns the active set of directories to scan across all supported formats based on user configuration.
///
/// Non-existent directories are omitted, and duplicates are removed.
pub fn plugin_directories() -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for dir in crate::config::directories() {
        if !dir.is_dir() {
            continue;
        }
        for format in FORMATS {
            out.push((format, dir.clone()));
        }
    }
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

/// Enumerates all plugin module paths across all configured directories without loading them.
pub fn installed_modules() -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for (format, dir) in plugin_directories() {
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
/// Infers format from the file extension.
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

/// Resolves a saved [`PluginRef`] to an existing module path.
///
/// Tries `path_hint` first if it exists and contains the matching plugin ID.
/// If not found, searches default plugin directories for a matching module.
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
