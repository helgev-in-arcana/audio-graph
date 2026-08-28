//! Translating between the core event model and CLAP event structures.

use std::ffi::c_void;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_END, CLAP_EVENT_NOTE_EXPRESSION,
    CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_GESTURE_BEGIN,
    CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD, CLAP_EVENT_PARAM_VALUE,
    CLAP_NOTE_EXPRESSION_BRIGHTNESS, CLAP_NOTE_EXPRESSION_EXPRESSION, CLAP_NOTE_EXPRESSION_PAN,
    CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING, CLAP_NOTE_EXPRESSION_VIBRATO,
    CLAP_NOTE_EXPRESSION_VOLUME, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING,
    CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_midi, clap_event_note,
    clap_event_note_expression, clap_event_param_gesture, clap_event_param_mod,
    clap_event_param_value, clap_event_transport, clap_input_events, clap_note_expression,
    clap_output_events,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use plugin_host_api::{
    Event, EventSink, NoteEvent, NoteExpression, ParamEvent, ParamId, Target, TimeContext,
};

/// One event, wide enough for any CLAP event this host sends or receives.
///
/// A union rather than a `Vec<u8>` arena: every event has to be handed to the
/// plugin as an aligned `clap_event_header*`, and a union gives that for free
/// while keeping the list indexable, which `clap_input_events::get` requires.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union RawEvent {
    pub header: clap_event_header,
    pub note: clap_event_note,
    pub expression: clap_event_note_expression,
    pub param_value: clap_event_param_value,
    pub param_mod: clap_event_param_mod,
    pub gesture: clap_event_param_gesture,
    pub midi: clap_event_midi,
}

impl RawEvent {
    fn zeroed() -> RawEvent {
        // Safe to zero: every member is plain data, and the fields that matter
        // are written before the event is ever read.
        unsafe { std::mem::zeroed() }
    }

    fn time(&self) -> u32 {
        unsafe { self.header.time }
    }
}

fn header(size: usize, time: u32, kind: u16) -> clap_event_header {
    clap_event_header {
        size: size as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_: kind,
        // No IS_LIVE: everything reaching a sub-plugin came from the graph or
        // from the DAW's automation, neither of which is a live gesture the
        // plugin should record.
        flags: 0,
    }
}

/// How a [`Target`] is spelled in CLAP's four addressing fields.
///
/// `-1` is CLAP's wildcard, so `Global` is all wildcards and the narrower
/// targets fill in only what they name.
fn addressing(target: Target) -> (i32, i16, i16, i16) {
    match target {
        Target::Global => (-1, -1, -1, -1),
        Target::NoteId(id) => (id, -1, -1, -1),
        Target::Key { channel, key } => (-1, -1, channel, key),
        Target::Channel(channel) => (-1, -1, channel, -1),
        Target::Port(port) => (-1, port, -1, -1),
    }
}

fn to_clap_expression(e: NoteExpression) -> clap_note_expression {
    match e {
        NoteExpression::Volume => CLAP_NOTE_EXPRESSION_VOLUME,
        NoteExpression::Pan => CLAP_NOTE_EXPRESSION_PAN,
        NoteExpression::Tuning => CLAP_NOTE_EXPRESSION_TUNING,
        NoteExpression::Vibrato => CLAP_NOTE_EXPRESSION_VIBRATO,
        NoteExpression::Expression => CLAP_NOTE_EXPRESSION_EXPRESSION,
        NoteExpression::Brightness => CLAP_NOTE_EXPRESSION_BRIGHTNESS,
        NoteExpression::Pressure => CLAP_NOTE_EXPRESSION_PRESSURE,
    }
}

fn from_clap_expression(id: clap_note_expression) -> Option<NoteExpression> {
    Some(match id {
        CLAP_NOTE_EXPRESSION_VOLUME => NoteExpression::Volume,
        CLAP_NOTE_EXPRESSION_PAN => NoteExpression::Pan,
        CLAP_NOTE_EXPRESSION_TUNING => NoteExpression::Tuning,
        CLAP_NOTE_EXPRESSION_VIBRATO => NoteExpression::Vibrato,
        CLAP_NOTE_EXPRESSION_EXPRESSION => NoteExpression::Expression,
        CLAP_NOTE_EXPRESSION_BRIGHTNESS => NoteExpression::Brightness,
        CLAP_NOTE_EXPRESSION_PRESSURE => NoteExpression::Pressure,
        _ => return None,
    })
}

