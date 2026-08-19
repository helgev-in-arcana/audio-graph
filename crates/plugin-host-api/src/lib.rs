//! Format-agnostic plugin hosting API.
//!
//! ARCHITECTURE.md §3 / ADR-4: the data model here is deliberately shaped after
//! CLAP (the richer format). VST3 backends *degrade* to it; the model is never
//! narrowed to the intersection of the two formats.
//!
//! ARCHITECTURE.md §4: nothing that cannot cross a process boundary may appear
//! in a public signature here — no `ComPtr`, no raw pointers, no references or
//! `Arc` in payloads, no single-shot getters.

mod buffers;
mod events;
mod params;
mod traits;

pub use buffers::{AudioBuffers, AudioConfig, BufferLayout};
pub use events::{Event, EventSink, NoteEvent, NoteExpression, ParamEvent, Target, TimeContext};
pub use params::{Capabilities, ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue};
pub use traits::{HostContext, ProcessStatus, RestartReason, SubPluginMain, SubPluginProcessor};

/// Errors surfaced across the host API boundary.
///
/// Deliberately a flat, owned enum: it must be serializable across an IPC
/// boundary (ADR-6) without dragging backend types along.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The module (bundle / DLL / .so) could not be loaded.
    ModuleLoad(String),
    /// The module loaded but did not expose a usable plugin factory.
    NoFactory(String),
    /// No class with the requested identity exists in the module.
    ClassNotFound(String),
    /// The backend refused the call. `code` is the raw format-specific result.
    Backend { context: String, code: i32 },
    /// The requested bus/channel configuration is not supported by the plugin.
    UnsupportedBusConfig(String),
    /// State blob could not be read or written.
    State(String),
    /// A call was made in the wrong lifecycle phase.
    InvalidState(&'static str),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::ModuleLoad(s) => write!(f, "module load failed: {s}"),
            HostError::NoFactory(s) => write!(f, "no plugin factory: {s}"),
            HostError::ClassNotFound(s) => write!(f, "class not found: {s}"),
            HostError::Backend { context, code } => {
                write!(f, "{context} failed (code {code:#010x})")
            }
            HostError::UnsupportedBusConfig(s) => write!(f, "unsupported bus config: {s}"),
            HostError::State(s) => write!(f, "state error: {s}"),
            HostError::InvalidState(s) => write!(f, "invalid state: {s}"),
        }
    }
}

impl std::error::Error for HostError {}

pub type Result<T> = std::result::Result<T, HostError>;
