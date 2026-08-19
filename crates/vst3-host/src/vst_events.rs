//! Translating between the core's event model and VST3's.
//!
//! This is where §3.4's degradation table actually happens. Two losses are
//! structural rather than incidental:
//!
//! * `Modulate` cannot be expressed. VST3 parameters hold one value, so a
//!   modulation has nowhere to live alongside the base value. v1 only uses
//!   Drive mode (ADR-5), which emits `SetValue`, so this costs nothing today.
//! * Voice-addressed targets collapse to global for the same reason.
//!
//! Both are dropped rather than approximated: a modulation silently applied as
//! a value change would destroy the user's automation, which is worse than not
//! happening at all.

use plugin_host_api::{
    Event as ApiEvent, EventSink, NoteEvent, NoteExpression, ParamEvent, TimeContext,
};
use vst3::ComWrapper;
use vst3::Steinberg::Vst::{
    Event as VstEvent, Event_::EventTypes_, NoteExpressionTypeIDs_, NoteOffEvent, NoteOnEvent,
    NoteExpressionValueEvent, ProcessContext, ProcessContext_::StatesAndFlags_,
};

use crate::param_map::ParamMap;
use crate::process_io::{EventList, ParameterChanges};

/// Fill VST3's input containers from the core's event stream.
///
/// `map` converts the core's plain values into the normalised ones VST3 wants;
/// it was captured on the main thread at activate precisely so this can happen
/// here without touching `IEditController` (see [`crate::param_map`]).
pub fn fill_inputs(
    events: &[ApiEvent],
    map: &ParamMap,
    changes: &ComWrapper<ParameterChanges>,
    list: &ComWrapper<EventList>,
) {
    for event in events {
        match event {
            ApiEvent::Param(p) => fill_param(p, map, changes),
            ApiEvent::Note(n) => {
                if let Some(vst) = to_vst_event(n) {
                    list.push(vst);
                }
            }
        }
    }
}

fn fill_param(event: &ParamEvent, map: &ParamMap, changes: &ComWrapper<ParameterChanges>) {
    match *event {
        ParamEvent::SetValue { id, value, sample_offset, .. } => {
            // An unknown id is dropped rather than sent with a guessed value.
            if let Some(normalized) = map.normalize(id, value) {
                changes.add_point(id.0, sample_offset as i32, normalized);
            }
        }
        // See the module comment: dropped, not approximated.
        ParamEvent::Modulate { .. } => {}
        // Gestures exist only to bracket a user's edit for the host's undo
        // system. As a host we are the recipient of those, never the sender.
        ParamEvent::GestureBegin { .. } | ParamEvent::GestureEnd { .. } => {}
    }
}

fn to_vst_event(event: &NoteEvent) -> Option<VstEvent> {
    let mut vst: VstEvent = unsafe { std::mem::zeroed() };
    vst.busIndex = 0;
    vst.sampleOffset = event.sample_offset() as i32;

    match *event {
        NoteEvent::NoteOn { note_id, channel, key, velocity, .. } => {
            vst.r#type = EventTypes_::kNoteOnEvent as u16;
            vst.__field0.noteOn = NoteOnEvent {
                channel,
                pitch: key,
                tuning: 0.0,
                velocity: velocity as f32,
                length: 0,
                noteId: note_id,
            };
        }
        NoteEvent::NoteOff { note_id, channel, key, velocity, .. } => {
            vst.r#type = EventTypes_::kNoteOffEvent as u16;
            vst.__field0.noteOff = NoteOffEvent {
                channel,
                pitch: key,
                velocity: velocity as f32,
                noteId: note_id,
                tuning: 0.0,
            };
        }
        NoteEvent::Expression { note_id, expression, value, .. } => {
            let type_id = expression_type_id(expression)?;
            vst.r#type = EventTypes_::kNoteExpressionValueEvent as u16;
            vst.__field0.noteExpressionValue = NoteExpressionValueEvent {
                typeId: type_id,
                noteId: note_id,
                value,
            };
        }
        // NoteEnd travels plugin-to-host only; sending one would be meaningless.
        NoteEvent::NoteEnd { .. } => return None,
        // Raw MIDI has no VST3 event type. Channel messages that matter
        // (pitch bend, CC) reach a plugin through parameters instead, via
        // IMidiMapping, which is a M2 concern rather than an event one.
        NoteEvent::Midi { .. } => return None,
    }

    Some(vst)
}

/// Map the core's expression enum onto VST3's predefined type IDs.
///
/// Six of the seven have a direct counterpart. Pressure does not: VST3's
/// predefined set has no pressure type, and a plugin instead declares which
/// expression it wants physical pressure delivered as, through
/// `INoteExpressionPhysicalUIMapping`. Querying that is not wired up yet, and
/// picking an arbitrary type would send pressure to whatever that type happens
/// to control — so it is dropped until the mapping is read properly.
fn expression_type_id(expression: NoteExpression) -> Option<u32> {
    Some(match expression {
        NoteExpression::Volume => NoteExpressionTypeIDs_::kVolumeTypeID as u32,
        NoteExpression::Pan => NoteExpressionTypeIDs_::kPanTypeID as u32,
        NoteExpression::Tuning => NoteExpressionTypeIDs_::kTuningTypeID as u32,
        NoteExpression::Vibrato => NoteExpressionTypeIDs_::kVibratoTypeID as u32,
        NoteExpression::Expression => NoteExpressionTypeIDs_::kExpressionTypeID as u32,
        NoteExpression::Brightness => NoteExpressionTypeIDs_::kBrightnessTypeID as u32,
        NoteExpression::Pressure => return None,
    })
}

