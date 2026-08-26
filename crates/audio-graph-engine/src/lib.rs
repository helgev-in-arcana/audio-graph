//! The node graph: what turns a wrapper into an instrument of its own.
//!
//! ARCHITECTURE.md §9. Constants, LFOs and note expressions are combined into
//! values that drive the wrapper's slots, and the slots drive the sub-plugin's
//! parameters. Nothing here knows what a VST3 is, what a slot is bound to, or
//! that there is a sub-plugin at all — it reads numbers and writes numbers, and
//! the outer layers decide what those numbers mean.
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
pub use engine::{AudioChunk, AudioContext, AudioNodes, BlockContext, Engine, NoNodes};
pub use graph::{Graph, LineId, Link, Node, NodeId};
pub use handoff::Handoff;
pub use ir::{
    AudioOp, Buf, Chunking, ExprSource, InstanceIo, MAX_AUDIO_DELAY_LINES, MAX_AUDIO_DELAY_SECONDS,
    MAX_AUDIO_LANES, MAX_BUFFERS, MAX_CHANNELS, MAX_DELAY_LINES, MAX_DELAY_TAPS, MAX_GRAPH_PARAMS,
    MAX_LFOS, MAX_REGISTERS, MathOp, NoteSource, Op, Operand, ParamTarget, Program, RateSpec, Reg,
    Waveform,
};
pub use nodes::{
    AudioIn, AudioOut, Constant, DelayRead, DelayWrite, Expression, Gate, Lfo, Math, Mix, NodeKind,
    NoteIn, ParamPort, Plugin, PluginPorts, RangeMap, Rate, SlotIn, Switch, db_to_linear,
    linear_to_db,
};
#[cfg(feature = "ui")]
pub use nodes::{
    catalogue,
    widgets::{InstanceView, NODE_WIDTH, NodeAction, NodeUi},
};
pub use port::{Port, PortType};
