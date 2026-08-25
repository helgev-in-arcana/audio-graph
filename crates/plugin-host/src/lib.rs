//! The plugin host, as everything above it should see it.
//!
//! One facade over both backends. A caller here says "load this path", "give me
//! its parameters", "open its editor" and never learns which format answered —
//! which is the point: `subhost-adapter`, the node graph and the CLI are then
//! written once, and a third format is a new arm in this crate rather than a
//! new branch in every one of them.
//!
//! ```text
//! plugin-host          <- this crate: Format, ClassInfo, Plugin
//!   ├── vst3-host / vst3-host-view
//!   ├── clap-host
//!   └── plugin-host-api   <- the traits and the data model both backends share
//! ```
//!
//! The data model itself is *not* re-invented here: [`plugin_host_api`] already
//! owns it, is dependency-free, and is what ADR-6 relies on to keep an
//! out-of-process backend a substitution. This crate re-exports it so a caller
//! needs one dependency instead of two, and adds nothing to it.
//!
//! ## What belongs here, and what does not
//!
//! Here: anything whose answer differs by format. Where plugins live, how a
//! module is enumerated, how an instance is created, how an editor is attached.
//!
//! Not here: anything specific to hosting a plugin *inside another plugin* —
//! forwarding the DAW's transport, combining latency, nesting state. That is
//! `subhost-adapter`'s job (ARCHITECTURE.md §7), and the test is unchanged:
//! would an offline renderer or a plugin scanner still need it?

pub mod config;
mod format;
mod plugin;
mod scan;

pub use format::{FORMATS, Format};
pub use plugin::Plugin;
pub use scan::{
    ClassInfo, PluginRef, default_plugin_directories, find_modules, installed_modules,
    plugin_directories, resolve_reference, scan_module, scan_module_as,
};

// The shared data model, re-exported wholesale. Callers depend on this crate
// and get the vocabulary with it.
pub use plugin_host_api::{
    AudioBuffers, AudioConfig, AuxBuses, BufferLayout, BusInfo, Capabilities, Event, EventSink,
    HostContext, HostError, IoLayout, MAX_AUX_BUSES, NoteEvent, NoteExpression, ParamEvent,
    ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue, ProcessStatus, RestartReason,
    Result, SubPluginMain, SubPluginProcessor, Target, TimeContext, VoiceInfo,
};

// Window plumbing a host application needs and that no backend owns.
//
// `Deferred` is here because the rule it exists for belongs to the host, not to
// a format: a GUI toolkit's draw callback may only *record* that the user asked
// for a window, never open one.
pub use host_window::{
    ContainerWindow, Deferred, Size, deferred, forward_key, pump_events, root_window,
};

/// Prepare the calling thread for hosting plugins.
///
/// Today this is COM's apartment initialisation, which VST3 plugins on Windows
/// assume has happened and which crashes them when it has not (§13). CLAP needs
/// nothing, and neither format needs anything on other platforms — but a caller
/// should not have to know that, so there is one call and it is always correct.
///
/// Idempotent; call it on every thread that will load a plugin.
pub fn init_thread() {
    vst3_host::init_apartment();
}
