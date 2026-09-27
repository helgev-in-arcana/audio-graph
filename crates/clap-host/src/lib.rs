//! CLAP plugin hosting implementation in pure Rust.
//!
//! Provides discovery, loading, instantiation, parameter management, audio/event
//! processing, and GUI embedding for CLAP (CLever Audio Plug-in) format plugins.

mod events;
mod gui;
mod host;
mod library;
mod module;
mod plugin;
mod stream;
mod util;

pub use module::{ClassInfo, FactoryInfo, Module};
pub use plugin::{ClapPlugin, ClapProcessor};

/// The file extension of a CLAP module.
pub const CLAP_EXTENSION: &str = "clap";

/// List the `.clap` modules directly inside `dir`.
///
/// Not recursive, for the same reason as the VST3 scanner's: vendors nest their
/// own subfolders and a deep walk turns a scan into a filesystem crawl.
/// [`find_modules`] handles the one conventional level.
pub fn list_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == CLAP_EXTENSION))
        .collect();
    out.sort();
    out
}

/// Modules in `dir` plus those one level down, which is how vendors group them.
pub fn find_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = list_modules(dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut subdirs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.extension().is_none_or(|e| e != CLAP_EXTENSION))
            .collect();
        subdirs.sort();
        for sub in subdirs {
            out.extend(list_modules(&sub));
        }
    }
    out
}
