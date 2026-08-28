//! Format-agnostic plugin hosting API.
//!
//! Provides a unified data model and trait definitions for hosting audio plugins
//! across different plugin formats (such as CLAP and VST3). Public types are
//! designed to be self-contained and suitable for process boundary isolation.

mod buffers;
mod events;
mod params;
mod traits;

pub use buffers::{AudioBuffers, AudioConfig, AuxBuses, BufferLayout, MAX_AUX_BUSES};
pub use events::{Event, EventSink, NoteEvent, NoteExpression, ParamEvent, Target, TimeContext};
pub use params::{
    BusInfo, Capabilities, IoLayout, ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue,
    VoiceInfo,
};
pub use traits::{HostContext, ProcessStatus, RestartReason, SubPluginMain, SubPluginProcessor};

/// Errors surfaced across the host API boundary.
///
/// Uses an owned, self-contained representation suitable for IPC boundaries.
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
