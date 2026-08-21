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

mod audio;
mod compile;
mod engine;
mod graph;
mod handoff;
mod program;

pub use audio::MAX_BUFFERS;
pub use compile::{CompileError, compile};
pub use engine::{AudioChunk, AudioContext, AudioNodes, BlockContext, Engine, NoNodes};
pub use graph::{
    ExprSource, Graph, LineId, Link, MathOp, Node, NodeId, NodeKind, ParamPort, PluginPorts, Port,
    PortType, Rate, Waveform,
};
pub use handoff::Handoff;
pub use program::{
    AudioOp, Buf, Chunking, InstanceIo, MAX_AUDIO_DELAY_LINES, MAX_AUDIO_DELAY_SECONDS,
    MAX_AUDIO_LANES, MAX_CHANNELS, MAX_DELAY_LINES, MAX_DELAY_TAPS, MAX_GRAPH_PARAMS, MAX_LFOS,
    MAX_REGISTERS, NoteSource, Op, Operand, ParamTarget, Program, RateSpec, Reg,
};
