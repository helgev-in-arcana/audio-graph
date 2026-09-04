//! Parameter instruction set and evaluation primitives.
//!
//! Contains the scalar operations the parameter engine executes each sub-block.
//!
//! The payload enums a node's settings reduce to — [`Waveform`], [`MathOp`],
//! [`Follow`] — live here rather than next to the node that offers them,
//! because an instruction carries them: they are part of what crosses to the
//! audio thread, and `ir` is what may not depend on the edit side. The node
//! modules use them from here.

use serde::{Deserialize, Serialize};

/// An index into the register file.
pub type Reg = u16;

/// What "how loud" means to an [`Op::Follow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Detect {
    /// The largest sample in the window. What catches a transient, and what a
    /// peak meter or a limiter's sidechain reads.
    #[default]
    Peak,
    /// The root mean square of the window. Closer to how loud something
    /// sounds, and steadier, which is what a compressor usually wants.
    Rms,
}

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
    /// One of two operands, chosen by where `control` sits against `threshold`:
    /// `high` at the threshold and above, `low` below it.
    ///
    /// A `>=` rather than a `>` so that a control reaching exactly 1.0 — which a
    /// gate driven by an [`Op::NoteFollow`] reading [`Follow::Gate`] does —
    /// switches.
    Select {
        out: Reg,
        control: Reg,
        threshold: f64,
        low: Operand,
        high: Operand,
    },
    /// 1 while `key` is held, 0 otherwise.
    ///
    /// The one thing the graph could not otherwise ask about a note stream: an
    /// [`Op::NoteFollow`] answers for the newest note whatever it was, and a
    /// key switch is a question about one particular key regardless of what has
    /// been played since.
    KeyHeld {
        out: Reg,
        /// The note buffer to watch. A key switch answers to the stream wired
        /// into it, not to whatever the DAW happened to send: a switch behind
        /// a channel filter must not fire on another channel's keys.
        buf: u16,
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
        buf: u16,
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
        buf: u16,
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
    /// How loud audio buffer `buf` is over this sub-block.
    ///
    /// The one op that reads the audio pool, and the reason a program is cut
    /// into stages at all: how loud a signal is cannot be known before the
    /// signal is, so this runs in a stage after the one that made it. Every
    /// audio op of that stage has covered the whole block by then, so the
    /// window read here is this sub-block's own — no lookahead and nothing
    /// held back, which a sidechain wants and a limiter would want more of.
    ///
    /// The floor on the attack and release times is one sub-block: the value
    /// only moves at a boundary, because that is what a parameter is.
    ///
    /// `state` is a latch, so the envelope carries on across blocks and
    /// survives the recompile that happens on every drag of every control.
    Follow {
        out: Reg,
        buf: u16,
        state: u16,
        detect: Detect,
        /// Seconds to rise, and to fall. Zero for either means follow it
        /// exactly, which is what a meter wants and a compressor does not.
        attack: f64,
        release: f64,
    },
    /// Follow the notes on buffer `buf`: how hard, whether any are down, or
    /// which key.
    ///
    /// Monophonic on purpose. These three are the ones that still mean
    /// something when polyphony is flattened — how hard the player is playing,
    /// whether they are playing at all, and where on the keyboard. A per-note
    /// reading that loses its meaning in that flattening does not belong
    /// here.
    ///
    /// `state` is a latch: velocity and key hold their last value between
    /// notes, and the gate holds a count of what is down.
    NoteFollow {
        out: Reg,
        buf: u16,
        state: u16,
        what: Follow,
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

/// What a [`Op::NoteFollow`] reads off a note stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Follow {
    /// Velocity of the most recent note-on, `0..=1`.
    #[default]
    Velocity,
    /// 1 while any note from this stream is held, 0 otherwise.
    Gate,
    /// Key number of the most recent note-on, `0..=1` across the MIDI range.
    KeyTrack,
    /// How many keys are down, `0..=128`, counted off the mask of held keys
    /// rather than off the running note count beside it.
    ///
    /// The mask is the exact answer under a gate: a gate holds note-ons back
    /// and lets note-offs through, so a stream can carry the release of a note
    /// whose arrival it never saw. Clearing a bit that is already clear costs
    /// nothing, where a decrement would take the count somewhere the keyboard
    /// never was.
    ///
    /// A count, not a fraction, and the only reading here that is. Two keys
    /// are two, and what that is worth is the business of a `Param Map` or of
    /// the thresholds on a `Param Select` — both of which read the number as
    /// it stands.
    HeldKeys,
}

impl Follow {
    pub const ALL: [Follow; 4] = [
        Follow::Velocity,
        Follow::Gate,
        Follow::KeyTrack,
        Follow::HeldKeys,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Follow::Velocity => "Velocity",
            Follow::Gate => "Gate",
            Follow::KeyTrack => "Key Track",
            Follow::HeldKeys => "Held Keys",
        }
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
