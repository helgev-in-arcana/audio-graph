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

/// The `.clap` modules directly inside `dir`. See [`plugin_module::list_modules`].
pub fn list_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    plugin_module::list_modules(dir, CLAP_EXTENSION)
}

/// The `.clap` modules in `dir` and one level down. See
/// [`plugin_module::find_modules`].
pub fn find_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    plugin_module::find_modules(dir, CLAP_EXTENSION)
}
