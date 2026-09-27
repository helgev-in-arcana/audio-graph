//! Module inspection: what classes one module exports.
//!
//! Everything here is format-agnostic on the outside: one [`ClassInfo`]. The differences the two backends
//! have — a VST3 module exports several classes of which only some are
//! instantiable, a CLAP module exports plugins and nothing else — are resolved
//! on this side of the boundary rather than by the caller.

use std::path::{Path, PathBuf};

use plugin_host_api::Result;

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

/// Suppress an unused-import warning where only one format is compiled.
#[allow(dead_code)]
const _: [Format; 2] = FORMATS;
