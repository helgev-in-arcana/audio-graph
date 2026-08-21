//! Event model (ARCHITECTURE.md §3.2).
//!
//! `SetValue` and `Modulate` are separate variants on purpose: CLAP keeps
//! `PARAM_VALUE` and `PARAM_MOD` apart so modulation is non-destructive, and
//! collapsing them here would delete that capability from every backend.
//! VST3 flattens them back together in its own layer (§3.4).

use crate::params::ParamId;

/// Who a parameter event applies to.
///
/// VST3 can only express `Global`; the VST3 backend folds or drops the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    #[default]
    Global,
    NoteId(i32),
    Key {
        channel: i16,
        key: i16,
    },
    Channel(i16),
    Port(i16),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamEvent {
    /// Set the parameter to `value` (plain units). Drive mode uses only this.
    SetValue {
        id: ParamId,
        target: Target,
        value: f64,
        sample_offset: u32,
    },
    /// Add `amount` (plain units) on top of the parameter's own value.
    Modulate {
        id: ParamId,
        target: Target,
        amount: f64,
        sample_offset: u32,
    },
    GestureBegin {
        id: ParamId,
    },
    GestureEnd {
        id: ParamId,
    },
}

impl ParamEvent {
    pub fn id(&self) -> ParamId {
        match *self {
            ParamEvent::SetValue { id, .. }
            | ParamEvent::Modulate { id, .. }
            | ParamEvent::GestureBegin { id }
            | ParamEvent::GestureEnd { id } => id,
        }
    }

    pub fn sample_offset(&self) -> u32 {
        match *self {
            ParamEvent::SetValue { sample_offset, .. }
            | ParamEvent::Modulate { sample_offset, .. } => sample_offset,
            _ => 0,
        }
    }

    /// The same event, timed at `offset` instead.
    ///
    /// A gesture has no offset to move; it is returned unchanged rather than
    /// refused, so a caller rebasing a whole stream does not have to know which
    /// events carry a time.
    pub fn at_offset(mut self, offset: u32) -> ParamEvent {
        if let ParamEvent::SetValue { sample_offset, .. }
        | ParamEvent::Modulate { sample_offset, .. } = &mut self
        {
            *sample_offset = offset;
        }
        self
    }
}

/// Per-note continuous controllers. Both formats have these; VST3 declares the
/// available types at runtime via `INoteExpressionController`, CLAP uses a
/// fixed enum, so backends keep a mapping table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteExpression {
    Volume,
    Pan,
    /// Signed semitone offset from the note's nominal pitch.
    Tuning,
    Vibrato,
    Expression,
    Brightness,
    Pressure,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteEvent {
    NoteOn {
        note_id: i32,
        port: i16,
        channel: i16,
        key: i16,
        velocity: f64,
        sample_offset: u32,
    },
    NoteOff {
        note_id: i32,
        port: i16,
        channel: i16,
        key: i16,
        velocity: f64,
        sample_offset: u32,
    },
    /// The plugin tells the host a voice ended (CLAP `NOTE_END`). Forwarded so
    /// per-voice graph state can be released.
    NoteEnd {
        note_id: i32,
        port: i16,
        channel: i16,
        key: i16,
        sample_offset: u32,
    },
    Expression {
        note_id: i32,
        port: i16,
        channel: i16,
        key: i16,
        expression: NoteExpression,
        value: f64,
        sample_offset: u32,
    },
    /// Raw 3-byte MIDI for anything without a first-class representation.
    Midi {
        port: i16,
        data: [u8; 3],
        sample_offset: u32,
    },
}

impl NoteEvent {
    pub fn sample_offset(&self) -> u32 {
        match *self {
            NoteEvent::NoteOn { sample_offset, .. }
            | NoteEvent::NoteOff { sample_offset, .. }
            | NoteEvent::NoteEnd { sample_offset, .. }
            | NoteEvent::Expression { sample_offset, .. }
            | NoteEvent::Midi { sample_offset, .. } => sample_offset,
        }
    }

    /// The same event, timed at `offset` instead.
    pub fn at_offset(mut self, offset: u32) -> NoteEvent {
        let (NoteEvent::NoteOn { sample_offset, .. }
        | NoteEvent::NoteOff { sample_offset, .. }
        | NoteEvent::NoteEnd { sample_offset, .. }
        | NoteEvent::Expression { sample_offset, .. }
        | NoteEvent::Midi { sample_offset, .. }) = &mut self;
        *sample_offset = offset;
        self
    }
}

/// Everything that can be handed to `process`, in one ordered stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Param(ParamEvent),
    Note(NoteEvent),
}

impl Event {
    pub fn sample_offset(&self) -> u32 {
        match self {
            Event::Param(e) => e.sample_offset(),
            Event::Note(e) => e.sample_offset(),
        }
    }

    /// The same event, timed at `offset` instead.
    ///
    /// Used when a block is cut into chunks and each chunk is handed to the
    /// sub-plugin as its own `process` call (§14.9): an event at sample 40 of
    /// the block is at sample 8 of the chunk that starts at 32, and handing it
    /// over still saying 40 would put it past the end of a 32-sample buffer.
    pub fn at_offset(self, offset: u32) -> Event {
        match self {
            Event::Param(e) => Event::Param(e.at_offset(offset)),
            Event::Note(e) => Event::Note(e.at_offset(offset)),
        }
    }
}

/// Collects events emitted *by* the sub-plugin during `process`.
///
/// A plain owned buffer rather than a callback: a callback would be a reference
/// crossing the boundary, which §4.1 forbids.
#[derive(Debug, Clone, Default)]
pub struct EventSink {
    events: Vec<Event>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate so the audio thread never grows the buffer (§9.1).
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            events: Vec::with_capacity(cap),
        }
    }

    pub fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Transport / tempo state for one process block.
///
/// `subhost-adapter` forwards the DAW's context to the sub-plugin, adjusting
/// for any latency the wrapper itself introduces (§7.1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeContext {
    pub tempo_bpm: f64,
    pub time_sig_numerator: i32,
    pub time_sig_denominator: i32,
    /// Position in samples since the start of the project.
    pub project_time_samples: i64,
    /// Position in quarter notes since the start of the project.
    pub project_time_music: f64,
    pub bar_position_music: f64,
    pub playing: bool,
    pub recording: bool,
    pub loop_active: bool,
}

impl Default for TimeContext {
    fn default() -> Self {
        Self {
            tempo_bpm: 120.0,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
            project_time_samples: 0,
            project_time_music: 0.0,
            bar_position_music: 0.0,
            playing: false,
            recording: false,
            loop_active: false,
        }
    }
}