/// Build the CLAP form of one core event.
///
/// Returns `None` for anything CLAP has no place for, which today is nothing —
/// the shape is here because the core model is allowed to grow and silently
/// sending a malformed event is the worse failure.
fn encode(event: &Event) -> Option<RawEvent> {
    let mut raw = RawEvent::zeroed();
    match *event {
        Event::Param(ParamEvent::SetValue {
            id,
            target,
            value,
            sample_offset,
        }) => {
            let (note_id, port_index, channel, key) = addressing(target);
            raw.param_value = clap_event_param_value {
                header: header(
                    size_of::<clap_event_param_value>(),
                    sample_offset,
                    CLAP_EVENT_PARAM_VALUE,
                ),
                param_id: id.0,
                // Cookies are an optimisation the plugin hands out at
                // `get_info` time; null is always legal and means "look it up".
                cookie: std::ptr::null_mut(),
                note_id,
                port_index,
                channel,
                key,
                value,
            };
        }
        Event::Param(ParamEvent::Modulate {
            id,
            target,
            amount,
            sample_offset,
        }) => {
            let (note_id, port_index, channel, key) = addressing(target);
            raw.param_mod = clap_event_param_mod {
                header: header(
                    size_of::<clap_event_param_mod>(),
                    sample_offset,
                    CLAP_EVENT_PARAM_MOD,
                ),
                param_id: id.0,
                cookie: std::ptr::null_mut(),
                note_id,
                port_index,
                channel,
                key,
                amount,
            };
        }
        Event::Param(ParamEvent::GestureBegin { id }) => {
            raw.gesture = clap_event_param_gesture {
                header: header(
                    size_of::<clap_event_param_gesture>(),
                    0,
                    CLAP_EVENT_PARAM_GESTURE_BEGIN,
                ),
                param_id: id.0,
            };
        }
        Event::Param(ParamEvent::GestureEnd { id }) => {
            raw.gesture = clap_event_param_gesture {
                header: header(
                    size_of::<clap_event_param_gesture>(),
                    0,
                    CLAP_EVENT_PARAM_GESTURE_END,
                ),
                param_id: id.0,
            };
        }
        Event::Note(NoteEvent::NoteOn {
            note_id,
            port,
            channel,
            key,
            velocity,
            sample_offset,
        }) => {
            raw.note = clap_event_note {
                header: header(
                    size_of::<clap_event_note>(),
                    sample_offset,
                    CLAP_EVENT_NOTE_ON,
                ),
                note_id,
                port_index: port,
                channel,
                key,
                velocity,
            };
        }
        Event::Note(NoteEvent::NoteOff {
            note_id,
            port,
            channel,
            key,
            velocity,
            sample_offset,
        }) => {
            raw.note = clap_event_note {
                header: header(
                    size_of::<clap_event_note>(),
                    sample_offset,
                    CLAP_EVENT_NOTE_OFF,
                ),
                note_id,
                port_index: port,
                channel,
                key,
                velocity,
            };
        }
        Event::Note(NoteEvent::NoteEnd {
            note_id,
            port,
            channel,
            key,
            sample_offset,
        }) => {
            // Host-to-plugin NOTE_END is not a thing a plugin expects, but the
            // core model carries the variant in both directions and refusing to
            // encode it here would make round-tripping a stream lossy.
            raw.note = clap_event_note {
                header: header(
                    size_of::<clap_event_note>(),
                    sample_offset,
                    CLAP_EVENT_NOTE_END,
                ),
                note_id,
                port_index: port,
                channel,
                key,
                velocity: 0.0,
            };
        }
        Event::Note(NoteEvent::Expression {
            note_id,
            port,
            channel,
            key,
            expression,
            value,
            sample_offset,
        }) => {
            raw.expression = clap_event_note_expression {
                header: header(
                    size_of::<clap_event_note_expression>(),
                    sample_offset,
                    CLAP_EVENT_NOTE_EXPRESSION,
                ),
                expression_id: to_clap_expression(expression),
                note_id,
                port_index: port,
                channel,
                key,
                value,
            };
        }
        Event::Note(NoteEvent::Midi {
            port,
            data,
            sample_offset,
        }) => {
            raw.midi = clap_event_midi {
                header: header(size_of::<clap_event_midi>(), sample_offset, CLAP_EVENT_MIDI),
                port_index: port.max(0) as u16,
                data,
            };
        }
    }
    Some(raw)
}