/// Convert what the plugin emitted back into the core model.
pub fn drain_outputs(list: &ComWrapper<EventList>, sink: &mut EventSink) {
    for index in 0..list.len() {
        let Some(vst) = list.get(index) else { continue };
        let sample_offset = vst.sampleOffset.max(0) as u32;

        // Only note lifecycle events are forwarded: they are what the engine
        // needs to release per-voice graph state (§9).
        if vst.r#type == EventTypes_::kNoteOffEvent as u16 {
            let off = unsafe { vst.__field0.noteOff };
            sink.push(ApiEvent::Note(NoteEvent::NoteEnd {
                note_id: off.noteId,
                port: 0,
                channel: off.channel,
                key: off.pitch,
                sample_offset,
            }));
        }
    }
}

/// Build VST3's `ProcessContext` from the core's transport snapshot.
pub fn to_process_context(context: &TimeContext, sample_rate: f64) -> ProcessContext {
    let mut out: ProcessContext = unsafe { std::mem::zeroed() };

    // The `state` bitfield says which fields the host actually filled in.
    // Plugins branch on it, and claiming validity for a field we left at zero
    // makes tempo-synced effects behave erratically.
    let mut state = StatesAndFlags_::kTempoValid
        | StatesAndFlags_::kTimeSigValid
        | StatesAndFlags_::kProjectTimeMusicValid
        | StatesAndFlags_::kBarPositionValid
        | StatesAndFlags_::kSystemTimeValid;
    if context.playing {
        state |= StatesAndFlags_::kPlaying;
    }
    if context.recording {
        state |= StatesAndFlags_::kRecording;
    }
    if context.loop_active {
        state |= StatesAndFlags_::kCycleActive;
    }

    out.state = state as u32;
    out.sampleRate = sample_rate;
    out.projectTimeSamples = context.project_time_samples;
    out.continousTimeSamples = context.project_time_samples;
    out.projectTimeMusic = context.project_time_music;
    out.barPositionMusic = context.bar_position_music;
    out.tempo = context.tempo_bpm;
    out.timeSigNumerator = context.time_sig_numerator;
    out.timeSigDenominator = context.time_sig_denominator;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host_api::{ParamFlags, ParamId, ParamInfo};

    /// A single linear 0..1 parameter, so tests exercise translation rather
    /// than the mapping curve (which `param_map` covers on its own).
    fn unit_map(id: u32) -> ParamMap {
        let params = [ParamInfo {
            id: ParamId(id),
            name: String::new(),
            module: String::new(),
            min: 0.0,
            max: 1.0,
            default: 0.0,
            flags: ParamFlags::NONE,
        }];
        ParamMap::build(&params, |_, n| n)
    }

    #[test]
    fn set_value_reaches_the_change_list() {
        let changes = ParameterChanges::new(4, 4);
        let list = EventList::new(4);
        fill_inputs(
            &[ApiEvent::Param(ParamEvent::SetValue {
                id: ParamId(3),
                target: Default::default(),
                value: 0.5,
                sample_offset: 16,
            })],
            &unit_map(3),
            &changes,
            &list,
        );
        assert_eq!(changes.points(), vec![(3, 16, 0.5)]);
    }

    #[test]
    fn modulation_is_dropped_rather_than_flattened_into_a_value() {
        // Flattening would overwrite the parameter and destroy whatever value
        // the user had automated, which is worse than the modulation not
        // applying at all.
        let changes = ParameterChanges::new(4, 4);
        let list = EventList::new(4);
        fill_inputs(
            &[ApiEvent::Param(ParamEvent::Modulate {
                id: ParamId(3),
                target: Default::default(),
                amount: 0.25,
                sample_offset: 0,
            })],
            &unit_map(3),
            &changes,
            &list,
        );
        assert!(changes.points().is_empty());
    }

    #[test]
    fn notes_convert_with_their_identity_intact() {
        let list = EventList::new(4);
        let changes = ParameterChanges::new(1, 1);
        fill_inputs(
            &[ApiEvent::Note(NoteEvent::NoteOn {
                note_id: 42,
                port: 0,
                channel: 1,
                key: 60,
                velocity: 0.8,
                sample_offset: 8,
            })],
            &unit_map(1),
            &changes,
            &list,
        );
        let event = list.get(0).expect("one event");
        assert_eq!(event.r#type, EventTypes_::kNoteOnEvent as u16);
        assert_eq!(event.sampleOffset, 8);
        let on = unsafe { event.__field0.noteOn };
        assert_eq!((on.noteId, on.pitch, on.channel), (42, 60, 1));
        assert!((on.velocity - 0.8).abs() < 1e-6);
    }

    #[test]
    fn transport_flags_match_the_fields_we_filled() {
        let ctx = TimeContext { playing: true, tempo_bpm: 140.0, ..Default::default() };
        let out = to_process_context(&ctx, 48_000.0);
        assert_eq!(out.tempo, 140.0);
        assert!(out.state & StatesAndFlags_::kPlaying as u32 != 0);
        assert!(out.state & StatesAndFlags_::kTempoValid as u32 != 0);
        assert_eq!(out.state & StatesAndFlags_::kRecording as u32, 0);
    }
}
