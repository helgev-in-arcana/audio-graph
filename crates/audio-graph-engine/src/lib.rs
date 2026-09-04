//! Audio graph engine crate for evaluating node graphs.
//!
//! Modulation sources (constants, LFOs, note expressions) and audio routing
//! nodes are evaluated to drive parameters and process multi-channel audio
//! streams. Nothing here knows what a VST3 is or what a slot is bound to: it
//! reads numbers and writes numbers, and the outer layers decide what those
//! numbers mean.
//!
//! See `README.md` in this crate for the invariants that hold across the
//! thread boundary.
//!
//! Plugin nodes interact through
//! [`AudioInstances`][subhost_adapter::AudioInstances], passing instance IDs,
//! note stream *names*, and flat audio slices. Which way that dependency points
//! matters: `subhost-adapter` is the general crate and this one is AudioGraph's,
//! so this one does the depending.
//!
//! The crate architecture is organized across the UI/audio thread boundary:
//!
//! - [`Graph`]: the edit side. Freely mutable, serialisable, allowed to be
//!   nonsense in the middle of an edit.
//! - [`compile`]: turns a graph into a [`Program`] — flat, ordered, checked.
//! - [`Handoff`]: carries the program down to the audio thread and the old one
//!   back up, without a lock in either direction.
//! - [`Engine`]: runs it, allocating nothing and freeing nothing.

mod compile;
mod engine;
mod graph;
mod handoff;
mod ir;
mod nodes;
mod notes;
mod port;

pub use compile::{CompileError, compile};
pub use engine::{AudioContext, BlockContext, Engine};
pub use graph::{Graph, LineId, Link, Node, NodeId};
pub use handoff::Handoff;
pub use ir::{
    AudioOp, Buf, Chunking, Detect, Follow, MAX_AUDIO_DELAY_LINES, MAX_AUDIO_DELAY_SECONDS,
    MAX_AUDIO_LANES, MAX_BUFFERS, MAX_CHANNELS, MAX_DELAY_LINES, MAX_DELAY_TAPS, MAX_GRAPH_PARAMS,
    MAX_LFOS, MAX_REGISTERS, MathOp, NoteOp, Op, Operand, Program, RateSpec, Reg, Waveform,
};
pub use nodes::{
    AudioIn, AudioOut, CcIn, Constant, DelayRead, DelayWrite, EnvelopeFollower, FilterMode, Gate,
    KeyParam, KeyParamMode, KeySplit, KeySwitch, KeySwitchMode, Lfo, Math, Mix, NodeKind,
    NoteFilter, NoteFollow, NoteGate, NoteIn, NoteMute, ParamPort, ParamToCc, Plugin, PluginPorts,
    RangeMap, Rate, SlotIn, Switch, db_to_linear, linear_to_db,
};
#[cfg(feature = "ui")]
pub use nodes::{
    NodeGroup, catalogue,
    widgets::{InstanceView, NODE_WIDTH, NodeAction, NodeUi},
};
pub use notes::{Ended, MAX_LIVE_NOTES};
pub use port::{Port, PortType};