/// Read one CLAP event the plugin emitted back into the core model.
///
/// # Safety
/// `raw` must be a complete event of the type its header names.
unsafe fn decode(raw: &RawEvent) -> Option<Event> {
    let header = unsafe { raw.header };
    if header.space_id != CLAP_CORE_EVENT_SPACE_ID {
        // Another vendor's event space. Ignored rather than guessed at: the
        // layout past the header is not ours to read.
        return None;
    }
    Some(match header.type_ {
        CLAP_EVENT_PARAM_VALUE => {
            let e = unsafe { raw.param_value };
            Event::Param(ParamEvent::SetValue {
                id: ParamId(e.param_id),
                target: decode_target(e.note_id, e.port_index, e.channel, e.key),
                value: e.value,
                sample_offset: header.time,
            })
        }
        CLAP_EVENT_PARAM_MOD => {
            let e = unsafe { raw.param_mod };
            Event::Param(ParamEvent::Modulate {
                id: ParamId(e.param_id),
                target: decode_target(e.note_id, e.port_index, e.channel, e.key),
                amount: e.amount,
                sample_offset: header.time,
            })
        }
        CLAP_EVENT_PARAM_GESTURE_BEGIN => Event::Param(ParamEvent::GestureBegin {
            id: ParamId(unsafe { raw.gesture }.param_id),
        }),
        CLAP_EVENT_PARAM_GESTURE_END => Event::Param(ParamEvent::GestureEnd {
            id: ParamId(unsafe { raw.gesture }.param_id),
        }),
        CLAP_EVENT_NOTE_ON => {
            let e = unsafe { raw.note };
            Event::Note(NoteEvent::NoteOn {
                note_id: e.note_id,
                port: e.port_index,
                channel: e.channel,
                key: e.key,
                velocity: e.velocity,
                sample_offset: header.time,
            })
        }
        CLAP_EVENT_NOTE_OFF => {
            let e = unsafe { raw.note };
            Event::Note(NoteEvent::NoteOff {
                note_id: e.note_id,
                port: e.port_index,
                channel: e.channel,
                key: e.key,
                velocity: e.velocity,
                sample_offset: header.time,
            })
        }
        // The output event that notifies the host when a voice has ended.
        CLAP_EVENT_NOTE_END => {
            let e = unsafe { raw.note };
            Event::Note(NoteEvent::NoteEnd {
                note_id: e.note_id,
                port: e.port_index,
                channel: e.channel,
                key: e.key,
                sample_offset: header.time,
            })
        }
        CLAP_EVENT_NOTE_EXPRESSION => {
            let e = unsafe { raw.expression };
            Event::Note(NoteEvent::Expression {
                note_id: e.note_id,
                port: e.port_index,
                channel: e.channel,
                key: e.key,
                expression: from_clap_expression(e.expression_id)?,
                value: e.value,
                sample_offset: header.time,
            })
        }
        CLAP_EVENT_MIDI => {
            let e = unsafe { raw.midi };
            Event::Note(NoteEvent::Midi {
                port: e.port_index as i16,
                data: e.data,
                sample_offset: header.time,
            })
        }
        // NOTE_CHOKE, MIDI_SYSEX, MIDI2 and TRANSPORT have no core
        // representation. Dropped, not approximated.
        _ => return None,
    })
}

fn decode_target(note_id: i32, port: i16, channel: i16, key: i16) -> Target {
    if note_id >= 0 {
        Target::NoteId(note_id)
    } else if key >= 0 {
        Target::Key { channel, key }
    } else if channel >= 0 {
        Target::Channel(channel)
    } else if port >= 0 {
        Target::Port(port)
    } else {
        Target::Global
    }
}

/// The event list handed to `process` as `in_events`.
///
/// Owns its storage and is filled fresh each block. The vtable's `ctx` is
/// pointed at `self` immediately before the pointer is handed over, so the
/// buffer is free to live wherever its owner puts it.
pub(crate) struct InputEvents {
    raw: clap_input_events,
    events: Vec<RawEvent>,
}

