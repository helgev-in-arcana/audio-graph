//! Translation between host engine event types and VST3 event representations.
//!
//! Parameter updates and note events are converted into VST3 parameter change queues
//! and event lists. Note that VST3 parameters hold a single normalized value, so
//! per-voice parameter addressing and separate non-destructive modulation streams
//! cannot be represented directly and are dropped rather than approximated as value changes
//! (forwarding modulation as a value change would destroy the user's automation).

use plugin_host_api::{
    Event as ApiEvent, EventSink, NoteEvent, NoteExpression, ParamEvent, TimeContext,
    note_id_from_wire, note_id_to_wire,
};
use vst3::ComWrapper;
use vst3::Steinberg::Vst::{
    Event as VstEvent, Event_::EventTypes_, NoteExpressionTypeIDs_, NoteExpressionValueEvent,
    NoteOffEvent, NoteOnEvent, PolyPressureEvent, ProcessContext, ProcessContext_::StatesAndFlags_,
};

use crate::midi_map::{self, MidiMap};
use crate::param_map::ParamMap;
use crate::process_io::{EventList, ParameterChanges};

/// Fill VST3's input containers from the core's event stream.
///
/// `map` converts the core's plain values into the normalised ones VST3 wants;
/// it was captured on the main thread at activate precisely so this can happen
/// here without touching `IEditController` (see [`crate::param_map`]).
/// `midi` says which parameter each MIDI controller stands for, which in VST3
/// is the only route a controller has (see [`crate::midi_map`]).
pub fn fill_inputs(
    events: &[ApiEvent],
    map: &ParamMap,
    midi: &MidiMap,
    changes: &ComWrapper<ParameterChanges>,
    list: &ComWrapper<EventList>,
) {
    for event in events {
        match event {
            ApiEvent::Param(p) => fill_param(p, map, changes),
            ApiEvent::Note(n) => {
                if let Some((controller, channel, value)) = as_controller(n) {
                    fill_controller(n, midi, controller, channel, value, changes);
                } else if let Some(vst) = to_vst_event(n) {
                    list.push(vst);
                }
            }
        }
    }
}

/// The controller number, channel and normalised value of a MIDI controller
/// message, or `None` for everything that is not one.
fn as_controller(event: &NoteEvent) -> Option<(u16, i16, f64)> {
    Some(match *event {
        NoteEvent::Cc {
            channel, cc, value, ..
        } => (u16::from(cc), channel, value),
        NoteEvent::ChannelPressure { channel, value, .. } => (midi_map::AFTERTOUCH, channel, value),
        // VST3 has no signed controller value: the parameter runs 0..1 with
        // 0.5 at rest, the way a bend wheel reads to the plugin.
        NoteEvent::PitchBend { channel, value, .. } => {
            (midi_map::PITCH_BEND, channel, value * 0.5 + 0.5)
        }
        _ => return None,
    })
}

/// Send a controller as the parameter change the format requires.
///
/// The value goes in as-is rather than through [`ParamMap`]. That is not an
/// oversight: `ParamMap` converts *plain* values, and a controller has no
/// plain domain — CC 64 at 100 is a wheel position, not a cutoff in hertz.
/// The plugin defines its mapped parameter so that the controller's own range
/// covers 0..1, which is exactly what we already hold.
fn fill_controller(
    event: &NoteEvent,
    midi: &MidiMap,
    controller: u16,
    channel: i16,
    value: f64,
    changes: &ComWrapper<ParameterChanges>,
) {
    // No mapping means the plugin does not answer to this controller. Nothing
    // to approximate: there is no other door.
    let Some(id) = midi.param(channel, controller) else {
        return;
    };
    changes.add_point(id, event.sample_offset() as i32, value.clamp(0.0, 1.0));
}

