//! Parameter instruction set and evaluation primitives.
//!
//! Contains the payload definitions ([`Waveform`], [`MathOp`], [`ExprSource`])
//! and scalar operations executed by the parameter engine during each sub-block.

use serde::{Deserialize, Serialize};

/// An index into the register file.
pub type Reg = u16;

/// A math operand: either another node's output register or an immediate constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operand {
    Reg(Reg),
    Value(f64),
}

/// LFO rate specification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateSpec {
    Hz(f64),
    /// Rate expressed in cycles per beat for tempo-synchronized evaluation.
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
    /// Select between two operands based on whether `control` meets or exceeds `threshold`:
    /// outputs `high` when `control >= threshold`, otherwise `low`.
    Select {
        out: Reg,
        control: Reg,
        threshold: f64,
        low: Operand,
        high: Operand,
    },
    /// Outputs 1.0 while `key` is held, 0.0 otherwise.
    KeyHeld {
        out: Reg,
        key: u8,
    },
    /// Advances latch `state` to the next of `count` positions when `key` is struck, wrapping around.
    KeyStep {
        state: u16,
        key: u8,
        count: u16,
    },
    /// Sets latch `state` to `value` when `key` is struck.
    KeyLatch {
        state: u16,
        key: u8,
        value: f64,
    },
    /// Read latch `state`, or `initial` if unset.
    Latch {
        out: Reg,
        state: u16,
        initial: f64,
    },
    /// Outputs 1.0 when latch `state` matches `value`, 0.0 otherwise.
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
    /// The delay duration is clamped to at least one sub-block at runtime.
    DelayRead {
        out: Reg,
        line: u16,
        time: f64,
        time_reg: Option<Reg>,
    },
    /// Write the current sub-block's value into a parameter delay line.
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

/// Expression and per-note controller sources tracked by the engine.
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

    /// Whether this source represents continuous per-note controller expression
    /// (pressure, tuning, brightness, expression, vibrato, volume, pan) rather than basic note metadata.
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
    /// Power curve `a^b` for values in 0..1.
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