impl InputEvents {
    /// Reserve room for `capacity` events. New events beyond capacity are dropped
    /// to avoid allocations on real-time threads.
    pub(crate) fn new(capacity: usize) -> InputEvents {
        InputEvents {
            raw: clap_input_events {
                ctx: std::ptr::null_mut(),
                size: Some(input_size),
                get: Some(input_get),
            },
            events: Vec::with_capacity(capacity),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.events.clear();
    }

    /// Append a core event. Silently ignored once the buffer is full: dropping
    /// one automation point beats allocating on the audio thread.
    pub(crate) fn push(&mut self, event: &Event) {
        if self.events.len() == self.events.capacity() {
            return;
        }
        if let Some(raw) = encode(event) {
            self.events.push(raw);
        }
    }

    /// Append a bare parameter set, which is what a main-thread edit becomes.
    pub(crate) fn push_param(&mut self, id: ParamId, value: f64, sample_offset: u32) {
        self.push(&Event::Param(ParamEvent::SetValue {
            id,
            target: Target::Global,
            value,
            sample_offset,
        }));
    }

    /// Sort by time, which CLAP requires of `in_events` and does not check.
    ///
    /// A stable sort so two events on the same sample keep the order they were
    /// pushed in — which is what makes "gesture begin, value, gesture end" hold
    /// together.
    pub(crate) fn sort(&mut self) {
        self.events.sort_by_key(RawEvent::time);
    }

    /// The pointer to put in `clap_process`. Re-points `ctx` first, so moving
    /// the buffer between blocks is harmless.
    pub(crate) fn as_raw(&mut self) -> *const clap_input_events {
        self.raw.ctx = (&raw mut *self).cast::<c_void>();
        &raw const self.raw
    }
}

unsafe extern "C" fn input_size(list: *const clap_input_events) -> u32 {
    match unsafe { list_ctx::<InputEvents>((*list).ctx) } {
        Some(events) => events.events.len() as u32,
        None => 0,
    }
}

unsafe extern "C" fn input_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let Some(events) = (unsafe { list_ctx::<InputEvents>((*list).ctx) }) else {
        return std::ptr::null();
    };
    match events.events.get(index as usize) {
        Some(event) => (&raw const *event).cast::<clap_event_header>(),
        None => std::ptr::null(),
    }
}

/// The event list handed to `process` as `out_events`.
pub(crate) struct OutputEvents {
    raw: clap_output_events,
    events: Vec<RawEvent>,
}

impl OutputEvents {
    pub(crate) fn new(capacity: usize) -> OutputEvents {
        OutputEvents {
            raw: clap_output_events {
                ctx: std::ptr::null_mut(),
                try_push: Some(output_try_push),
            },
            events: Vec::with_capacity(capacity),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.events.clear();
    }

    pub(crate) fn as_raw(&mut self) -> *const clap_output_events {
        self.raw.ctx = (&raw mut *self).cast::<c_void>();
        &raw const self.raw
    }

    /// Move everything the plugin emitted into the core's sink.
    pub(crate) fn drain_into(&mut self, sink: &mut EventSink) {
        for raw in &self.events {
            if let Some(event) = unsafe { decode(raw) } {
                sink.push(event);
            }
        }
        self.events.clear();
    }
}

unsafe extern "C" fn output_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    if event.is_null() {
        return false;
    }
    let Some(events) = (unsafe { list_ctx_mut::<OutputEvents>((*list).ctx) }) else {
        return false;
    };
    if events.events.len() == events.events.capacity() {
        // False is the format's "I could not take it", which is the honest
        // answer; growing here would allocate on the audio thread.
        return false;
    }
    let header = unsafe { *event };
    if (header.size as usize) > size_of::<RawEvent>() {
        // An event larger than anything this host knows how to hold. Copying
        // only the part that fits would hand `decode` a truncated struct.
        return false;
    }
    let mut raw = RawEvent::zeroed();
    // Copied by size rather than by type: the header is all that has been
    // validated, and every core event is plain data.
    unsafe {
        std::ptr::copy_nonoverlapping(
            event.cast::<u8>(),
            (&raw mut raw).cast::<u8>(),
            header.size as usize,
        );
    }
    events.events.push(raw);
    true
}

/// # Safety
/// `ctx` must be null or a pointer this crate stored, of type `T`.
unsafe fn list_ctx<'a, T>(ctx: *mut c_void) -> Option<&'a T> {
    (!ctx.is_null()).then(|| unsafe { &*ctx.cast::<T>() })
}

