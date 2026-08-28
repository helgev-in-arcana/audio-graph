//! Unified plugin hosting facade supporting VST3 and CLAP formats.
//!
//! Provides a single interface for scanning, loading, controlling, and rendering
//! audio plugins across backends:
//!
//! ```text
//! plugin-host             <- unified facade: Format, ClassInfo, Plugin
//!   ├── vst3-host / vst3-host-view
//!   ├── clap-host
//!   └── plugin-host-api    <- shared traits and data model
//! ```
//!
//! This crate handles format-specific differences (directory discovery, module
//! enumeration, instantiation, editor window management) while re-exporting the
//! common types from [`plugin_host_api`].

pub mod catalogue;
pub mod config;
mod format;
mod main_thread;
mod plugin;
mod scan;

pub use format::{FORMATS, Format};
pub use main_thread::MainThread;
pub use plugin::Plugin;
pub use scan::{
    ClassInfo, PluginRef, default_plugin_directories, find_modules, installed_modules,
    plugin_directories, resolve_reference, scan_module, scan_module_as,
};

// Re-export the shared data model so callers have a single dependency.
pub use plugin_host_api::{
    AudioBuffers, AudioConfig, AuxBuses, BufferLayout, BusInfo, Capabilities, Event, EventSink,
    HostContext, HostError, IoLayout, MAX_AUX_BUSES, NoteEvent, NoteExpression, ParamEvent,
    ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue, ProcessStatus, RestartReason,
    Result, SubPluginMain, SubPluginProcessor, Target, TimeContext, VoiceInfo,
};

// Window plumbing for managing and embedding plugin editor windows.
pub use host_window::{
    ContainerWindow, Deferred, Size, deferred, forward_key, pump_events, root_window,
};

/// Prepares the calling thread for hosting plugins.
///
/// On Windows, initializes COM apartment state required by VST3 plugins.
/// Idempotent; should be called on every thread that will load or interact with plugins.
pub fn init_thread() {
    vst3_host::init_apartment();
}
