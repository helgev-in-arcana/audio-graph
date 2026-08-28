//! Audio graph engine crate for evaluating node graphs.
//!
//! Modulation sources (constants, LFOs, note expressions) and audio routing
//! nodes are evaluated to drive parameters and process multi-channel audio
//! streams. The engine operates on numeric signals and buffer indices without
//! direct coupling to plugin formats or external host parameter mappings.
//!
//! Plugin nodes interact through [`AudioInstances`][subhost_adapter::AudioInstances],
//! passing instance IDs, note stream names, and audio slices.
//!
//! The crate architecture is organized across the UI/audio thread boundary:
//!
//! - [`Graph`]: Mutable, serializable graph representation for editing.
//! - [`compile`]: Compiles and validates a graph into a flattened execution [`Program`].
//! - [`Handoff`]: Lock-free message passing mechanism between threads.
//! - [`Engine`]: Audio-thread execution engine that runs compiled programs without allocations.

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
