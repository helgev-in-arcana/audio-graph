//! Event model for parameter changes, MIDI, and note expressions.
//!
//! `SetValue` and `Modulate` are separate variants on purpose: CLAP keeps
//! `PARAM_VALUE` and `PARAM_MOD` apart so modulation is non-destructive, and
//! collapsing them here would delete that capability from every backend. The
//! VST3 backend flattens them back together in its own layer, where the loss
//! belongs.

use crate::params::ParamId;

/// Target scope of a parameter event.
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

    /// Returns a copy of the event with its sample offset updated to `offset`.
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

/// Per-note continuous controllers.
///
/// Both formats have these, but VST3 declares the available types at runtime
/// via `INoteExpressionController` while CLAP uses a fixed enum, so backends
/// keep a mapping table.
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

/// A note's identity, as the host handed it to us.
///
/// `None` is the normal case, not an error: CLAP spells out that a host may
/// send `NOTE_OFF` with `note_id == -1` for a note whose `NOTE_ON` carried
/// one, and VST3 documents `noteId` as "if not available then -1". Storing
/// that as `Option` keeps a real id distinguishable from a fabricated one.
pub type NoteId = Option<i32>;

/// Decode a wire-format note id, where a negative value means "not supplied".
pub fn note_id_from_wire(raw: i32) -> NoteId {
    (raw >= 0).then_some(raw)
}

