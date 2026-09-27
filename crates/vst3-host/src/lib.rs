//! A VST3 host backend implementation in pure Rust.
//!
//! This crate handles loading, introspecting, and executing VST3 plugins.
//! Higher-level concerns such as DAW wrapping, multi-plugin graph orchestration,
//! and transport adaptation live in higher layers. Host services and policies
//! are injected through [`plugin_host_api::HostContext`].

// The `vst3` crate's constants are bindgen outputs whose types depend on
// platform C++ ABIs (`i32` on MSVC, `u32` elsewhere). The explicit casts
// are necessary for cross-platform builds.
#![allow(clippy::unnecessary_cast)]

mod cid;
mod com;
mod host_app;
mod library;
mod midi_map;
mod module;
mod moduleinfo;
mod param_map;
mod param_sync;
mod plugin;
mod process_io;
mod stream;
mod util;
mod vst_events;

pub use cid::Cid;
pub use com::{ApartmentGuard, init_apartment};
pub use module::{ClassInfo, FactoryInfo, Module, scan_without_loading};
pub use moduleinfo::{ModuleClass, ModuleInfo, ModuleInfoError};
pub use plugin::{Vst3Plugin, Vst3Processor, Vst3View};

/// The file extension of a VST3 module, bundle or bare library alike.
pub const VST3_EXTENSION: &str = "vst3";

/// The `.vst3` modules directly inside `dir`. See [`plugin_module::list_modules`].
pub fn list_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    plugin_module::list_modules(dir, VST3_EXTENSION)
}

/// The `.vst3` modules in `dir` and one level down. See
/// [`plugin_module::find_modules`].
pub fn find_modules(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    plugin_module::find_modules(dir, VST3_EXTENSION)
}
