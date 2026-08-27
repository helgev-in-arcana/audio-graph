//! The node graph: what turns a wrapper into an instrument of its own.
//!
//! ARCHITECTURE.md §4. Constants, LFOs and note expressions are combined into
//! values that drive the wrapper's slots, and the slots drive the sub-plugin's
//! parameters. Nothing here knows what a VST3 is or what a slot is bound to —
//! it reads numbers and writes numbers, and the outer layers decide what those
//! numbers mean.
//!
//! It does know that a plugin node has something behind it, but only through
//! `subhost-adapter`'s [`AudioInstances`][subhost_adapter::AudioInstances]: an instance
//! number, a note stream's *name*, and two flat slices. Which way that
//! dependency points matters — `subhost-adapter` is the general crate and this
//! one is AudioGraph's, so this one does the depending.
//!
//! The crate is split along the one line that matters, the thread boundary:
//!
//! - [`Graph`] is the edit side. Freely mutable, serialisable, allowed to be
//!   nonsense in the middle of an edit.
//! - [`compile`] turns a graph into a [`Program`] — flat, ordered, checked.
//! - [`Handoff`] carries the program down to the audio thread and the old one
//!   back up, without a lock in either direction.
//! - [`Engine`] runs it, allocating nothing and freeing nothing.

mod compile;
mod engine;
mod graph;
mod handoff;
mod ir;
mod nodes;
mod port;

pub use compile::{CompileError, compile};
pub use engine::{AudioContext, BlockContext, Engine};
pub use graph::{Graph, LineId, Link, Node, NodeId};
pub use handoff::Handoff;
pub use ir::{
    AudioOp, Buf, Chunking, ExprSource, MAX_AUDIO_DELAY_LINES, MAX_AUDIO_DELAY_SECONDS,
    MAX_AUDIO_LANES, MAX_BUFFERS, MAX_CHANNELS, MAX_DELAY_LINES, MAX_DELAY_TAPS, MAX_GRAPH_PARAMS,
    MAX_LFOS, MAX_REGISTERS, MathOp, NoteRoute, Op, Operand, Program, RateSpec, Reg, Waveform,
};
pub use nodes::{
    AudioIn, AudioOut, Constant, DelayRead, DelayWrite, Expression, Gate, KeyParam, KeyParamMode,
    KeySwitch, KeySwitchMode, Lfo, Math, Mix, NodeKind, NoteGate, NoteIn, ParamPort, Plugin,
    PluginPorts, RangeMap, Rate, SlotIn, Switch, db_to_linear, linear_to_db,
};
#[cfg(feature = "ui")]
pub use nodes::{
    NodeGroup, catalogue,
    widgets::{InstanceView, NODE_WIDTH, NodeAction, NodeUi},
};
pub use port::{Port, PortType};
