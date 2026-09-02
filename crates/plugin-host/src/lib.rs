//! Unified plugin hosting facade supporting VST3 and CLAP formats.
//!
//! One facade over both backends. A caller here says "load this path", "give
//! me its parameters", "open its editor" and never learns which format
//! answered — which is the point: `subhost-adapter`, the node graph and the
//! CLI are then written once, and a third format is a new arm in this crate
//! rather than a new branch in every one of them.
//!
//! ```text
//! plugin-host             <- unified facade: Format, ClassInfo, Plugin
//!   ├── vst3-host / vst3-host-view
//!   ├── clap-host
//!   └── plugin-host-api    <- shared traits and data model
//! ```
//!
//! The data model itself is *not* re-invented here: [`plugin_host_api`]
//! already owns it and is dependency-free, which is what keeps an
//! out-of-process backend a substitution rather than a rewrite. This crate
//! re-exports it so a caller needs one dependency instead of two, and adds
//! nothing to it.
//!
//! ## What belongs here, and what does not
//!
//! Here: anything whose answer differs by format. Where plugins live, how a
//! module is enumerated, how an instance is created, how an editor is attached.
//!
//! Not here: anything specific to hosting a plugin *inside another plugin* —
//! forwarding the DAW's transport, combining latency, nesting state. That is
//! `subhost-adapter`'s job, and the test is: would an offline renderer or a
//! plugin scanner still need it?
//!
//! [`MainThread`] is here by that test rather than in spite of it. The rule it
//! encodes — VST3 pins a controller call to the thread that created the
//! instance — is a format's rule, not a nesting one.

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

// The shared data model, re-exported wholesale. Callers depend on this crate
// and get the vocabulary with it.
pub use plugin_host_api::{
    AudioBuffers, AudioConfig, AuxBuses, BufferLayout, BusInfo, Capabilities, Event, EventSink,
    HostContext, HostError, IoLayout, MAX_AUX_BUSES, NoteEvent, NoteExpression, NoteId, ParamEvent,
    ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue, ProcessStatus, RestartReason,
    Result, SubPluginMain, SubPluginProcessor, Target, TimeContext, VoiceInfo,
};

// Window plumbing a host application needs and that no backend owns.
pub use host_window::{ContainerWindow, Key, Size, forward_key, poll, pump_events, root_window};

/// Prepares the calling thread for hosting plugins.
///
/// Today this is COM's apartment initialisation, which VST3 plugins on Windows
/// assume has happened and which crashes them when it has not. CLAP needs
/// nothing, and neither format needs anything on other platforms — but a
/// caller should not have to know that, so there is one call and it is always
/// correct.
///
/// Idempotent; call it on every thread that will load a plugin.
pub fn init_thread() {
    vst3_host::init_apartment();
}
