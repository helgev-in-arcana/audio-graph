//! A VST3 host backend, in pure Rust (ADR-1).
//!
//! Scope discipline (ARCHITECTURE.md §7): this crate knows how to load and run
//! a VST3 plugin and nothing about *why*. Anything specific to hosting a plugin
//! from inside another plugin — forwarding the DAW's transport, combining
//! latency, nesting state — lives in `subhost-adapter`. The test for whether
//! code belongs here is whether an offline renderer or a plugin scanner would
//! still need it.
//!
//! Consequently the crate never constructs an `IHostApplication` of its own;
//! host services are injected through [`plugin_host_api::HostContext`].

mod cid;
mod host_app;
mod library;
mod module;
mod moduleinfo;
mod param_map;
mod plugin;
mod process_io;
mod stream;
mod util;
mod vst_events;

pub use cid::Cid;
pub use module::{ClassInfo, FactoryInfo, Module, scan_without_loading};
pub use moduleinfo::{ModuleClass, ModuleInfo, ModuleInfoError};
pub use plugin::{Vst3Plugin, Vst3Processor};

/// The file extension of a VST3 module, bundle or bare library alike.
pub const VST3_EXTENSION: &str = "vst3";

/// Directories the OS conventionally keeps VST3 plugins in.
///
/// Used to re-resolve a saved binding whose recorded path has moved (§8.3), and
/// by the scanner CLI.
pub fn default_plugin_directories() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Ok(pf) = std::env::var("CommonProgramFiles") {
            dirs.push(std::path::PathBuf::from(pf).join("VST3"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(std::path::PathBuf::from(local).join("Programs").join("Common").join("VST3"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        dirs.push(std::path::PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(std::path::PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(std::path::PathBuf::from(&home).join(".vst3"));
        }
        dirs.push(std::path::PathBuf::from("/usr/lib/vst3"));
        dirs.push(std::path::PathBuf::from("/usr/local/lib/vst3"));
    }

    dirs.retain(|d| d.is_dir());
    dirs
}

/// List the `.vst3` modules directly inside `dir`.
///
/// Not recursive by default because vendors nest their own subfolders and a
/// deep walk turns a scan into a filesystem crawl; [`find_modules`] handles the
/// one level of vendor subdirectory that is conventional.
pub fn list_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == VST3_EXTENSION))
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
            .filter(|p| p.is_dir() && p.extension().is_none_or(|e| e != VST3_EXTENSION))
            .collect();
        subdirs.sort();
        for sub in subdirs {
            out.extend(list_modules(&sub));
        }
    }
    out
}