fn fill_param(event: &ParamEvent, map: &ParamMap, changes: &ComWrapper<ParameterChanges>) {
    match *event {
        ParamEvent::SetValue {
            id,
            value,
            sample_offset,
            ..
        } => {
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
        NoteEvent::NoteOn {
            note_id,
            channel,
            key,
            velocity,
            ..
        } => {
            vst.r#type = EventTypes_::kNoteOnEvent as u16;
            vst.__field0.noteOn = NoteOnEvent {
                channel,
                pitch: key,
                tuning: 0.0,
                velocity: velocity as f32,
                length: 0,
                noteId: note_id_to_wire(note_id),
            };
        }
        NoteEvent::NoteOff {
            note_id,
            channel,
            key,
            velocity,
            ..
        } => {
            vst.r#type = EventTypes_::kNoteOffEvent as u16;
            vst.__field0.noteOff = NoteOffEvent {
                channel,
                pitch: key,
                velocity: velocity as f32,
                noteId: note_id_to_wire(note_id),
                tuning: 0.0,
            };
        }
        NoteEvent::Expression {
            note_id,
            expression,
            value,
            ..
        } => {
            let type_id = expression_type_id(expression)?;
            vst.r#type = EventTypes_::kNoteExpressionValueEvent as u16;
            vst.__field0.noteExpressionValue = NoteExpressionValueEvent {
                typeId: type_id,
                noteId: note_id_to_wire(note_id),
                value,
            };
        }
        // NoteEnd travels plugin-to-host only; sending one would be meaningless.
        NoteEvent::NoteEnd { .. } => return None,
        // Polyphonic aftertouch is the one MIDI channel-voice message VST3
        // carries as an event; it addresses a note, so it belongs here rather
        // than on the parameter side.
        NoteEvent::PolyPressure {
            channel,
            key,
            value,
            ..
        } => {
            vst.r#type = EventTypes_::kPolyPressureEvent as u16;
            vst.__field0.polyPressure = PolyPressureEvent {
                channel,
                pitch: key,
                pressure: value as f32,
                noteId: -1,
            };
        }
        // Controllers never reach here: `fill_inputs` sends them through
        // IMidiMapping, which is the only route VST3 gives them. Raw bytes
        // have no VST3 event at all.
        NoteEvent::Cc { .. }
        | NoteEvent::PitchBend { .. }
        | NoteEvent::ChannelPressure { .. }
        | NoteEvent::Midi { .. } => return None,
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

        // Forward note-off lifecycle events to the host event sink. They are what the
        // engine needs to release per-voice graph state.
        if vst.r#type == EventTypes_::kNoteOffEvent as u16 {
            let off = unsafe { vst.__field0.noteOff };
            sink.push(ApiEvent::Note(NoteEvent::NoteEnd {
                note_id: note_id_from_wire(off.noteId),
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
    // `kCycleActive` says a loop is running; `kCycleValid` says where it is.
    // Only the second one depends on the host having told us.
    if let Some((start, end)) = context.loop_range_music {
        state |= StatesAndFlags_::kCycleValid;
        out.cycleStartMusic = start;
        out.cycleEndMusic = end;
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
            &MidiMap::empty(),
            &changes,
            &list,
        );
        assert_eq!(changes.points(), vec![(3, 16, 0.5)]);
    }

    /// The sustain pedal is the reason this route exists: without it, CC64
    /// reaches a VST3 plugin by no path at all.
    #[test]
    fn a_controller_arrives_as_the_parameter_it_is_mapped_to() {
        let changes = ParameterChanges::new(4, 4);
        let list = EventList::new(4);
        let midi = MidiMap::from_assignments(&[(0, 64, 900), (0, midi_map::PITCH_BEND, 901)]);
        fill_inputs(
            &[
                ApiEvent::Note(NoteEvent::Cc {
                    port: 0,
                    channel: 0,
                    cc: 64,
                    value: 1.0,
                    sample_offset: 8,
                }),
                // At rest, which is the midpoint of the parameter VST3 maps it
                // to rather than the zero the core model uses.
                ApiEvent::Note(NoteEvent::PitchBend {
                    port: 0,
                    channel: 0,
                    value: 0.0,
                    sample_offset: 9,
                }),
            ],
            &unit_map(3),
            &midi,
            &changes,
            &list,
        );
        assert_eq!(changes.points(), vec![(900, 8, 1.0), (901, 9, 0.5)]);
        assert_eq!(list.len(), 0, "controllers are not VST3 events");
    }

    /// An unmapped controller has nowhere else to go, and inventing a
    /// parameter for it would drive something the user never touched.
    #[test]
    fn an_unmapped_controller_is_dropped() {
        let changes = ParameterChanges::new(4, 4);
        let list = EventList::new(4);
        fill_inputs(
            &[ApiEvent::Note(NoteEvent::Cc {
                port: 0,
                channel: 0,
                cc: 64,
                value: 1.0,
                sample_offset: 0,
            })],
            &unit_map(3),
            &MidiMap::empty(),
            &changes,
            &list,
        );
        assert!(changes.points().is_empty());
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn sub_block_updates_keep_their_offsets_and_share_one_queue() {
        // Multiple parameter points across a block must land in a single queue
        // with their sample offsets preserved — collapsing them to the block start
        // would make a fast LFO sound stepped.
        let changes = ParameterChanges::new(4, 64);
        let list = EventList::new(4);
        let events: Vec<ApiEvent> = (0..8)
            .map(|i| {
                ApiEvent::Param(ParamEvent::SetValue {
                    id: ParamId(3),
                    target: Default::default(),
                    value: i as f64 / 8.0,
                    sample_offset: i * 32,
                })
            })
            .collect();

        fill_inputs(&events, &unit_map(3), &MidiMap::empty(), &changes, &list);

        let ids: std::collections::BTreeSet<_> =
            changes.points().iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids.len(), 1, "all points belong to one parameter queue");
        let offsets: Vec<i32> = changes.points().iter().map(|(_, o, _)| *o).collect();
        assert_eq!(offsets, vec![0, 32, 64, 96, 128, 160, 192, 224]);
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
            &MidiMap::empty(),
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
                note_id: Some(42),
                port: 0,
                channel: 1,
                key: 60,
                velocity: 0.8,
                sample_offset: 8,
            })],
            &unit_map(1),
            &MidiMap::empty(),
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
        let ctx = TimeContext {
            playing: true,
            tempo_bpm: 140.0,
            ..Default::default()
        };
        let out = to_process_context(&ctx, 48_000.0);
        assert_eq!(out.tempo, 140.0);
        assert!(out.state & StatesAndFlags_::kPlaying as u32 != 0);
        assert!(out.state & StatesAndFlags_::kTempoValid as u32 != 0);
        assert_eq!(out.state & StatesAndFlags_::kRecording as u32, 0);
    }

    #[test]
    fn a_running_loop_is_only_described_when_its_bounds_are_known() {
        let running = TimeContext {
            loop_active: true,
            ..Default::default()
        };
        let out = to_process_context(&running, 48_000.0);
        assert!(out.state & StatesAndFlags_::kCycleActive as u32 != 0);
        // Active but not valid: the two are separate claims, and only the
        // second one needs bounds.
        assert_eq!(out.state & StatesAndFlags_::kCycleValid as u32, 0);

        let described = TimeContext {
            loop_range_music: Some((4.0, 8.0)),
            ..running
        };
        let out = to_process_context(&described, 48_000.0);
        assert!(out.state & StatesAndFlags_::kCycleValid as u32 != 0);
        assert_eq!(out.cycleStartMusic, 4.0);
        assert_eq!(out.cycleEndMusic, 8.0);
    }
}
