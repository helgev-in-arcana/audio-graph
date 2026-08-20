//! The compiled form: a flat list of instructions over a register file.
//!
//! This is what crosses to the audio thread. It holds no `Rc`, no `Box<dyn>`,
//! no map lookups and no pointers back into the edit graph — running it is a
//! straight walk down a `Vec` writing `f64`s into a slice. Everything that
//! could have been a decision has already been made by the compiler, which is
//! the whole point of §9.1: the audio thread does not think, it executes.
//!
//! A `Program` is immutable once built. The per-instance state that changes as
//! it runs — LFO phases — lives in [`Engine`][crate::Engine] instead, so
//! swapping a program does not reset an oscillator mid-note.

use crate::graph::{ExprSource, MathOp, NodeId, Waveform};

/// Ceilings, so the audio thread can preallocate and never resize.
///
/// A graph that would exceed one is refused at compile time with an error the
/// user can read, which is a much better failure than an allocation inside
/// `process`.
pub const MAX_REGISTERS: usize = 256;
pub const MAX_LFOS: usize = 64;
pub const MAX_DELAY_LINES: usize = 16;

/// How far back a param delay line can read, in sub-blocks.
///
/// A param line stores one value per sub-block (§9.2), so this is a time only
/// once the sample rate and the quantum are known: 4096 sub-blocks is 2.7 s at
/// 48 kHz with the default quantum of 32, and 1.4 s at the finest quantum of
/// 16. The ring is preallocated for it, because §9.1 forbids allocating in
/// `process` and the alternative — sizing from the longest delay in the graph —
/// would mean a reallocation every time the user drags the time control.
pub const MAX_DELAY_TAPS: usize = 4096;

/// An index into the register file.
pub type Reg = u16;

/// A math operand: either another node's output or the node's own constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operand {
    Reg(Reg),
    Value(f64),
}

/// How fast an LFO runs, resolved into what the evaluator needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateSpec {
    Hz(f64),
    /// Cycles per beat is what the evaluator wants; the editor thinks in beats
    /// per cycle, so the reciprocal is taken once, here, rather than every
    /// sub-block.
    CyclesPerBeat(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Const {
        out: Reg,
        value: f64,
    },
    /// Read the DAW's automation for a slot.
    Slot {
        out: Reg,
        slot: u16,
    },
    Lfo {
        out: Reg,
        /// Index into the engine's phase array.
        state: u16,
        waveform: Waveform,
        rate: RateSpec,
        offset_phase: f64,
        depth: f64,
        centre: f64,
    },
    Expr {
        out: Reg,
        source: ExprSource,
    },
    Math {
        out: Reg,
        a: Reg,
        b: Operand,
        op: MathOp,
    },
    Range {
        out: Reg,
        a: Reg,
        in_lo: f64,
        in_span: f64,
        out_lo: f64,
        out_span: f64,
        clamp: bool,
    },
    /// Read from a delay line, `time` seconds back (§14.4).
    ///
    /// Clamped at run time to at least one sub-block, which is the floor of
    /// §14.4 expressed in the param domain. The compiler cannot do it: the
    /// floor depends on the sample rate and the quantum, and it knows neither.
    DelayRead {
        out: Reg,
        line: u16,
        time: f64,
    },
    /// Write this sub-block's value into a delay line.
    ///
    /// Emitted after everything feeding it, like any other op — but nothing
    /// reads its result, so no register is involved and no edge exists for the
    /// topological sort to follow back.
    DelayWrite {
        line: u16,
        a: Reg,
    },
}

/// A graph, compiled.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Topologically ordered: every `Op` reads only registers already written.
    pub ops: Vec<Op>,
    pub registers: usize,
    /// Which slot each output drives, and where its value ends up. Sorted by
    /// slot, and at most one entry per slot.
    pub outputs: Vec<(u16, Reg)>,
    /// Line index → the `DelayWrite` node it belongs to.
    ///
    /// Carried across a swap for the same reason as `lfo_nodes`: §14.5. A
    /// feedback loop that emptied itself every time the user nudged an
    /// unrelated control would not be usable.
    pub delay_nodes: Vec<NodeId>,
    /// State index → the LFO node it belongs to.
    ///
    /// Carried across a swap so that recompiling — which happens on every drag
    /// of every knob — does not restart the oscillators. Without it, editing
    /// an unrelated node would put a click in the middle of a slow LFO sweep.
    pub lfo_nodes: Vec<NodeId>,
}

impl Program {
    /// The program that does nothing: no graph, or a graph with no outputs.
    pub fn empty() -> Program {
        Program {
            ops: Vec::new(),
            registers: 0,
            outputs: Vec::new(),
            delay_nodes: Vec::new(),
            lfo_nodes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Whether the graph drives this slot, and so overrides the DAW's
    /// automation for it.
    pub fn drives(&self, slot: usize) -> bool {
        u16::try_from(slot).is_ok_and(|s| self.outputs.iter().any(|&(o, _)| o == s))
    }
}