/// # Safety
/// As [`list_ctx`], and no other reference to the value may be live.
unsafe fn list_ctx_mut<'a, T>(ctx: *mut c_void) -> Option<&'a mut T> {
    (!ctx.is_null()).then(|| unsafe { &mut *ctx.cast::<T>() })
}

/// The DAW's transport, as CLAP wants it.
///
/// CLAP measures musical time in fixed point rather than in doubles, so the
/// conversion is where precision is lost — nowhere else in this file.
///
/// `sample_rate` is taken rather than assumed because CLAP wants the song
/// position in seconds as well as in beats, and the core model carries only
/// samples. The VST3 backend takes it for the same reason.
pub(crate) fn to_transport(context: &TimeContext, sample_rate: f64) -> clap_event_transport {
    let beats = |quarters: f64| -> i64 { (quarters * CLAP_BEATTIME_FACTOR as f64) as i64 };
    let secs = |seconds: f64| -> i64 { (seconds * CLAP_SECTIME_FACTOR as f64) as i64 };

    let mut flags = CLAP_TRANSPORT_HAS_TEMPO
        | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
        | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
        | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
    if context.playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if context.recording {
        flags |= CLAP_TRANSPORT_IS_RECORDING;
    }
    // Only when the bounds are known, which is not the same question as
    // whether a loop is running. The `HAS_*_TIMELINE` flags above already
    // promise that `loop_start`/`loop_end` mean something, and CLAP has no
    // second flag to withdraw that promise for the loop fields alone the way
    // VST3's `kCycleValid` does. A loop reported as running but spanning
    // nothing is worse for a plugin than no loop reported at all, so a host
    // that cannot describe the loop says nothing about it.
    let loop_music = context.loop_range_music.unwrap_or((0.0, 0.0));
    let loop_seconds = context.loop_range_seconds.unwrap_or((0.0, 0.0));
    if context.loop_active && context.loop_range_music.is_some() {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }

    let song_pos_seconds = if sample_rate > 0.0 {
        context.project_time_samples as f64 / sample_rate
    } else {
        0.0
    };

    clap_event_transport {
        header: header(size_of::<clap_event_transport>(), 0, 9),
        flags,
        song_pos_beats: beats(context.project_time_music),
        song_pos_seconds: secs(song_pos_seconds),
        tempo: context.tempo_bpm,
        tempo_inc: 0.0,
        loop_start_beats: beats(loop_music.0),
        loop_end_beats: beats(loop_music.1),
        loop_start_seconds: secs(loop_seconds.0),
        loop_end_seconds: secs(loop_seconds.1),
        bar_start: beats(context.bar_position_music),
        // Derived rather than carried: the core model records where the bar
        // starts, and CLAP wants which bar it is. Both are the DAW's answer to
        // the same question.
        bar_number: bar_number(context),
        tsig_num: context.time_sig_numerator.max(1) as u16,
        tsig_denom: context.time_sig_denominator.max(1) as u16,
    }
}

