//! Format-agnostic plugin hosting API.
//!
//! Provides a unified data model and trait definitions for hosting audio
//! plugins across different plugin formats (such as CLAP and VST3).
//!
//! Two rules shape everything here, and `README.md` in this crate spells out
//! why:
//!
//! * The model is shaped after CLAP, the richer format. VST3 backends
//!   *degrade* to it; it is never narrowed to the intersection of the two.
//! * Nothing that cannot cross a process boundary may appear in a public
//!   signature — no `ComPtr`, no raw pointers, no references or `Arc` in
//!   payloads, no single-shot getters.

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
/// Deliberately a flat, owned enum: it must be serializable across an IPC
/// boundary without dragging backend types along.
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
