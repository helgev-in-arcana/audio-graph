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
mod program;

pub use compile::{CompileError, compile};
pub use engine::{BlockContext, Engine};
pub use graph::{ExprSource, Graph, Link, MathOp, Node, NodeId, NodeKind, Rate, Waveform};
pub use handoff::Handoff;
pub use program::{MAX_LFOS, MAX_REGISTERS, Op, Operand, Program, RateSpec, Reg};