fn bar_number(context: &TimeContext) -> i32 {
    let beats_per_bar = f64::from(context.time_sig_numerator.max(1)) * 4.0
        / f64::from(context.time_sig_denominator.max(1));
    if beats_per_bar <= 0.0 {
        return 0;
    }
    (context.bar_position_music / beats_per_bar).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_song_position_in_seconds_follows_the_sample_count() {
        let context = TimeContext {
            project_time_samples: 48_000 * 3,
            ..TimeContext::default()
        };
        let transport = to_transport(&context, 48_000.0);
        assert_ne!(transport.flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, 0);
        assert_eq!(transport.song_pos_seconds, 3 * CLAP_SECTIME_FACTOR);
    }

    #[test]
    fn a_loop_whose_bounds_are_unknown_is_not_reported_at_all() {
        // CLAP cannot say "looping, but I cannot tell you where", and a plugin
        // handed a loop of length zero is worse off than one told nothing.
        let context = TimeContext {
            loop_active: true,
            ..TimeContext::default()
        };
        let transport = to_transport(&context, 48_000.0);
        assert_eq!(transport.flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE, 0);
        assert_eq!(transport.loop_start_beats, 0);
        assert_eq!(transport.loop_end_beats, 0);
    }

    #[test]
    fn a_loop_with_bounds_carries_both_timelines() {
        let context = TimeContext {
            loop_active: true,
            loop_range_music: Some((4.0, 8.0)),
            loop_range_seconds: Some((2.0, 4.0)),
            ..TimeContext::default()
        };
        let transport = to_transport(&context, 48_000.0);
        assert_ne!(transport.flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE, 0);
        assert_eq!(transport.loop_start_beats, 4 * CLAP_BEATTIME_FACTOR);
        assert_eq!(transport.loop_end_beats, 8 * CLAP_BEATTIME_FACTOR);
        assert_eq!(transport.loop_start_seconds, 2 * CLAP_SECTIME_FACTOR);
        assert_eq!(transport.loop_end_seconds, 4 * CLAP_SECTIME_FACTOR);
    }

    #[test]
    fn a_param_set_round_trips() {
        let event = Event::Param(ParamEvent::SetValue {
            id: ParamId(7),
            target: Target::Global,
            value: 0.25,
            sample_offset: 13,
        });
        let raw = encode(&event).expect("encodable");
        assert_eq!(unsafe { decode(&raw) }, Some(event));
    }

    #[test]
    fn every_note_variant_round_trips() {
        let events = [
            Event::Note(NoteEvent::NoteOn {
                note_id: 3,
                port: 0,
                channel: 1,
                key: 60,
                velocity: 0.75,
                sample_offset: 4,
            }),
            Event::Note(NoteEvent::NoteOff {
                note_id: 3,
                port: 0,
                channel: 1,
                key: 60,
                velocity: 0.0,
                sample_offset: 8,
            }),
            Event::Note(NoteEvent::NoteEnd {
                note_id: 3,
                port: 0,
                channel: 1,
                key: 60,
                sample_offset: 9,
            }),
            Event::Note(NoteEvent::Expression {
                note_id: 3,
                port: 0,
                channel: 1,
                key: 60,
                expression: NoteExpression::Tuning,
                value: -1.5,
                sample_offset: 10,
            }),
            Event::Note(NoteEvent::Midi {
                port: 0,
                data: [0x90, 60, 100],
                sample_offset: 11,
            }),
        ];
        for event in events {
            let raw = encode(&event).expect("encodable");
            assert_eq!(unsafe { decode(&raw) }, Some(event), "{event:?}");
        }
    }

    #[test]
    fn targets_survive_the_trip() {
        // Verify that per-voice and per-channel addressing round-trips accurately.
        for target in [
            Target::Global,
            Target::NoteId(11),
            Target::Key {
                channel: 2,
                key: 64,
            },
            Target::Channel(3),
            Target::Port(1),
        ] {
            let event = Event::Param(ParamEvent::Modulate {
                id: ParamId(1),
                target,
                amount: 0.5,
                sample_offset: 0,
            });
            let raw = encode(&event).expect("encodable");
            assert_eq!(unsafe { decode(&raw) }, Some(event), "{target:?}");
        }
    }

    #[test]
    fn every_event_declares_its_own_size_not_the_unions() {
        // A plugin reads `size` to step through the list; declaring the union's
        // size would make it walk past events it has not seen.
        let event = Event::Param(ParamEvent::GestureBegin { id: ParamId(1) });
        let raw = encode(&event).expect("encodable");
        assert_eq!(
            unsafe { raw.header.size } as usize,
            size_of::<clap_event_param_gesture>()
        );
        assert!(size_of::<clap_event_param_gesture>() < size_of::<RawEvent>());
    }

    #[test]
    fn the_input_list_sorts_by_time() {
        let mut list = InputEvents::new(8);
        for offset in [30u32, 10, 20] {
            list.push(&Event::Param(ParamEvent::SetValue {
                id: ParamId(1),
                target: Target::Global,
                value: 0.0,
                sample_offset: offset,
            }));
        }
        list.sort();
        let times: Vec<u32> = list.events.iter().map(RawEvent::time).collect();
        assert_eq!(times, vec![10, 20, 30]);
    }

    #[test]
    fn a_full_input_list_drops_rather_than_grows() {
        let mut list = InputEvents::new(2);
        let capacity = list.events.capacity();
        for _ in 0..10 {
            list.push_param(ParamId(1), 0.0, 0);
        }
        assert_eq!(list.events.len(), capacity);
        assert_eq!(list.events.capacity(), capacity, "the buffer reallocated");
    }
}
