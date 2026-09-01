//! Parameter instruction set and evaluation primitives.
//!
//! Contains the scalar operations the parameter engine executes each sub-block.
//!
//! The payload enums a node's settings reduce to — [`Waveform`], [`MathOp`],
//! [`ExprSource`] — live here rather than next to the node that offers them,
//! because an instruction carries them: they are part of what crosses to the
//! audio thread, and `ir` is what may not depend on the edit side. The node
//! modules use them from here.

use serde::{Deserialize, Serialize};

/// An index into the register file.
pub type Reg = u16;

/// A math operand: either another node's output register or an immediate constant.
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
    /// Read the host automation value for a slot.
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
    /// One of two operands, chosen by where `control` sits against `threshold`:
    /// `high` at the threshold and above, `low` below it.
    ///
    /// A `>=` rather than a `>` so that a control reaching exactly 1.0 — which a
    /// gate wired from `Expression`'s `Gate` does — switches.
    Select {
        out: Reg,
        control: Reg,
        threshold: f64,
        low: Operand,
        high: Operand,
    },
    /// 1 while `key` is held, 0 otherwise.
    ///
    /// The one thing the graph could not otherwise ask about a note stream:
    /// `Expression` answers for the newest note whatever it was, and a key
    /// switch is a question about one particular key regardless of what has been
    /// played since.
    KeyHeld {
        out: Reg,
        key: u8,
    },
    /// Advances latch `state` to the next of `count` positions when `key` is
    /// struck, wrapping around. One key cycling a switch, which with `count` of
    /// 2 is a plain toggle.
    ///
    /// The value is read back by an [`Op::Latch`] or an [`Op::LatchIs`], because
    /// the latch is what survives a program swap and a register does not.
    /// Like `DelayWrite`, this deliberately does not write to a register to omit an edge in
    /// the topological sort, preventing cycles.
    KeyStep {
        state: u16,
        key: u8,
        count: u16,
    },
    /// Sets latch `state` to `value` when `key` is struck.
    ///
    /// Several of these on one latch is a bank of key switches: the last key
    /// pressed wins, which is what a bank of switches does.
    /// Like `DelayWrite`, this deliberately does not write to a register to omit an edge in
    /// the topological sort, preventing cycles.
    KeyLatch {
        state: u16,
        key: u8,
        value: f64,
    },
    /// Read the latest value of a controller off note buffer `buf`.
    ///
    /// The one op that reads the note half, and the reason the note pass runs
    /// before this one: it names a buffer, so the buffer had to exist already.
    ///
    /// It sees the *previous* sub-block's stream, because the note half fills
    /// the buffers at the end of each sub-block's parameter evaluation. That is
    /// the honest answer rather than a shortcut: a parameter signal has
    /// sub-block resolution, so the value a reader wants is the one in effect
    /// at the boundary — the last event before it, not one from the middle of
    /// the sub-block it is about to start. Events reaching a sub-plugin keep
    /// their own sample offsets and are not delayed by this.
    ///
    /// `channel` of -1 means any. `state` is a latch holding the last value
    /// seen, because a controller keeps its position between messages and a
    /// block with no CC in it must not snap the value back to zero.
    NoteCc {
        out: Reg,
        buf: u16,
        state: u16,
        channel: i16,
        cc: u8,
        /// What the controller reads as before it has ever been moved.
        initial: f64,
    },
    /// Read latch `state`, or `initial` if unset.
    Latch {
        out: Reg,
        state: u16,
        initial: f64,
    },
    /// 1 when latch `state` holds `value`, 0 otherwise.
    ///
    /// What makes a bank of switches exclusive: each position asks whether it is
    /// the one selected, and exactly one of them can be. `initial` is what an
    /// unset latch counts as, so an untouched bank still has a position.
    LatchIs {
        out: Reg,
        state: u16,
        value: f64,
        initial: f64,
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
    /// Read from a parameter delay line, `time` seconds back.
    ///
    /// When `time_reg` is present, its value overrides `time`.
    /// The delay duration is clamped at run time to at least one sub-block: a
    /// read that could see the current sub-block's own write would close a loop
    /// with no delay in it. The compiler cannot do the clamping — the floor
    /// depends on the sample rate and the quantum, and it knows neither.
    DelayRead {
        out: Reg,
        line: u16,
        time: f64,
        time_reg: Option<Reg>,
    },
    /// Write the current sub-block's value into a parameter delay line.
    /// Deliberately does not write to a register to omit an edge in the topological
    /// sort, thereby preventing cycles.
    DelayWrite {
        line: u16,
        a: Reg,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
    /// Sample and hold: a new random value at each cycle boundary.
    Random,
}

impl Waveform {
    pub const ALL: [Waveform; 5] = [
        Waveform::Sine,
        Waveform::Triangle,
        Waveform::Saw,
        Waveform::Square,
        Waveform::Random,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Waveform::Sine => "Sine",
            Waveform::Triangle => "Triangle",
            Waveform::Saw => "Saw",
            Waveform::Square => "Square",
            Waveform::Random => "Random",
        }
    }
}

/// Which per-note controller a node reads.
///
/// The graph is monophonic, so polyphony is reduced away: each source keeps the
/// most recent value from any note, because a per-voice value would have nowhere
/// to go. `Capabilities.poly_modulation` is what will decide whether the *voice*
/// level ever becomes reachable, and the editor already greys these out when the
/// sub-plugin cannot accept per-note modulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExprSource {
    Pressure,
    Tuning,
    Brightness,
    Expression,
    Vibrato,
    Volume,
    Pan,
    /// Velocity of the most recent note-on event, scaled to 0..1.
    Velocity,
    /// 1.0 while any note is currently held, 0.0 otherwise.
    Gate,
    /// Key number of the most recent note, normalized to 0..1 across the MIDI range.
    KeyTrack,
}

impl ExprSource {
    pub const ALL: [ExprSource; 10] = [
        ExprSource::Pressure,
        ExprSource::Tuning,
        ExprSource::Brightness,
        ExprSource::Expression,
        ExprSource::Vibrato,
        ExprSource::Volume,
        ExprSource::Pan,
        ExprSource::Velocity,
        ExprSource::Gate,
        ExprSource::KeyTrack,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExprSource::Pressure => "Pressure",
            ExprSource::Tuning => "Tuning",
            ExprSource::Brightness => "Brightness",
            ExprSource::Expression => "Expression",
            ExprSource::Vibrato => "Vibrato",
            ExprSource::Volume => "Volume",
            ExprSource::Pan => "Pan",
            ExprSource::Velocity => "Velocity",
            ExprSource::Gate => "Gate",
            ExprSource::KeyTrack => "Key track",
        }
    }

    /// Whether this source comes from a per-note controller rather than from the
    /// note itself. These are the ones a sub-plugin without per-note modulation
    /// cannot meaningfully receive.
    pub fn is_per_note(self) -> bool {
        !matches!(
            self,
            ExprSource::Velocity | ExprSource::Gate | ExprSource::KeyTrack
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MathOp {
    Add,
    Subtract,
    Multiply,
    Min,
    Max,
    /// `a^b` on a 0..1 input — the curve control.
    Curve,
}

impl MathOp {
    pub const ALL: [MathOp; 6] = [
        MathOp::Add,
        MathOp::Subtract,
        MathOp::Multiply,
        MathOp::Min,
        MathOp::Max,
        MathOp::Curve,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MathOp::Add => "Add",
            MathOp::Subtract => "Subtract",
            MathOp::Multiply => "Multiply",
            MathOp::Min => "Min",
            MathOp::Max => "Max",
            MathOp::Curve => "Curve",
        }
    }
}