/// Encode a note id for the wire. Both formats spell "absent" as `-1`.
pub fn note_id_to_wire(id: NoteId) -> i32 {
    id.unwrap_or(-1)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteEvent {
    NoteOn {
        note_id: NoteId,
        port: i16,
        channel: i16,
        key: i16,
        velocity: f64,
        sample_offset: u32,
    },
    NoteOff {
        note_id: NoteId,
        port: i16,
        channel: i16,
        key: i16,
        velocity: f64,
        sample_offset: u32,
    },
    /// The plugin tells the host a voice ended (CLAP `NOTE_END`). Forwarded so
    /// per-voice graph state can be released.
    NoteEnd {
        note_id: NoteId,
        port: i16,
        channel: i16,
        key: i16,
        sample_offset: u32,
    },
    Expression {
        note_id: NoteId,
        port: i16,
        channel: i16,
        key: i16,
        expression: NoteExpression,
        value: f64,
        sample_offset: u32,
    },
    /// Control change. `value` is normalized to `0.0..=1.0`.
    ///
    /// Held as a number rather than the byte it arrived as: values generated
    /// inside the graph have no reason to be quantized to 7 bits, and both
    /// plugin formats carry parameters as `double`, so nothing on the way out
    /// needs the byte back either.
    Cc {
        port: i16,
        channel: i16,
        cc: u8,
        value: f64,
        sample_offset: u32,
    },
    /// Pitch bend, normalized to `-1.0..=1.0`. Which semitone range that means
    /// is the receiving plugin's business, not ours.
    PitchBend {
        port: i16,
        channel: i16,
        value: f64,
        sample_offset: u32,
    },
    /// Channel aftertouch, normalized to `0.0..=1.0`.
    ChannelPressure {
        port: i16,
        channel: i16,
        value: f64,
        sample_offset: u32,
    },
    /// Polyphonic aftertouch, normalized to `0.0..=1.0`.
    ///
    /// Distinct from `Expression { Pressure }`: this is a MIDI channel-voice
    /// message addressed by key, while a note expression is addressed by
    /// `note_id` and is not MIDI at all.
    PolyPressure {
        port: i16,
        channel: i16,
        key: i16,
        value: f64,
        sample_offset: u32,
    },
    /// Raw 3-byte MIDI for messages with no structured representation here —
    /// program change, song position, and the rest. Anything this enum models
    /// is decoded on the way in and never lands in this variant.
    Midi {
        port: i16,
        data: [u8; 3],
        sample_offset: u32,
    },
}

/// 7-bit MIDI value to `0.0..=1.0`, and back.
fn from7(byte: u8) -> f64 {
    f64::from(byte & 0x7f) / 127.0
}

fn to7(value: f64) -> u8 {
    (value * 127.0).round().clamp(0.0, 127.0) as u8
}

impl NoteEvent {
    /// Classify a 3-byte MIDI message into a structured variant.
    ///
    /// Note-on with velocity 0 is folded into note-off, as every MIDI receiver
    /// must. The resulting note carries no `note_id`, which is exactly true:
    /// raw MIDI has no way to express one.
    pub fn from_midi(port: i16, data: [u8; 3], sample_offset: u32) -> NoteEvent {
        let channel = i16::from(data[0] & 0x0f);
        let key = i16::from(data[1] & 0x7f);
        match data[0] & 0xf0 {
            0x80 => NoteEvent::NoteOff {
                note_id: None,
                port,
                channel,
                key,
                velocity: from7(data[2]),
                sample_offset,
            },
            0x90 if data[2] & 0x7f == 0 => NoteEvent::NoteOff {
                note_id: None,
                port,
                channel,
                key,
                velocity: 0.0,
                sample_offset,
            },
            0x90 => NoteEvent::NoteOn {
                note_id: None,
                port,
                channel,
                key,
                velocity: from7(data[2]),
                sample_offset,
            },
            0xa0 => NoteEvent::PolyPressure {
                port,
                channel,
                key,
                value: from7(data[2]),
                sample_offset,
            },
            0xb0 => NoteEvent::Cc {
                port,
                channel,
                cc: data[1] & 0x7f,
                value: from7(data[2]),
                sample_offset,
            },
            0xd0 => NoteEvent::ChannelPressure {
                port,
                channel,
                value: from7(data[1]),
                sample_offset,
            },
            0xe0 => {
                let raw = i32::from(data[1] & 0x7f) | (i32::from(data[2] & 0x7f) << 7);
                NoteEvent::PitchBend {
                    port,
                    channel,
                    // 8192 is centre. The halves are asymmetric — 8192 below,
                    // 8191 above — so dividing by 8192 throughout keeps centre
                    // exact and merely leaves full-up a hair under 1.0.
                    value: f64::from(raw - 8192) / 8192.0,
                    sample_offset,
                }
            }
            _ => NoteEvent::Midi {
                port,
                data,
                sample_offset,
            },
        }
    }

    /// Render back to 3 MIDI bytes, for backends that speak raw MIDI.
    ///
    /// `None` for what MIDI 1.0 cannot express: note expressions, and
    /// `NoteEnd`, which only ever travels plugin-to-host.
    pub fn to_midi(&self) -> Option<[u8; 3]> {
        let status = |high: u8, channel: i16| high | (channel.max(0) as u8 & 0x0f);
        Some(match *self {
            NoteEvent::NoteOn {
                channel,
                key,
                velocity,
                ..
            } => [status(0x90, channel), key as u8 & 0x7f, to7(velocity)],
            NoteEvent::NoteOff {
                channel,
                key,
                velocity,
                ..
            } => [status(0x80, channel), key as u8 & 0x7f, to7(velocity)],
            NoteEvent::PolyPressure {
                channel,
                key,
                value,
                ..
            } => [status(0xa0, channel), key as u8 & 0x7f, to7(value)],
            NoteEvent::Cc {
                channel, cc, value, ..
            } => [status(0xb0, channel), cc & 0x7f, to7(value)],
            NoteEvent::ChannelPressure { channel, value, .. } => {
                [status(0xd0, channel), to7(value), 0]
            }
            NoteEvent::PitchBend { channel, value, .. } => {
                let raw = (value * 8192.0 + 8192.0).round().clamp(0.0, 16383.0) as u16;
                [
                    status(0xe0, channel),
                    (raw & 0x7f) as u8,
                    ((raw >> 7) & 0x7f) as u8,
                ]
            }
            NoteEvent::Midi { data, .. } => data,
            NoteEvent::NoteEnd { .. } | NoteEvent::Expression { .. } => return None,
        })
    }

    /// The MIDI key this event is about, if it is about one.
    ///
    /// `None` for anything that is not per-key — a control change, say —
    /// because a filter that guessed a key for those would drop them with the
    /// notes they happen to sit next to.
    pub fn key(&self) -> Option<i16> {
        match *self {
            NoteEvent::NoteOn { key, .. }
            | NoteEvent::NoteOff { key, .. }
            | NoteEvent::NoteEnd { key, .. }
            | NoteEvent::Expression { key, .. }
            | NoteEvent::PolyPressure { key, .. } => Some(key),
            NoteEvent::Cc { .. }
            | NoteEvent::PitchBend { .. }
            | NoteEvent::ChannelPressure { .. }
            | NoteEvent::Midi { .. } => None,
        }
    }

    /// The note port this event arrived on.
    pub fn port(&self) -> i16 {
        match *self {
            NoteEvent::NoteOn { port, .. }
            | NoteEvent::NoteOff { port, .. }
            | NoteEvent::NoteEnd { port, .. }
            | NoteEvent::Expression { port, .. }
            | NoteEvent::PolyPressure { port, .. }
            | NoteEvent::Cc { port, .. }
            | NoteEvent::PitchBend { port, .. }
            | NoteEvent::ChannelPressure { port, .. }
            | NoteEvent::Midi { port, .. } => port,
        }
    }

    /// The MIDI channel this event is on, if it is on one.
    pub fn channel(&self) -> Option<i16> {
        match *self {
            NoteEvent::NoteOn { channel, .. }
            | NoteEvent::NoteOff { channel, .. }
            | NoteEvent::NoteEnd { channel, .. }
            | NoteEvent::Expression { channel, .. }
            | NoteEvent::PolyPressure { channel, .. }
            | NoteEvent::Cc { channel, .. }
            | NoteEvent::PitchBend { channel, .. }
            | NoteEvent::ChannelPressure { channel, .. } => Some(channel),
            NoteEvent::Midi { .. } => None,
        }
    }

    /// The note id this event carries, when the variant has one at all.
    pub fn note_id(&self) -> NoteId {
        match *self {
            NoteEvent::NoteOn { note_id, .. }
            | NoteEvent::NoteOff { note_id, .. }
            | NoteEvent::NoteEnd { note_id, .. }
            | NoteEvent::Expression { note_id, .. } => note_id,
            _ => None,
        }
    }

    pub fn sample_offset(&self) -> u32 {
        match *self {
            NoteEvent::NoteOn { sample_offset, .. }
            | NoteEvent::NoteOff { sample_offset, .. }
            | NoteEvent::NoteEnd { sample_offset, .. }
            | NoteEvent::Expression { sample_offset, .. }
            | NoteEvent::Cc { sample_offset, .. }
            | NoteEvent::PitchBend { sample_offset, .. }
            | NoteEvent::ChannelPressure { sample_offset, .. }
            | NoteEvent::PolyPressure { sample_offset, .. }
            | NoteEvent::Midi { sample_offset, .. } => sample_offset,
        }
    }

    /// The same event, timed at `offset` instead.
    pub fn at_offset(mut self, offset: u32) -> NoteEvent {
        let (NoteEvent::NoteOn { sample_offset, .. }
        | NoteEvent::NoteOff { sample_offset, .. }
        | NoteEvent::NoteEnd { sample_offset, .. }
        | NoteEvent::Expression { sample_offset, .. }
        | NoteEvent::Cc { sample_offset, .. }
        | NoteEvent::PitchBend { sample_offset, .. }
        | NoteEvent::ChannelPressure { sample_offset, .. }
        | NoteEvent::PolyPressure { sample_offset, .. }
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

    /// Returns a copy of the event with its sample offset adjusted to
    /// `offset`.
    ///
    /// Used when a block is cut into chunks and each chunk is handed to the
    /// sub-plugin as its own `process` call: an event at sample 40 of the block
    /// is at sample 8 of the chunk that starts at 32, and handing it over still
    /// saying 40 would put it past the end of a 32-sample buffer.
    pub fn at_offset(self, offset: u32) -> Event {
        match self {
            Event::Param(e) => Event::Param(e.at_offset(offset)),
            Event::Note(e) => Event::Note(e.at_offset(offset)),
        }
    }
}

/// Collects events emitted *by* the sub-plugin during `process`.
///
/// A plain owned buffer rather than a callback: a callback would be a
/// reference crossing the boundary, which this API does not allow.
#[derive(Debug, Clone, Default)]
pub struct EventSink {
    events: Vec<Event>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate so the audio thread never grows the buffer.
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

/// Transport, tempo, and playback position state for an audio processing
/// block.
///
/// `subhost-adapter` forwards the DAW's context to the sub-plugin, adjusting
/// for any latency the wrapper itself introduces.
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
    /// The loop's bounds in quarter notes, when the host knows them.
    ///
    /// Separate from `loop_active` because the two are not the same claim: a
    /// host can know a loop is running without being able to say where it is,
    /// and both formats care about the difference. VST3 spells it out with
    /// `kCycleValid`; CLAP has no such flag, so the CLAP backend reports no
    /// loop at all rather than a loop of length zero.
    pub loop_range_music: Option<(f64, f64)>,
    /// The same bounds in seconds, which CLAP asks for separately and VST3
    /// does not carry at all.
    pub loop_range_seconds: Option<(f64, f64)>,
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
            loop_range_music: None,
            loop_range_seconds: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_classification_round_trips() {
        let cases: [[u8; 3]; 6] = [
            [0x91, 60, 100], // note on
            [0x81, 60, 64],  // note off
            [0xa1, 60, 100], // poly pressure
            [0xb1, 64, 127], // control change
            [0xd1, 90, 0],   // channel pressure
            [0xc1, 5, 0],    // program change: no structured form
        ];
        for data in cases {
            let event = NoteEvent::from_midi(0, data, 7);
            assert_eq!(event.sample_offset(), 7, "{data:02x?}");
            assert_eq!(event.to_midi(), Some(data), "{data:02x?}");
        }
    }

    #[test]
    fn a_note_on_at_zero_velocity_is_a_note_off() {
        assert!(matches!(
            NoteEvent::from_midi(0, [0x90, 60, 0], 0),
            NoteEvent::NoteOff { velocity, .. } if velocity == 0.0
        ));
    }

    /// The centre of the bend range is the one value users notice when it
    /// drifts, and the 14-bit encoding is asymmetric around it.
    #[test]
    fn pitch_bend_centre_is_exact() {
        let centre = NoteEvent::from_midi(0, [0xe0, 0, 64], 0);
        assert!(matches!(centre, NoteEvent::PitchBend { value, .. } if value == 0.0));
        assert_eq!(centre.to_midi(), Some([0xe0, 0, 64]));
    }

    #[test]
    fn controllers_carry_no_key() {
        assert_eq!(NoteEvent::from_midi(0, [0xb0, 64, 127], 0).key(), None);
        assert_eq!(NoteEvent::from_midi(0, [0xa0, 60, 127], 0).key(), Some(60));
    }

    #[test]
    fn an_absent_note_id_survives_the_wire() {
        assert_eq!(note_id_from_wire(note_id_to_wire(None)), None);
        assert_eq!(note_id_from_wire(note_id_to_wire(Some(0))), Some(0));
        assert_eq!(note_id_from_wire(note_id_to_wire(Some(42))), Some(42));
    }
}
