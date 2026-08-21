//! A CLAP host backend, in pure Rust.
//!
//! The same scope discipline as `vst3-host` (ARCHITECTURE.md §7): this crate
//! knows how to load and run a CLAP plugin and nothing about *why*. Anything
//! specific to hosting a plugin from inside another plugin lives in
//! `subhost-adapter`, and the format-agnostic facade both backends sit behind
//! is `plugin-host`.
//!
//! ADR-1 applies unchanged. `clap-sys` is a dependency-free set of `#[repr(C)]`
//! declarations — the C header transcribed, with no C toolchain involved — so
//! using it is the CLAP equivalent of using the `vst3` crate's bindings rather
//! than a step away from pure Rust.
//!
//! Two structural differences from the VST3 backend are worth knowing before
//! reading further:
//!
//! * **Buses are declared, not negotiated.** There is no `setBusArrangements`;
//!   a port is the width it says it is. See `plugin::bind_ports`.
//! * **The editor lives inside the instance.** CLAP's GUI is an extension on
//!   the plugin rather than a separate object, so `ClapPlugin` owns its editor
//!   and the §5.3 teardown order is guaranteed by its `Drop` rather than by the
//!   caller declaring fields in the right sequence.

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

/// Directories the OS conventionally keeps CLAP plugins in.
///
/// `CLAP_PATH` comes first when it is set: the format specifies it, and a
/// developer pointing at a build directory expects it to win over an installed
/// copy of the same plugin.
pub fn default_plugin_directories() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();

    if let Some(path) = std::env::var_os("CLAP_PATH") {
        dirs.extend(std::env::split_paths(&path));
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(pf) = std::env::var("CommonProgramFiles") {
            dirs.push(std::path::PathBuf::from(pf).join("CLAP"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(
                std::path::PathBuf::from(local)
                    .join("Programs")
                    .join("Common")
                    .join("CLAP"),
            );
        }
    }

    #[cfg(target_os = "macos")]
    {
        dirs.push(std::path::PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(std::path::PathBuf::from(home).join("Library/Audio/Plug-Ins/CLAP"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(std::path::PathBuf::from(&home).join(".clap"));
        }
        dirs.push(std::path::PathBuf::from("/usr/lib/clap"));
        dirs.push(std::path::PathBuf::from("/usr/local/lib/clap"));
    }

    dirs.retain(|d| d.is_dir());
    dirs.dedup();
    dirs
}

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
