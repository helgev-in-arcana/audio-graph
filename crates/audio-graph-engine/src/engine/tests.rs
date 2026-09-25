use super::*;
use crate::compile::compile;
use crate::graph::{Graph, NodeId};
use crate::ir::MathOp;
use crate::nodes::{
    AudioIn, AudioOut, CcIn, Constant, DelayRead, DelayWrite, EnvelopeFollower, Gate, KeyParam,
    KeyParamMode, KeySplit, KeySwitch, KeySwitchMode, Lfo, Math, Mix, NodeKind, NoteFilter,
    NoteFollow, NoteGate, NoteMute, ParamPort, ParamToCc, Plugin, PluginPorts, RangeMap, Rate,
    SlotIn, Switch, linear_to_db,
};
use crate::notes::MAX_LIVE_NOTES;
use crate::port::PortType;

const SLOTS: usize = 32;

/// Helper creating a parameter sink plugin node.
fn param_sink(graph: &mut Graph) -> NodeId {
    graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                params: vec![ParamPort {
                    id: 0,
                    name: "p".into(),
                }],
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    )
}

/// The lane [`param_sink`]'s parameter is driven through.
const SINK: usize = SLOTS;

/// A lane row: the slot table, and the sink's lane after it.
fn lanes() -> Vec<f64> {
    vec![0.0; SLOTS + 1]
}

fn ctx(frames: u32) -> BlockContext {
    BlockContext {
        sample_rate: 48_000.0,
        tempo_bpm: 120.0,
        frames,
        offset: 0,
        row: 0,
        block: frames,
    }
}

/// The rate the audio tests run at. Real rather than convenient, because
/// the delay rings are sized in seconds and a fake rate would make a
/// sensible `max_time` come out as four samples.
const RATE: f64 = 48_000.0;

/// Samples, as the seconds a delay node wants.
fn seconds(samples: f64) -> f64 {
    samples / RATE
}

/// A context for a test with no automation in it.
fn audio_ctx(frames: u32) -> AudioContext<'static> {
    AudioContext {
        frames,
        quantum: 32,
        sample_rate: RATE,
        lanes: &[],
        lanes_per_row: 0,
    }
}

fn load(engine: &mut Engine, graph: &Graph) {
    let handoff = Handoff::new();
    let program = compile(graph, SLOTS).unwrap();
    handoff.send(Box::new(PreparedProgram::prepare(program, RATE, &[]).0));
    assert!(engine.adopt_handoff(&handoff));
}

#[test]
fn a_lane_the_graph_does_not_drive_keeps_the_daws_value() {
    let mut graph = Graph::new();
    let c = graph.add(NodeKind::Constant(Constant { value: 0.25 }), [0.0, 0.0]);
    let out = param_sink(&mut graph);
    graph.connect(c, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = vec![0.9; SLOTS + 1];
    engine.run(&ctx(32), &mut slots);
    assert_eq!(slots[SINK], 0.25);
    assert_eq!(slots[1], 0.9, "an undriven slot is left alone");
}

#[test]
fn the_daws_automation_can_be_read_shaped_and_written_back() {
    let mut graph = Graph::new();
    let input = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
    let half = graph.add(
        NodeKind::Math(Math {
            op: MathOp::Multiply,
            b: 0.5,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(input, 0, half, 0);
    graph.connect(half, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    slots[3] = 0.8;
    engine.run(&ctx(32), &mut slots);
    assert!((slots[SINK] - 0.4).abs() < 1e-12);
}

/// The parameter half's switch: one value below the threshold, another at
/// it and above.
#[test]
fn a_switch_picks_by_threshold() {
    let mut graph = Graph::new();
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
    let switch = graph.add(
        NodeKind::Switch(Switch {
            values: vec![0.2, 0.9],
            thresholds: vec![0.6],
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(control, 0, switch, 0);
    graph.connect(switch, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    slots[1] = 0.59;
    engine.run(&ctx(32), &mut slots);
    assert_eq!(slots[SINK], 0.2);

    slots[1] = 0.6;
    engine.run(&ctx(32), &mut slots);
    assert_eq!(slots[SINK], 0.9, "the threshold itself is on");
}

/// More than two rungs: the last threshold the control has passed is the
/// one that wins, and below all of them the first value is what is read.
#[test]
fn a_switch_climbs_a_ladder_of_thresholds() {
    let mut graph = Graph::new();
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
    let switch = graph.add(
        NodeKind::Switch(Switch {
            values: vec![0.1, 0.4, 0.7],
            thresholds: vec![0.3, 0.8],
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(control, 0, switch, 0);
    graph.connect(switch, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    for (control, expected) in [(0.0, 0.1), (0.29, 0.1), (0.3, 0.4), (0.79, 0.4), (0.8, 0.7)] {
        slots[1] = control;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], expected, "at {control}");
    }
}

/// Either side of a switch can be a signal rather than a number, which is
/// what makes it a router as well as a chooser.
#[test]
fn a_switch_can_pick_between_two_signals() {
    let mut graph = Graph::new();
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
    let a = graph.add(NodeKind::SlotIn(SlotIn { slot: 2 }), [0.0, 0.0]);
    let b = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
    let switch = graph.add(
        NodeKind::Switch(Switch {
            values: vec![0.0, 1.0],
            thresholds: vec![0.5],
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(control, 0, switch, 0);
    graph.connect(a, 0, switch, 1);
    graph.connect(b, 0, switch, 2);
    graph.connect(switch, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    slots[2] = 0.25;
    slots[3] = 0.75;
    slots[1] = 0.0;
    engine.run(&ctx(32), &mut slots);
    assert_eq!(slots[SINK], 0.25);

    slots[2] = 0.25;
    slots[3] = 0.75;
    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    assert_eq!(slots[SINK], 0.75);
}

#[test]
fn an_lfo_sweeps_and_comes_back() {
    let mut graph = Graph::new();
    let lfo = graph.add(
        NodeKind::Lfo(Lfo {
            waveform: Waveform::Saw,
            rate: Rate::Hz(1.0),
            phase: 0.0,
            depth: 0.5,
            offset: 0.5,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(lfo, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    let mut seen: Vec<f64> = Vec::new();
    // One second at 48 kHz in 32-sample sub-blocks: a whole cycle.
    for _ in 0..1500 {
        engine.run(&ctx(32), &mut slots);
        seen.push(slots[SINK]);
    }
    let lowest = seen.iter().cloned().fold(f64::INFINITY, f64::min);
    let highest = seen.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(lowest < 0.02, "a saw should reach the bottom, got {lowest}");
    assert!(highest > 0.98, "a saw should reach the top, got {highest}");
    assert!(seen.iter().all(|v| (0.0..=1.0).contains(v)));
}

#[test]
fn tempo_sync_follows_the_host() {
    let mut graph = Graph::new();
    let lfo = graph.add(
        NodeKind::Lfo(Lfo {
            waveform: Waveform::Saw,
            // One cycle per beat: at 120 bpm that is 2 Hz.
            rate: Rate::Beats(1.0),
            phase: 0.0,
            depth: 0.5,
            offset: 0.5,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(lfo, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    // Quarter of a beat at 120 bpm = 0.125 s = 6000 samples.
    let mut slots = lanes();
    engine.run(
        &BlockContext {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            frames: 6000,
            offset: 0,
            row: 0,
            block: 6000,
        },
        &mut slots,
    );
    engine.run(
        &BlockContext {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            frames: 1,
            offset: 0,
            row: 0,
            block: 1,
        },
        &mut slots,
    );
    assert!(
        (slots[SINK] - 0.25).abs() < 1e-3,
        "expected a quarter cycle, got {}",
        slots[SINK]
    );
}

/// Helper creating a parameter feedback loop test graph.
fn feedback_graph(time: f64) -> (Graph, NodeId) {
    let mut graph = Graph::new();
    let seed = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
    let read = graph.add(
        NodeKind::DelayRead(DelayRead {
            line: 0,
            ty: PortType::Param,
            max_time: 1.0,
            time,
        }),
        [0.0, 0.0],
    );
    // The loop: (input + what came back) * 0.5, written back to the line.
    let mixed = graph.add(
        NodeKind::Math(Math {
            op: MathOp::Add,
            b: 0.0,
        }),
        [0.0, 0.0],
    );
    let decayed = graph.add(
        NodeKind::Math(Math {
            op: MathOp::Multiply,
            b: 0.5,
        }),
        [0.0, 0.0],
    );
    let write = graph.add(
        NodeKind::DelayWrite(DelayWrite {
            line: 0,
            ty: PortType::Param,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);

    graph.connect(seed, 0, mixed, 0);
    graph.connect(read, 0, mixed, 1);
    graph.connect(mixed, 0, decayed, 0);
    graph.connect(decayed, 0, write, 0);
    graph.connect(decayed, 0, out, 0);
    (graph, write)
}

#[test]
fn a_delay_line_carries_a_value_round_the_loop() {
    // One sub-block of delay, so each run reads exactly what the previous
    // one wrote.
    let (graph, _) = feedback_graph(32.0 / 48_000.0);
    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    // (1 + 0) * 0.5
    assert!(
        (slots[SINK] - 0.5).abs() < 1e-9,
        "first pass: {}",
        slots[SINK]
    );

    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    // (1 + 0.5) * 0.5 — the 0.5 came back round.
    assert!(
        (slots[SINK] - 0.75).abs() < 1e-9,
        "second pass: {}",
        slots[SINK]
    );
}

/// Verifies that recompiling does not clear parameter delay line state.
#[test]
fn recompiling_does_not_empty_a_delay_line() {
    let (mut graph, _) = feedback_graph(32.0 / 48_000.0);
    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    assert!((slots[SINK] - 0.5).abs() < 1e-9);

    // An unrelated node appears, as it does on any edit.
    graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
    load(&mut engine, &graph);

    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    assert!(
        (slots[SINK] - 0.75).abs() < 1e-9,
        "the line was emptied by the swap: {}",
        slots[SINK]
    );
}

/// Verifies that parameter delay times below one sub-block are clamped to the minimum floor.
#[test]
fn a_delay_shorter_than_a_sub_block_is_held_at_one() {
    let (graph, _) = feedback_graph(0.0);
    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    slots[1] = 1.0;
    engine.run(&ctx(32), &mut slots);
    assert!(
        (slots[SINK] - 0.75).abs() < 1e-9,
        "a zero time should behave as one sub-block, not as zero: {}",
        slots[SINK]
    );
}

/// The DAW's buffer size is not the sub-block size, and the loop is defined
/// in sub-blocks. Two runs of 32 must land where one run of 64 does not.
#[test]
fn the_loop_is_measured_in_sub_blocks_not_daw_blocks() {
    let (graph, _) = feedback_graph(2.0 * 32.0 / 48_000.0);
    let run = |frames: u32, passes: usize| {
        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = lanes();
        for _ in 0..passes {
            slots[1] = 1.0;
            engine.run(&ctx(frames), &mut slots);
        }
        slots[SINK]
    };
    // Same sub-block size, same answer, however the DAW hands us the block.
    assert!((run(32, 4) - run(32, 4)).abs() < 1e-12);
    // Two sub-blocks of delay: nothing has come back yet after two passes.
    assert!((run(32, 2) - 0.5).abs() < 1e-9);
    // By the third, it has.
    assert!(run(32, 3) > 0.5);
}

/// An instrument: one stereo output, and a notes port.
fn note_plugin(graph: &mut Graph, instance: usize) -> NodeId {
    graph.add(
        NodeKind::Plugin(Plugin {
            instance,
            ports: PluginPorts {
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                accepts_notes: true,
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    )
}

/// Records the note stream each instance was handed, so the note half can
/// be read off a run.
#[derive(Default)]
struct Heard(std::collections::BTreeMap<u32, Vec<Event>>);

impl AudioInstances for Heard {
    fn process(
        &mut self,
        instance: u32,
        notes: &[Event],
        _input: &[f32],
        output: &mut [f32],
        chunk: AudioChunk,
        _schedule: ScheduleView<'_>,
    ) {
        self.0.entry(instance).or_default().extend_from_slice(notes);
        for ch in 0..chunk.output_channels {
            output[chunk.channel(ch)].fill(0.0);
        }
    }
}

/// The note id an event carries, for reading a recorded stream back.
fn named(event: &Event) -> Option<i32> {
    match event {
        Event::Note(note) => note.note_id(),
        Event::Param(_) => None,
    }
}

fn note_on(key: i16, at: u32) -> Event {
    Event::Note(NoteEvent::NoteOn {
        note_id: None,
        port: 0,
        channel: 0,
        key,
        velocity: 1.0,
        sample_offset: at,
    })
}

fn note_off(key: i16, at: u32) -> Event {
    Event::Note(NoteEvent::NoteOff {
        note_id: None,
        port: 0,
        channel: 0,
        key,
        velocity: 0.0,
        sample_offset: at,
    })
}

/// Runs `graph` over one block with `events` on the DAW's note input, and
/// returns what each instance heard.
fn hear(graph: &Graph, events: &[Event], lanes: &[f64]) -> Heard {
    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, graph);
    let mut heard = Heard::default();
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut row = vec![0.0; width];
    row[..lanes.len().min(width)].copy_from_slice(&lanes[..lanes.len().min(width)]);
    // The wrapper's order: take the stream in, run the param half over the
    // row the note filters read, then the audio half.
    engine.begin_block(events);
    engine.run(&ctx(8), &mut row);
    engine.run_audio(
        &AudioContext {
            frames: 8,
            quantum: 32,
            sample_rate: RATE,
            lanes: &row,
            lanes_per_row: width,
        },
        &[0.0; 2 * 8],
        &mut [0.0; 2 * 8],
        &mut heard,
    );
    heard
}

/// A synth wired to the MIDI input hears the DAW; the one next to it does
/// not. This is the whole reason notes are routed rather than broadcast.
#[test]
fn only_the_wired_instrument_hears_the_daw() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let wired = note_plugin(&mut graph, 0);
    let idle = note_plugin(&mut graph, 1);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: Vec::new(),
        }),
        [0.0, 0.0],
    );
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, wired, 0);
    graph.connect(wired, 0, mix, 0);
    graph.connect(idle, 0, mix, 2);
    graph.connect(mix, 0, out, 0);

    let heard = hear(&graph, &[note_on(60, 0)], &[]);
    assert_eq!(heard.0[&0].len(), 1);
    assert!(
        heard.0.get(&1).is_none_or(Vec::is_empty),
        "an unwired notes port means silence, not everything"
    );
}

/// A shut gate holds the note-ons back and lets the releases through, so a
/// note that was sounding when it closed still gets its note-off. Blocking
/// everything would leave a hung note behind whatever threw the gate.
#[test]
fn a_shut_note_gate_still_delivers_the_releases() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let gate = graph.add(
        NodeKind::NoteGate(NoteGate {
            threshold: 0.5,
            invert: false,
        }),
        [0.0, 0.0],
    );
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, gate, 0);
    graph.connect(control, 0, gate, 1);
    graph.connect(gate, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let events = [note_on(60, 0), note_off(55, 1)];

    // Slot 0 is the gate's control, and it is read straight out of the
    // lane row the DAW's automation rides in.
    let open = hear(&graph, &events, &[1.0]);
    assert_eq!(open.0[&0].len(), 2, "open, everything passes");

    let shut = hear(&graph, &events, &[0.0]);
    let seen = &shut.0[&0];
    assert_eq!(seen.len(), 1, "only the release got through");
    assert!(matches!(
        seen[0],
        Event::Note(NoteEvent::NoteOff { key: 55, .. })
    ));
}

/// A key mute drops both halves of the keys it names — the note-on went
/// too, so nothing is left waiting for a release — and leaves everything
/// else, including events that have no key at all.
#[test]
fn a_key_mute_takes_both_halves_and_leaves_the_controllers() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let mute = graph.add(NodeKind::NoteMute(NoteMute { keys: vec![24] }), [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, mute, 0);
    graph.connect(mute, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let sustain = Event::Note(NoteEvent::Cc {
        port: 0,
        channel: 0,
        cc: 64,
        value: 1.0,
        sample_offset: 2,
    });
    let heard = hear(
        &graph,
        &[note_on(24, 0), note_off(24, 1), sustain, note_on(60, 3)],
        &[],
    );
    let seen = &heard.0[&0];
    assert_eq!(seen.len(), 2, "both halves of key 24 are gone: {seen:?}");
    assert!(matches!(seen[0], Event::Note(NoteEvent::Cc { .. })));
    assert!(matches!(
        seen[1],
        Event::Note(NoteEvent::NoteOn { key: 60, .. })
    ));
}

/// A key split hands each band to its own instrument, and hands the pedal
/// to both of them. Dividing the keys is the whole job; an event that has
/// no key belongs to no band and so belongs to all of them.
#[test]
fn a_key_split_divides_the_keys_and_shares_what_has_none() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let split = graph.add(
        NodeKind::KeySplit(KeySplit { splits: vec![60] }),
        [0.0, 0.0],
    );
    let upper = note_plugin(&mut graph, 0);
    let lower = note_plugin(&mut graph, 1);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: Vec::new(),
        }),
        [0.0, 0.0],
    );
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, split, 0);
    graph.connect(split, 0, upper, 0);
    graph.connect(split, 1, lower, 0);
    graph.connect(upper, 0, mix, 0);
    graph.connect(lower, 0, mix, 2);
    graph.connect(mix, 0, out, 0);

    let sustain = Event::Note(NoteEvent::Cc {
        port: 0,
        channel: 0,
        cc: 64,
        value: 1.0,
        sample_offset: 3,
    });
    // 60 is the split, and it belongs to the band it names — the lower one.
    let heard = hear(
        &graph,
        &[note_on(72, 0), note_on(60, 1), note_on(48, 2), sustain],
        &[],
    );

    let keys = |instance: u32| -> Vec<i16> {
        heard.0[&instance]
            .iter()
            .filter_map(|event| match event {
                Event::Note(NoteEvent::NoteOn { key, .. }) => Some(*key),
                _ => None,
            })
            .collect()
    };
    assert_eq!(keys(0), vec![72], "the upper band takes 61 and above");
    assert_eq!(keys(1), vec![60, 48], "the lower band takes 60 and below");
    for instance in [0, 1] {
        assert!(
            heard.0[&instance]
                .iter()
                .any(|event| matches!(event, Event::Note(NoteEvent::Cc { cc: 64, .. }))),
            "the pedal reaches instance {instance}"
        );
    }
}

/// A MIDI filter narrows the stream by channel and by controller number,
/// and judges each event only on what it actually has: a note has a
/// channel but no controller number, so a CC list must not swallow it.
#[test]
fn a_midi_filter_narrows_by_channel_and_controller() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let filter = graph.add(
        NodeKind::NoteFilter(NoteFilter {
            channels: vec![0],
            channel_mode: crate::nodes::FilterMode::Keep,
            controllers: vec![64],
            controller_mode: crate::nodes::FilterMode::Keep,
        }),
        [0.0, 0.0],
    );
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, filter, 0);
    graph.connect(filter, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let cc = |number: u8, channel: i16, at: u32| {
        Event::Note(NoteEvent::Cc {
            port: 0,
            channel,
            cc: number,
            value: 1.0,
            sample_offset: at,
        })
    };
    let mut on_other_channel = note_on(60, 4);
    if let Event::Note(NoteEvent::NoteOn { channel, .. }) = &mut on_other_channel {
        *channel = 1;
    }

    let heard = hear(
        &graph,
        &[
            note_on(60, 0),   // channel 0: passes
            on_other_channel, // channel 1: dropped
            cc(64, 0, 1),     // the sustain pedal: passes
            cc(1, 0, 2),      // the mod wheel: dropped
            cc(64, 1, 3),     // right controller, wrong channel
        ],
        &[],
    );
    let seen = &heard.0[&0];
    assert_eq!(seen.len(), 2, "expected the note and the pedal: {seen:?}");
    assert!(matches!(
        seen[0],
        Event::Note(NoteEvent::NoteOn {
            key: 60,
            channel: 0,
            ..
        })
    ));
    assert!(matches!(seen[1], Event::Note(NoteEvent::Cc { cc: 64, .. })));
}

/// A parameter driving a controller: the value reaches the plugin as CC,
/// and joins the stream that was already flowing rather than replacing it.
#[test]
fn a_parameter_reaches_the_plugin_as_a_controller() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let pedal = graph.add(
        NodeKind::ParamToCc(ParamToCc { channel: 0, cc: 64 }),
        [0.0, 0.0],
    );
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(control, 0, pedal, 0);
    graph.connect(notes, 0, pedal, 1);
    graph.connect(pedal, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let heard = hear(&graph, &[note_on(60, 2)], &[1.0]);
    let seen = &heard.0[&0];
    assert_eq!(seen.len(), 2, "the pedal and the note: {seen:?}");
    assert!(
        matches!(
            seen[0],
            Event::Note(NoteEvent::Cc { cc: 64, value, sample_offset: 0, .. }) if value == 1.0
        ),
        "the controller comes first, at the sub-block start, so the \
         buffer stays sorted: {seen:?}"
    );
    assert!(matches!(
        seen[1],
        Event::Note(NoteEvent::NoteOn { key: 60, .. })
    ));
}

/// An unchanged controller is not an event. Re-sending it every sub-block
/// would fill a plugin's parameter queue with nothing and retrigger the
/// smoothing on plugins that ramp towards each incoming point.
#[test]
fn a_controller_is_sent_once_until_it_moves() {
    let mut graph = Graph::new();
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let pedal = graph.add(
        NodeKind::ParamToCc(ParamToCc { channel: 0, cc: 64 }),
        [0.0, 0.0],
    );
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(control, 0, pedal, 0);
    graph.connect(pedal, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);

    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut heard = Heard::default();
    let block = |engine: &mut Engine, value: f64, heard: &mut Heard| {
        let mut row = vec![0.0; width];
        row[0] = value;
        engine.begin_block(&[]);
        engine.run(&ctx(8), &mut row);
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 32,
                sample_rate: RATE,
                lanes: &row,
                lanes_per_row: width,
            },
            &[0.0; 2 * 8],
            &mut [0.0; 2 * 8],
            heard,
        );
    };

    block(&mut engine, 1.0, &mut heard);
    assert_eq!(heard.0[&0].len(), 1, "the first value is always news");
    block(&mut engine, 1.0, &mut heard);
    assert_eq!(heard.0[&0].len(), 1, "and holding it is not");
    block(&mut engine, 0.0, &mut heard);
    assert_eq!(heard.0[&0].len(), 2, "letting go is");

    load(&mut engine, &graph);
    block(&mut engine, 0.0, &mut heard);
    assert_eq!(heard.0[&0].len(), 3, "adoption resends the current value");
    block(&mut engine, 0.0, &mut heard);
    assert_eq!(heard.0[&0].len(), 3);
    let NodeKind::ParamToCc(emitter) = &mut graph.node_mut(pedal).unwrap().kind else {
        unreachable!()
    };
    emitter.cc = 1;
    load(&mut engine, &graph);
    block(&mut engine, 0.0, &mut heard);
    assert!(matches!(
        heard.0[&0].last(),
        Some(Event::Note(NoteEvent::Cc { cc: 1, .. }))
    ));
    assert_eq!(heard.0[&0].len(), 4);
    graph.remove(pedal);
    load(&mut engine, &graph);
    block(&mut engine, 0.0, &mut heard);
    assert_eq!(
        heard.0[&0].len(),
        4,
        "a removed emitter cannot replay its old buffer"
    );
    let replacement = graph.add(NodeKind::ParamToCc(ParamToCc::default()), [0.0; 2]);
    graph.connect(control, 0, replacement, 0);
    graph.connect(replacement, 0, synth, 0);
    load(&mut engine, &graph);
    block(&mut engine, 0.0, &mut heard);
    assert_eq!(
        heard.0[&0].len(),
        5,
        "a reused emitter slot must send its first value"
    );
}

/// Events, boundary indices, and held state move together without replacing buffer storage.
#[test]
fn note_buffer_handoff_preserves_storage_and_the_previous_block_tail() {
    use crate::ir::NoteStreamKind;
    let stream = |node| NoteStream {
        node,
        port: 0,
        source: None,
        kind: NoteStreamKind::Input(0),
    };
    let program = |streams: Vec<NoteStream>| {
        let mut program = Program::empty();
        program.note_bufs = streams.len() as u16;
        program.note_streams = streams;
        program
    };
    let publisher = crate::ProgramPublisher::default();
    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    publisher.publish(program(vec![stream(11), stream(22)]), RATE);
    engine.adopt(&publisher);
    engine.notes.bufs[0]
        .events
        .extend([note_on(60, 1), note_on(61, 40)]);
    engine.notes.bufs[1]
        .events
        .extend([note_on(70, 1), note_on(71, 2), note_on(72, 50)]);
    engine.notes.bufs[0].held = 1 << 61;
    engine.notes.bufs[0].struck = 1 << 61;
    engine.notes.bufs[0].count = 2;
    engine.notes.bufs[0].velocity = 0.75;
    engine.notes.bufs[0].key = 61.0 / 127.0;
    engine.notes.bufs[1].count = 3;
    engine.note_rows = 2;
    engine.note_marks[1][0] = 1;
    engine.note_marks[1][1] = 2;
    let storage = |engine: &Engine| {
        let mut storage: [_; MAX_NOTE_BUFS] = std::array::from_fn(|i| {
            (
                engine.notes.bufs[i].events.as_ptr() as usize,
                engine.notes.bufs[i].events.capacity(),
            )
        });
        storage.sort_unstable();
        storage
    };
    let before = storage(&engine);
    publisher.publish(program(vec![stream(22), stream(11), stream(33)]), RATE);
    assert!(engine.adopt(&publisher));
    assert_eq!(storage(&engine), before);
    assert_eq!(engine.note_marks[1][..3], [2, 1, 0]);
    let carried = &engine.notes.bufs[1];
    assert_eq!(
        (carried.held, carried.struck, carried.count),
        (1 << 61, 1 << 61, 2)
    );
    assert_eq!((carried.velocity, carried.key), (0.75, 61.0 / 127.0));
    assert_eq!(engine.notes.bufs[2].count, 0);
    assert!(engine.notes.bufs[2].events.is_empty());
    engine.begin_block(&[]);
    assert_eq!(engine.notes.bufs[0].events, [note_on(72, 50)]);
    assert_eq!(engine.notes.bufs[1].events, [note_on(61, 40)]);
    assert_eq!(engine.notes.bufs[0].count, 3);
    publisher.publish(program(vec![stream(44), stream(11)]), RATE);
    engine.adopt(&publisher);
    assert_eq!(storage(&engine), before);
    assert_eq!(engine.notes.bufs[0].count, 0);
    assert_eq!(engine.notes.bufs[0].key, 0.5);
    assert!(engine.notes.bufs[0].events.is_empty());
    assert_eq!(engine.notes.bufs[1].count, 2);
}

/// A generator follows its parameter at sub-block resolution even when the
/// plugins are called once for the whole block.
///
/// Those are separate questions and must not be conflated: reading one
/// row's lane values and applying them to the whole chunk would make a
/// `Param → CC` send one value per block however fast the parameter moved.
/// The sub-plugin call rate is a cost decision; the resolution of what it
/// is told is not.
#[test]
fn a_generator_follows_its_lane_inside_a_whole_block_chunk() {
    let mut graph = Graph::new();
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let pedal = graph.add(
        NodeKind::ParamToCc(ParamToCc { channel: 0, cc: 64 }),
        [0.0, 0.0],
    );
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(control, 0, pedal, 0);
    graph.connect(pedal, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    load(&mut engine, &graph);
    assert_eq!(
        engine.chunking(),
        Chunking::WholeBlock,
        "no feedback loop, so the plugin is called once for the block"
    );

    // Four sub-blocks of 16, with the control taking a different value in
    // each. One whole-block chunk covers all four.
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let values = [0.0, 0.25, 0.25, 1.0];
    let mut rows = vec![0.0; width * values.len()];
    for (index, &value) in values.iter().enumerate() {
        let row = &mut rows[index * width..(index + 1) * width];
        row[0] = value;
        engine.run(
            &BlockContext {
                sample_rate: RATE,
                tempo_bpm: 120.0,
                frames: 16,
                offset: index as u32 * 16,
                row: index as u32,
                block: 64,
            },
            row,
        );
    }

    let mut heard = Heard::default();
    engine.run_audio(
        &AudioContext {
            frames: 64,
            quantum: 16,
            sample_rate: RATE,
            lanes: &rows,
            lanes_per_row: width,
        },
        &[0.0; 2 * 64],
        &mut [0.0; 2 * 64],
        &mut heard,
    );

    let seen = &heard.0[&0];
    let sent: Vec<(u32, f64)> = seen
        .iter()
        .filter_map(|event| match *event {
            Event::Note(NoteEvent::Cc {
                value,
                sample_offset,
                ..
            }) => Some((sample_offset, value)),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        vec![(0, 0.0), (16, 0.25), (48, 1.0)],
        "one event per move, timed at the sub-block it happened in, and \
         nothing for the sub-block where it held: {seen:?}"
    );
}

/// A sub-plugin is handed the graph's own note id, not the DAW's — and
/// gets one even when the DAW supplied none, which is the normal case for
/// anything that arrived as raw MIDI.
#[test]
fn a_sub_plugin_is_handed_the_graphs_own_note_id() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    // Two overlapping notes on one key, neither carrying an id.
    // Substituting the key number for a missing one would give both the
    // same id and make them one note.
    let heard = hear(&graph, &[note_on(60, 0), note_on(60, 1)], &[]);
    let ids: Vec<Option<i32>> = heard.0[&0].iter().map(named).collect();
    assert!(
        ids.iter().all(Option::is_some),
        "every note is named: {ids:?}"
    );
    assert_ne!(ids[0], ids[1], "and the two are told apart");
}

/// The note-off names the note the note-on opened, however little the DAW
/// said about it.
#[test]
fn a_note_off_reaches_the_plugin_naming_the_note_it_ends() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let heard = hear(&graph, &[note_on(60, 0), note_off(60, 1)], &[]);
    let seen = &heard.0[&0];
    assert_eq!(seen.len(), 2);
    assert_eq!(named(&seen[0]), named(&seen[1]));
}

/// Every branch gated shut: the note reaches nobody, and the DAW is still
/// holding a voice for it. Telling it so at the end of the block is the
/// honest answer, and the only one that lets the voice go.
#[test]
fn a_note_that_reaches_no_plugin_is_reported_ended() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let gate = graph.add(
        NodeKind::NoteGate(NoteGate {
            threshold: 0.5,
            invert: false,
        }),
        [0.0, 0.0],
    );
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, gate, 0);
    graph.connect(control, 0, gate, 1);
    graph.connect(gate, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);

    // Shut, so the note-on is swallowed before it reaches the synth.
    let mut heard = Heard::default();
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut row = vec![0.0; width];
    engine.begin_block(&[note_on(60, 0)]);
    engine.run(&ctx(8), &mut row);
    engine.run_audio(
        &AudioContext {
            frames: 8,
            quantum: 32,
            sample_rate: RATE,
            lanes: &row,
            lanes_per_row: width,
        },
        &[0.0; 2 * 8],
        &mut [0.0; 2 * 8],
        &mut heard,
    );
    assert!(
        heard.0.get(&0).is_none_or(Vec::is_empty),
        "the gate held it back"
    );

    let mut ended = Vec::with_capacity(8);
    engine.end_block(&[], &mut ended);
    assert_eq!(ended.len(), 1, "and the DAW is told the note is over");
    assert_eq!(ended[0].key, 60);
}

/// A note that did reach a plugin waits for the plugin to say it is done.
#[test]
fn a_delivered_note_is_reported_only_when_the_plugin_ends_it() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);

    let mut heard = Heard::default();
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let row = vec![0.0; width];
    engine.begin_block(&[note_on(60, 0)]);
    engine.run(&ctx(8), &mut vec![0.0; width]);
    engine.run_audio(
        &AudioContext {
            frames: 8,
            quantum: 32,
            sample_rate: RATE,
            lanes: &row,
            lanes_per_row: width,
        },
        &[0.0; 2 * 8],
        &mut [0.0; 2 * 8],
        &mut heard,
    );
    let id = named(&heard.0[&0][0]).expect("the synth was told a name");

    let mut ended = Vec::with_capacity(8);
    engine.end_block(&[], &mut ended);
    assert!(ended.is_empty(), "the synth is still playing it");

    // What a CLAP sub-plugin sends when its voice finishes.
    engine.end_block(
        &[Event::Note(NoteEvent::NoteEnd {
            note_id: Some(id),
            port: 0,
            channel: 0,
            key: 60,
            sample_offset: 0,
        })],
        &mut ended,
    );
    assert_eq!(ended.len(), 1, "and now it is not");
}

/// Fallback releases only its own delivery; a native-completion branch still has to finish.
#[test]
fn completion_policy_can_differ_between_instances() {
    struct Mixed {
        heard: Heard,
        native: bool,
    }
    impl AudioInstances for Mixed {
        fn reports_note_end(&self, instance: u32, _: i16) -> bool {
            self.native && instance == 1
        }
        fn process(
            &mut self,
            instance: u32,
            events: &[Event],
            input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
            schedule: ScheduleView<'_>,
        ) {
            self.heard
                .process(instance, events, input, output, chunk, schedule);
        }
    }
    for (native, shut) in [(false, false), (true, false), (true, true)] {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
        let first = note_plugin(&mut graph, 0);
        let second = note_plugin(&mut graph, 1);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0; 2],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0; 2],
        );
        if shut {
            let control = graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0; 2]);
            let gate = graph.add(
                NodeKind::NoteGate(NoteGate {
                    threshold: 0.5,
                    invert: false,
                }),
                [0.0; 2],
            );
            graph.connect(notes, 0, gate, 0);
            graph.connect(control, 0, gate, 1);
            graph.connect(gate, 0, first, 0);
        } else {
            graph.connect(notes, 0, first, 0);
        }
        graph.connect(notes, 0, second, 0);
        graph.connect(first, 0, mix, 0);
        graph.connect(second, 0, mix, 2);
        graph.connect(mix, 0, out, 0);
        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut row = vec![0.0; width];
        let mut nodes = Mixed {
            heard: Heard::default(),
            native,
        };
        for event in [note_on(60, 0), note_off(60, 5)] {
            engine.begin_block(&[event]);
            engine.run(&ctx(8), &mut row);
            engine.run_audio(
                &AudioContext {
                    frames: 8,
                    quantum: 8,
                    sample_rate: RATE,
                    lanes: &row,
                    lanes_per_row: width,
                },
                &[0.0; 16],
                &mut [0.0; 16],
                &mut nodes,
            );
        }
        let id = named(&nodes.heard.0[&1][0]).unwrap();
        let mut ended = Vec::with_capacity(8);
        engine.end_block(
            &[Event::Note(NoteEvent::NoteOff {
                note_id: Some(id),
                port: 0,
                channel: 0,
                key: 60,
                velocity: 0.0,
                sample_offset: 0,
            })],
            &mut ended,
        );
        assert_eq!(
            ended.len(),
            usize::from(!native),
            "native={native}, shut={shut}"
        );
        if native {
            engine.end_block(
                &[Event::Note(NoteEvent::NoteEnd {
                    note_id: Some(id),
                    port: 0,
                    channel: 0,
                    key: 60,
                    sample_offset: 0,
                })],
                &mut ended,
            );
            assert_eq!(ended.len(), 1);
        }
    }
}

/// Each sub-block gets its own events, once. Handing every chunk the whole
/// block would replay every note once per chunk.
#[test]
fn a_note_lands_in_one_sub_block_only() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);
    let mut heard = Heard::default();
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    // A quantum of 4 cuts the block in two whatever the graph says, so the
    // same note must not appear in both halves.
    let mut row = vec![0.0; width * 2];
    engine.begin_block(&[note_on(60, 0), note_on(61, 5)]);
    for sub in 0..2 {
        engine.run(
            &BlockContext {
                sample_rate: RATE,
                tempo_bpm: 120.0,
                frames: 4,
                offset: sub as u32 * 4,
                row: sub as u32,
                block: 8,
            },
            &mut row[sub * width..(sub + 1) * width],
        );
    }
    engine.run_audio(
        &AudioContext {
            frames: 8,
            quantum: 4,
            sample_rate: RATE,
            lanes: &row,
            lanes_per_row: width,
        },
        &[0.0; 2 * 8],
        &mut [0.0; 2 * 8],
        &mut heard,
    );
    assert_eq!(heard.0[&0].len(), 2, "each note once: {:?}", heard.0[&0]);
}

/// Later parameter stages do not duplicate note input or orphan notes already delivered to an instrument.
#[test]
fn notes_are_ingested_once_across_parameter_and_audio_stages() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, synth, 0);

    let follower = graph.add(
        NodeKind::EnvelopeFollower(EnvelopeFollower {
            detect: Detect::Peak,
            attack: 0.0,
            release: 0.0,
        }),
        [0.0, 0.0],
    );
    let sink = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 1,
            ports: PluginPorts {
                params: vec![ParamPort {
                    id: 0,
                    name: "level".into(),
                }],
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    );
    graph.connect(synth, 0, follower, 0);
    graph.connect(follower, 0, sink, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    load(&mut engine, &graph);
    assert!(
        engine.stages() > 1,
        "the follower waits for audio from the instrument"
    );
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut schedule = SlotSchedule::new(width, 64, 32).unwrap();
    let mut heard = Heard::default();
    let mut daw_out = [0.0; 2 * 64];
    engine.run_block(
        &mut schedule,
        &[],
        &[note_on(60, 0), note_on(61, 37)],
        64,
        32,
        RATE,
        120.0,
        &[0.0; 2 * 64],
        &mut daw_out,
        &mut heard,
    );
    assert_eq!(schedule.blocks(), 2);
    assert_eq!(heard.0[&0].len(), 2, "each note once: {:?}", heard.0[&0]);
    let mut ended = Vec::with_capacity(8);
    engine.end_block(&[], &mut ended);
    assert!(
        ended.is_empty(),
        "both notes belong to the instrument and remain live"
    );

    heard.0.clear();
    engine.run_block(
        &mut schedule,
        &[],
        &[],
        64,
        32,
        RATE,
        120.0,
        &[0.0; 2 * 64],
        &mut daw_out,
        &mut heard,
    );
    assert!(
        heard.0[&0].is_empty(),
        "the next block does not replay either note"
    );
}

/// The last sub-block of a block is still readable at the start of the
/// next one, and is not played twice.
///
/// The buffers carry that tail across the block boundary so a parameter op
/// reading at the first boundary of a block has the stream that was in
/// force there — without it every controller would snap back to its
/// starting value once per DAW block. The plugins were handed those events
/// last block and must not be handed them again, which is the half of the
/// arrangement nothing else would notice.
#[test]
fn the_boundary_a_block_starts_on_belongs_to_the_block_before() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let synth = note_plugin(&mut graph, 0);
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);

    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut heard = Heard::default();
    let block = |engine: &mut Engine, events: &[Event], heard: &mut Heard| {
        let mut row = vec![0.0; width];
        engine.begin_block(events);
        engine.run(&ctx(8), &mut row);
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 32,
                sample_rate: RATE,
                lanes: &row,
                lanes_per_row: width,
            },
            &[0.0; 2 * 8],
            &mut [0.0; 2 * 8],
            heard,
        );
    };

    block(&mut engine, &[note_on(60, 0)], &mut heard);
    assert_eq!(heard.0[&0].len(), 1, "the synth is played the note");
    block(&mut engine, &[], &mut heard);
    assert_eq!(
        heard.0[&0].len(),
        1,
        "and not played it again: {:?}",
        heard.0[&0]
    );
}

/// A `CC In` reading the DAW's mod wheel into a parameter lane, one
/// sub-block behind the events — which is what a parameter signal's
/// resolution means, not a delay anybody chose.
#[test]
fn a_controller_becomes_a_parameter_at_the_next_boundary() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let wheel = graph.add(NodeKind::CcIn(CcIn::default()), [0.0, 0.0]);
    let sink = param_sink(&mut graph);
    graph.connect(notes, 0, wheel, 0);
    graph.connect(wheel, 0, sink, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    load(&mut engine, &graph);

    let wheel_at = |value: f64, at: u32| {
        Event::Note(NoteEvent::Cc {
            port: 0,
            channel: 0,
            cc: 1,
            value,
            sample_offset: at,
        })
    };
    // Two sub-blocks of 8 frames each, with the wheel moving inside the
    // first one.
    let events = [wheel_at(1.0, 3)];
    let mut rows = vec![0.0; (SLOTS + 1) * 2];
    let width = SLOTS + 1;

    engine.begin_block(&events);
    for index in 0..2u32 {
        let (head, tail) = rows.split_at_mut(index as usize * width);
        let row = if index == 0 {
            &mut head[..]
        } else {
            &mut tail[..width]
        };
        engine.run(
            &BlockContext {
                sample_rate: RATE,
                tempo_bpm: 120.0,
                frames: 8,
                offset: index * 8,
                row: index,
                block: 16,
            },
            row,
        );
    }

    assert_eq!(
        rows[SINK], 0.0,
        "the first sub-block carries the value in effect at its start"
    );
    assert_eq!(
        rows[width + SINK],
        1.0,
        "the second carries where the wheel ended up in the first"
    );
}

/// The stream a `CC In` reads is the routed one, so a filter upstream of it
/// changes what it sees. That is the whole reason it takes a note input
/// instead of reading whatever the DAW sent.
#[test]
fn a_filter_upstream_of_a_cc_in_changes_what_it_reads() {
    let build = |keep: u8| {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let filter = graph.add(
            NodeKind::NoteFilter(NoteFilter {
                controllers: vec![keep],
                controller_mode: crate::nodes::FilterMode::Keep,
                ..NoteFilter::default()
            }),
            [0.0, 0.0],
        );
        let wheel = graph.add(NodeKind::CcIn(CcIn::default()), [0.0, 0.0]);
        let sink = param_sink(&mut graph);
        graph.connect(notes, 0, filter, 0);
        graph.connect(filter, 0, wheel, 0);
        graph.connect(wheel, 0, sink, 0);
        graph
    };

    let events = [Event::Note(NoteEvent::Cc {
        port: 0,
        channel: 0,
        cc: 1,
        value: 1.0,
        sample_offset: 0,
    })];
    let read = |graph: &Graph| {
        let mut engine = Engine::new();
        engine.prepare(64, &[]);
        load(&mut engine, graph);
        let mut row = vec![0.0; SLOTS + 1];
        engine.begin_block(&events);
        // Twice: the first fills the buffers, the second reads them.
        for index in 0..2u32 {
            engine.run(
                &BlockContext {
                    sample_rate: RATE,
                    tempo_bpm: 120.0,
                    frames: 8,
                    offset: index * 8,
                    row: index,
                    block: 16,
                },
                &mut row,
            );
        }
        row[SINK]
    };

    assert_eq!(read(&build(1)), 1.0, "CC1 is kept, so the wheel is read");
    assert_eq!(
        read(&build(64)),
        0.0,
        "keeping only CC64 takes the wheel out before it gets here"
    );
}

/// A controller keeps its position between messages. A block with no CC in
/// it must hold the last value, not snap back to the starting one.
#[test]
fn a_controller_holds_its_position() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let wheel = graph.add(NodeKind::CcIn(CcIn::default()), [0.0, 0.0]);
    let sink = param_sink(&mut graph);
    graph.connect(notes, 0, wheel, 0);
    graph.connect(wheel, 0, sink, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    load(&mut engine, &graph);

    let moved = [Event::Note(NoteEvent::Cc {
        port: 0,
        channel: 0,
        cc: 1,
        value: 0.75,
        sample_offset: 0,
    })];
    let mut row = vec![0.0; SLOTS + 1];
    let block = |engine: &mut Engine, events: &[Event], row: &mut Vec<f64>| {
        engine.begin_block(events);
        engine.run(
            &BlockContext {
                sample_rate: RATE,
                tempo_bpm: 120.0,
                frames: 8,
                offset: 0,
                row: 0,
                block: 8,
            },
            row,
        );
    };

    block(&mut engine, &moved, &mut row);
    block(&mut engine, &[], &mut row);
    assert_eq!(row[SINK], 0.75, "the wheel moved and the value took");
    block(&mut engine, &[], &mut row);
    assert_eq!(row[SINK], 0.75, "and stayed where it was left");
}

/// A stand-in for the sub-plugins, so the engine's routing can be tested
/// without one. Each instance adds its own number to every sample, which
/// makes the order it ran in readable off the output.
struct Adders;

impl AudioInstances for Adders {
    fn process(
        &mut self,
        instance: u32,
        _notes: &[Event],
        input: &[f32],
        output: &mut [f32],
        chunk: AudioChunk,
        _schedule: ScheduleView<'_>,
    ) {
        for ch in 0..chunk.output_channels {
            let range = chunk.channel(ch);
            for (o, i) in output[range.clone()].iter_mut().zip(input[range].iter()) {
                *o = *i + (instance + 1) as f32;
            }
        }
    }
}

fn audio_plugin(graph: &mut Graph, instance: usize, latency: u32) -> NodeId {
    graph.add(
        NodeKind::Plugin(Plugin {
            instance,
            ports: PluginPorts {
                audio_in: vec![2],
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                latency,
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    )
}

#[test]
fn audio_runs_through_two_plugins_in_order() {
    let mut graph = Graph::new();
    let input = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    let first = audio_plugin(&mut graph, 0, 0);
    let second = audio_plugin(&mut graph, 1, 0);
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, first, 0);
    graph.connect(first, 0, second, 0);
    graph.connect(second, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);

    let daw_in = vec![10.0f32; 2 * 8];
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);

    // 10, then +1 from instance 0, then +2 from instance 1.
    assert!(
        daw_out.iter().all(|&s| (s - 13.0).abs() < 1e-6),
        "{daw_out:?}"
    );
}

/// Verifies latency compensation delay aligns parallel audio branches.
#[test]
fn a_compensated_branch_arrives_with_the_late_one() {
    let mut graph = Graph::new();
    let input = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    // Latency 4, but the stand-in does not actually delay: what is being
    // tested is that the *other* branch is delayed by the same 4.
    let slow = audio_plugin(&mut graph, 0, 4);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            // Empty is unity: what a mix did before it had gains.
            gains: Vec::new(),
        }),
        [0.0, 0.0],
    );
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, slow, 0);
    graph.connect(slow, 0, mix, 0);
    graph.connect(input, 0, mix, 2);
    graph.connect(mix, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);

    // An impulse on the first sample of each channel.
    let mut daw_in = vec![0.0f32; 2 * 8];
    daw_in[0] = 1.0;
    daw_in[8] = 1.0;
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);

    // The wet branch is the stand-in: input + 1, so the impulse shows as
    // 2.0 at sample 0 and 1.0 everywhere else. The dry branch is held back
    // 4 samples, so its impulse lands at sample 4 and nowhere else.
    assert!((daw_out[0] - 2.0).abs() < 1e-6, "wet at 0: {}", daw_out[0]);
    assert!(
        (daw_out[4] - 2.0).abs() < 1e-6,
        "dry arrives at 4: {}",
        daw_out[4]
    );
    assert!(
        (daw_out[1] - 1.0).abs() < 1e-6,
        "quiet between: {}",
        daw_out[1]
    );
}

/// The patch a new instance starts with has to be the through-connection
/// the wrapper used to make for itself, or every fresh instance is silent.
#[test]
fn the_default_patch_passes_audio_through() {
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &Graph::default_patch());

    let daw_in: Vec<f32> = (0..2 * 8).map(|i| i as f32 * 0.1).collect();
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    assert_eq!(daw_out, daw_in);
}

/// An unconnected `AudioOut` emits no `Output` op, so `run_audio` clears
/// the block itself.
#[test]
fn an_unconnected_output_clears_the_daw_buffer() {
    let mut graph = Graph::new();
    graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);

    let daw_in = vec![0.0f32; 2 * 8];
    let mut daw_out = vec![7.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    assert!(daw_out.iter().all(|&s| s == 0.0), "{daw_out:?}");
}

/// A channel no op reaches is silence too.
#[test]
fn a_channel_no_op_writes_is_cleared() {
    let mut graph = Graph::new();
    let input = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 1,
        }),
        [0.0, 0.0],
    );
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 1,
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, output, 0);
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);

    let daw_in = vec![1.0f32; 2 * 8];
    let mut daw_out = vec![7.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    assert!(daw_out[8..].iter().all(|&s| s == 0.0), "{daw_out:?}");
}

/// A line whose writer loses its input keeps its ring across the swap, so
/// the write head has to go on advancing for the line to drain.
#[test]
fn a_delay_line_nothing_writes_drains_to_silence() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    // A tap well inside a block, so a handful of blocks fills the ring.
    let (write, read) = audio_delay(&mut graph, 100.0);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    load(&mut engine, &graph);

    // Load the line.
    let daw_in = vec![1.0f32; 2 * 128];
    let mut daw_out = vec![0.0f32; 2 * 128];
    for _ in 0..8 {
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
    }
    assert!(
        daw_out.iter().any(|s| s.abs() > 0.5),
        "the line is loaded: {:?}",
        &daw_out[..8]
    );

    // Pull the wire out of the write node and let the line run dry.
    graph.disconnect(write, 0);
    load(&mut engine, &graph);
    let silence = vec![0.0f32; 2 * 128];
    for _ in 0..32 {
        engine.run_audio(&audio_ctx(128), &silence, &mut daw_out, &mut Adders);
    }
    assert!(
        daw_out.iter().all(|s| s.abs() < 1e-6),
        "the ring should have drained: {:?}",
        &daw_out[..8]
    );
}

/// `prepare` is the only thing that allocates, so running without it — or
/// with a longer block than promised — has to be a no-op rather than a
/// panic or a read past the end.
#[test]
fn running_audio_unprepared_does_nothing() {
    let mut graph = Graph::new();
    let input = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, output, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let daw_in = vec![1.0f32; 2 * 8];
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    assert!(daw_out.iter().all(|&s| s == 0.0));

    engine.prepare(4, &[2]);
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    assert!(
        daw_out.iter().all(|&s| s == 0.0),
        "8 frames were promised 4"
    );
}

#[test]
fn recompiling_does_not_restart_a_running_lfo() {
    let mut graph = Graph::new();
    let lfo = graph.add(
        NodeKind::Lfo(Lfo {
            waveform: Waveform::Saw,
            rate: Rate::Hz(1.0),
            phase: 0.0,
            depth: 0.5,
            offset: 0.5,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(lfo, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);

    let mut slots = lanes();
    for _ in 0..200 {
        engine.run(&ctx(32), &mut slots);
    }
    let before = slots[SINK];
    assert!(before > 0.05, "the LFO should have moved by now");

    // Something unrelated changes — a new node appears — and the graph is
    // recompiled, as it is on every edit.
    graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
    load(&mut engine, &graph);
    engine.run(&ctx(1), &mut slots);

    assert!(
        (slots[SINK] - before).abs() < 0.01,
        "the phase jumped across a recompile: {before} -> {}",
        slots[SINK]
    );
}

/// Plays notes into the graph's MIDI input the way the wrapper does.
///
/// Notes reach a parameter op one sub-block after they arrive — the note
/// half fills the buffers at the end of each sub-block's evaluation, so a
/// reader sees the stream in effect at the boundary it just crossed. That
/// is one sub-block of setup in every test that plays something, which
/// says nothing about what the test is checking, so it lives here.
#[derive(Default)]
struct Keyboard {
    pending: Vec<Event>,
}

impl Keyboard {
    fn note(&mut self, event: &NoteEvent) {
        self.pending.push(Event::Note(*event));
    }

    /// Evaluate over the two sub-blocks it takes for what has been played
    /// to reach the parameter half.
    fn run(&mut self, engine: &mut Engine, frames: u32, slots: &mut [f64]) {
        engine.begin_block(&self.pending);
        for (row, offset) in [0, frames].into_iter().enumerate() {
            engine.run(
                &BlockContext {
                    sample_rate: 48_000.0,
                    tempo_bpm: 120.0,
                    frames,
                    offset,
                    row: row as u32,
                    block: frames * 2,
                },
                slots,
            );
        }
        self.pending.clear();
    }
}

/// A key switch watches one key, whatever has been played since — which
/// is exactly what a follow of the newest note cannot answer.
#[test]
fn a_held_key_switch_follows_its_own_key() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let switch = graph.add(
        NodeKind::KeySwitch(KeySwitch {
            keys: vec![24],
            mode: KeySwitchMode::Hold,
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let synth = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                accepts_notes: true,
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    );
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, switch, 0);
    graph.connect(switch, 0, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();

    let program = compile(&graph, SLOTS).unwrap();
    let lane = program
        .note_ops
        .iter()
        .find_map(|op| match op {
            crate::ir::NoteOp::Filter { gate, .. } => *gate,
            _ => None,
        })
        .expect("the key switch booked a gate lane") as usize;

    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut lanes = vec![0.0; width];
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 0.0, "nothing is held yet");

    keys.note(&NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 24,
        velocity: 1.0,
        sample_offset: 0,
    });
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "the switch key is down");

    // A different key coming and going must not move it.
    keys.note(&NoteEvent::NoteOn {
        note_id: Some(2),
        port: 0,
        channel: 0,
        key: 60,
        velocity: 1.0,
        sample_offset: 0,
    });
    keys.note(&NoteEvent::NoteOff {
        note_id: Some(2),
        port: 0,
        channel: 0,
        key: 60,
        velocity: 0.0,
        sample_offset: 0,
    });
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "another key came and went");

    keys.note(&NoteEvent::NoteOff {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 24,
        velocity: 0.0,
        sample_offset: 0,
    });
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 0.0, "let go");
}

/// A toggling switch moves on each strike and stays where it was put —
/// including across the recompile that every edit causes.
#[test]
fn a_toggling_key_switch_latches_and_survives_a_recompile() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let switch = graph.add(
        NodeKind::KeySwitch(KeySwitch {
            keys: vec![24, 25],
            mode: KeySwitchMode::Toggle,
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let synth = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                accepts_notes: true,
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    );
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, switch, 0);
    graph.connect(switch, 1, synth, 0);
    graph.connect(synth, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();

    let lane = compile(&graph, SLOTS)
        .unwrap()
        .note_ops
        .iter()
        .find_map(|op| match op {
            crate::ir::NoteOp::Filter { gate, .. } => *gate,
            _ => None,
        })
        .expect("output b got a gate lane") as usize;

    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut lanes = vec![0.0; width];
    let strike = NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 24,
        velocity: 1.0,
        sample_offset: 0,
    };

    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 0.0, "b is shut until the switch is thrown");

    keys.note(&strike);
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "thrown");
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "and it stays thrown");

    // An unrelated edit, and the recompile it causes.
    graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "a recompile must not move the switch");

    keys.note(&strike);
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 0.0, "thrown back");
}

/// One key stepping a parameter through its values, and staying where it
/// was put. With two values that is a plain toggle.
#[test]
fn a_key_parameter_toggles_between_two_values() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let key = graph.add(
        NodeKind::KeyParam(KeyParam {
            mode: KeyParamMode::Toggle,
            keys: vec![24, 25],
            values: vec![0.2, 0.8],
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(notes, 0, key, 0);
    graph.connect(key, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();
    let strike = NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 24,
        velocity: 1.0,
        sample_offset: 0,
    };

    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.2, "untouched, it reads its first value");

    keys.note(&strike);
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.8);
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.8, "one strike is one step, not one per run");

    keys.note(&strike);
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.2, "and round again");
}

/// A bank of keys, one value each: the last one struck wins, which is what
/// a row of switches does.
#[test]
fn a_key_parameter_selects_by_the_last_key_struck() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let key = graph.add(
        NodeKind::KeyParam(KeyParam {
            mode: KeyParamMode::Select,
            keys: vec![24, 25, 26],
            values: vec![0.25, 0.5, 1.0],
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(notes, 0, key, 0);
    graph.connect(key, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();
    let strike = |key: i16| NoteEvent::NoteOn {
        note_id: Some(key as i32),
        port: 0,
        channel: 0,
        key,
        velocity: 1.0,
        sample_offset: 0,
    };

    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.25);

    keys.note(&strike(25));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.5);

    keys.note(&strike(26));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 1.0);

    // A key the bank does not name changes nothing.
    keys.note(&strike(60));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 1.0);
}

/// Nothing wired to the notes port means no keys are read at all. An
/// unwired node that quietly followed the keyboard anyway would be a node
/// whose links say nothing about what it does.
#[test]
fn a_key_parameter_with_no_notes_wired_stays_put() {
    let mut graph = Graph::new();
    let key = graph.add(
        NodeKind::KeyParam(KeyParam {
            mode: KeyParamMode::Select,
            keys: vec![24, 25],
            values: vec![0.25, 0.75],
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(key, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();

    keys.note(&NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 25,
        velocity: 1.0,
        sample_offset: 0,
    });
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.25);
}

/// A value socket wins over the number on its row, the same way `Math`'s
/// `b` gives way to its input — so a key switch can pick between two
/// signals, not only two numbers.
#[test]
fn a_key_parameter_value_can_be_a_signal() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let signal = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
    let key = graph.add(
        NodeKind::KeyParam(KeyParam {
            mode: KeyParamMode::Select,
            keys: vec![24, 25],
            values: vec![0.25, 0.75],
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(notes, 0, key, 0);
    // Socket 0 is the notes port, so value 2 is socket 2.
    graph.connect(signal, 0, key, 2);
    graph.connect(key, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();
    slots[3] = 0.6;
    keys.note(&NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 25,
        velocity: 1.0,
        sample_offset: 0,
    });
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.6, "the wired socket wins over the number");
}

#[test]
fn the_gate_follows_held_notes() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let gate = graph.add(
        NodeKind::NoteFollow(NoteFollow { what: Follow::Gate }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(notes, 0, gate, 0);
    graph.connect(gate, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();

    let on = |key: i16| NoteEvent::NoteOn {
        note_id: Some(key as i32),
        port: 0,
        channel: 0,
        key,
        velocity: 1.0,
        sample_offset: 0,
    };
    let off = |key: i16| NoteEvent::NoteOff {
        note_id: Some(key as i32),
        port: 0,
        channel: 0,
        key,
        velocity: 0.0,
        sample_offset: 0,
    };

    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.0);

    keys.note(&on(60));
    keys.note(&on(64));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 1.0);

    // Releasing one of two held notes must not drop the gate.
    keys.note(&off(60));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 1.0);

    keys.note(&off(64));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(slots[SINK], 0.0);
}

/// Held Keys counts keys under a hand, not note-ons on a wire: the same
/// key on two channels is one key, and one release lifts it. Both fall out
/// of reading the mask of what is down rather than a running total — see
/// [`Follow::HeldKeys`] for why the mask is the one to read.
///
/// Read through a map of 0..8, because a count is not a fraction and a
/// parameter lane is normalized on its way out of the engine.
#[test]
fn held_keys_counts_the_keys_that_are_down() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let count = graph.add(
        NodeKind::NoteFollow(NoteFollow {
            what: Follow::HeldKeys,
        }),
        [0.0, 0.0],
    );
    let map = graph.add(
        NodeKind::RangeMap(RangeMap {
            in_lo: 0.0,
            in_hi: 8.0,
            out_lo: 0.0,
            out_hi: 1.0,
            clamp: true,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(notes, 0, count, 0);
    graph.connect(count, 0, map, 0);
    graph.connect(map, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();

    let on = |channel: i16, key: i16| NoteEvent::NoteOn {
        note_id: None,
        port: 0,
        channel,
        key,
        velocity: 1.0,
        sample_offset: 0,
    };
    let off = |channel: i16, key: i16| NoteEvent::NoteOff {
        note_id: None,
        port: 0,
        channel,
        key,
        velocity: 0.0,
        sample_offset: 0,
    };
    // Eighths, so every reading below is exact in binary.
    let down = |slots: &[f64]| slots[SINK] * 8.0;

    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 0.0, "nothing played, nothing down");

    keys.note(&on(0, 60));
    keys.note(&on(0, 64));
    keys.note(&on(0, 67));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 3.0, "a triad is three keys");

    keys.note(&off(0, 64));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 2.0);

    // The same key again on another channel, which is one hand playing one
    // key however many streams it arrives on.
    keys.note(&on(1, 60));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 2.0, "channel is not part of a key");

    keys.note(&off(0, 60));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 1.0, "and one release lifts it");

    keys.note(&off(0, 67));
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 0.0);
}

/// A reset leaves the graph believing nothing is being played.
///
/// The whole point of the button: a patch rewired while a chord was down
/// keeps counting keys whose note-offs went somewhere the note-ons never
/// did, and nothing short of this convinces it otherwise.
#[test]
fn a_reset_takes_the_notes_the_graph_thought_were_down() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let count = graph.add(
        NodeKind::NoteFollow(NoteFollow {
            what: Follow::HeldKeys,
        }),
        [0.0, 0.0],
    );
    let map = graph.add(
        NodeKind::RangeMap(RangeMap {
            in_lo: 0.0,
            in_hi: 8.0,
            out_lo: 0.0,
            out_hi: 1.0,
            clamp: true,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(notes, 0, count, 0);
    graph.connect(count, 0, map, 0);
    graph.connect(map, 0, out, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let mut slots = lanes();
    let down = |slots: &[f64]| slots[SINK] * 8.0;

    for key in [60, 64, 67] {
        keys.note(&NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: 0,
        });
    }
    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 3.0, "a triad is three keys");

    // Sized the way the wrapper sizes it, because `end_all` fills rather
    // than grows: the audio thread may not allocate.
    let mut ended = Vec::with_capacity(MAX_LIVE_NOTES);
    engine.reset_everything(&mut ended);

    let mut sounding: Vec<i16> = ended.iter().map(|note| note.key).collect();
    sounding.sort_unstable();
    assert_eq!(
        sounding,
        [60, 64, 67],
        "the DAW is told about every note it is still holding a voice for"
    );

    keys.run(&mut engine, 32, &mut slots);
    assert_eq!(down(&slots), 0.0, "and nothing is down afterwards");
}

/// A latch is a setting, so only the button takes it.
///
/// A DAW seeks constantly and sends All Notes Off freely; a key switch
/// thrown by hand surviving both is what makes it usable at all. The
/// editor's Reset is the one thing the user asked for by name, so it is
/// the one thing that moves it.
#[test]
fn only_an_explicit_reset_throws_a_latched_key_switch_back() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let switch = graph.add(
        NodeKind::KeySwitch(KeySwitch {
            keys: vec![24, 25],
            mode: KeySwitchMode::Toggle,
            mute_keys: true,
        }),
        [0.0, 0.0],
    );
    let synth = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                accepts_notes: true,
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    );
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    graph.connect(notes, 0, switch, 0);
    graph.connect(switch, 1, synth, 0);
    graph.connect(synth, 0, out, 0);

    let lane = compile(&graph, SLOTS)
        .unwrap()
        .note_ops
        .iter()
        .find_map(|op| match op {
            crate::ir::NoteOp::Filter { gate, .. } => *gate,
            _ => None,
        })
        .expect("output b got a gate lane") as usize;

    let mut engine = Engine::new();
    engine.prepare(8, &[]);
    load(&mut engine, &graph);
    let mut keys = Keyboard::default();
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut lanes = vec![0.0; width];

    keys.note(&NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 24,
        velocity: 1.0,
        sample_offset: 0,
    });
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "thrown");

    let mut ended = Vec::with_capacity(MAX_LIVE_NOTES);
    engine.reset();
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "a transport jump must not move it");

    engine.reset_notes(&mut ended);
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 1.0, "nor an All Notes Off on the wire");

    engine.reset_everything(&mut ended);
    keys.run(&mut engine, 8, &mut lanes);
    assert_eq!(lanes[lane], 0.0, "the button does");
}

/// A reset empties a delay line rather than only rewinding it.
///
/// Silence written into the line from the moment of the reset sweeps the
/// ring at the rate the reads consume it, so most of the old contents are
/// gone before anything looks at them. The part in front of the write head
/// is not: for the first delay time after the head goes back to the start,
/// every read points behind it, into a stretch the sweep has not reached.
/// That is the tail carrying on through a reset asked for to stop it.
#[test]
fn a_reset_empties_an_audio_delay_line() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let (write, read) = audio_delay(&mut graph, 64.0);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    load(&mut engine, &graph);

    // Long enough for the write head to have been all the way round: a
    // line whose far end is still the silence it was allocated with would
    // pass this whether or not the reset emptied anything.
    let loud = vec![1.0f32; 2 * 128];
    let quiet = vec![0.0f32; 2 * 128];
    let mut daw_out = vec![0.0f32; 2 * 128];
    for _ in 0..(2 * (RATE * 0.05) as usize / 128) {
        engine.run_audio(&audio_ctx(128), &loud, &mut daw_out, &mut Adders);
    }
    assert!(daw_out[0] > 0.5, "the line is full before the reset");

    let mut ended = Vec::with_capacity(MAX_LIVE_NOTES);
    engine.reset_everything(&mut ended);

    engine.run_audio(&audio_ctx(128), &quiet, &mut daw_out, &mut Adders);
    let peak = daw_out.iter().fold(0.0f32, |a, b| a.max(b.abs()));
    assert!(peak < 1e-6, "the line came back with {peak} still in it");
}

#[test]
fn a_degenerate_graph_cannot_hand_a_nan_to_the_sub_plugin() {
    let mut graph = Graph::new();
    let a = graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
    let div = graph.add(
        NodeKind::Math(Math {
            op: MathOp::Curve,
            b: 0.0,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut graph);
    graph.connect(a, 0, div, 0);
    graph.connect(div, 0, out, 0);

    let mut engine = Engine::new();
    load(&mut engine, &graph);
    let mut slots = vec![0.5; SLOTS + 1];
    engine.run(&ctx(32), &mut slots);
    assert!(slots[SINK].is_finite());
    assert!((0.0..=1.0).contains(&slots[SINK]));
}

#[test]
fn an_engine_with_no_program_leaves_everything_alone() {
    let mut engine = Engine::new();
    let mut slots = vec![0.3; SLOTS];
    engine.run(&ctx(32), &mut slots);
    assert!(slots.iter().all(|&v| v == 0.3));
    assert!(!engine.has_program());
}

/// The standard block entry point clears output even before a program is adopted.
#[test]
fn run_block_silences_an_empty_engine() {
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    let mut schedule = SlotSchedule::new(1, 64, 32).unwrap();
    let mut output = vec![3.0f32; 16];
    let mut nodes = subhost_adapter::NoInstances;
    assert!(!engine.run_block(
        &mut schedule,
        &[],
        &[],
        8,
        32,
        RATE,
        120.0,
        &[0.0; 16],
        &mut output,
        &mut nodes,
    ));
    assert!(output.iter().all(|&sample| sample == 0.0));
}

/// A parameter read off audio, which only a program cut into stages can
/// express.
///
/// And read without latency: the follower's stage runs after the stage
/// that made the sound, and that stage covered the whole block, so the
/// window read for a sub-block is that sub-block's own. The first row
/// already carries the level of the first row's audio.
#[test]
fn a_parameter_is_read_off_audio_in_the_sub_block_it_belongs_to() {
    const BLOCK: u32 = 64;
    const QUANTUM: u32 = 32;
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;

    let level = |detect: Detect, attack: f64, quiet_first: bool| -> [f64; 2] {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let follower = graph.add(
            NodeKind::EnvelopeFollower(EnvelopeFollower {
                detect,
                attack,
                release: 0.0,
            }),
            [0.0, 0.0],
        );
        let sink = param_sink(&mut graph);
        graph.connect(input, 0, follower, 0);
        graph.connect(follower, 0, sink, 0);

        let mut engine = Engine::new();
        engine.prepare(BLOCK, &[2]);
        load(&mut engine, &graph);

        // Half scale, on the second sub-block only when asked: that is
        // what tells a reading of *this* row from a reading of the last.
        let mut daw_in = vec![0.5f32; 2 * BLOCK as usize];
        if quiet_first {
            for ch in 0..2usize {
                let at = ch * BLOCK as usize;
                daw_in[at..at + QUANTUM as usize].fill(0.0);
            }
        }
        let mut daw_out = vec![0.0f32; 2 * BLOCK as usize];
        let mut schedule = SlotSchedule::new(width, BLOCK, QUANTUM).unwrap();
        let mut nodes = Adders;
        engine.run_block(
            &mut schedule,
            &[],
            &[],
            BLOCK,
            QUANTUM,
            RATE,
            120.0,
            &daw_in,
            &mut daw_out,
            &mut nodes,
        );
        [schedule.block(0)[SINK], schedule.block(1)[SINK]]
    };

    let steady = level(Detect::Peak, 0.0, false);
    assert!(
        (steady[0] - 0.5).abs() < 1e-6 && (steady[1] - 0.5).abs() < 1e-6,
        "the peak of a half-scale signal is a half: {steady:?}"
    );

    // The one that would fail if the reading were a sub-block behind.
    let late = level(Detect::Peak, 0.0, true);
    assert!(
        late[0] < 1e-6,
        "the first sub-block was silent, so its level is nothing: {late:?}"
    );
    assert!(
        (late[1] - 0.5).abs() < 1e-6,
        "and the second is read in the sub-block it belongs to: {late:?}"
    );

    // RMS of a constant is that constant; what separates them is a
    // transient, which the peak catches and the mean does not.
    let mean = level(Detect::Rms, 0.0, true);
    assert!(
        (mean[1] - 0.5).abs() < 1e-6,
        "the RMS of a steady half is a half: {mean:?}"
    );

    // An attack time holds the rise back, and never past the level.
    let slow = level(Detect::Peak, 0.050, false);
    assert!(
        slow[0] > 0.0 && slow[0] < 0.5 && slow[1] > slow[0] && slow[1] < 0.5,
        "an attack of 50 ms climbs towards the level over sub-blocks: {slow:?}"
    );
}

/// What the all-stages helpers cost, which is nothing until a level comes
/// back round to audio.
///
/// `run` + `run_audio` put every parameter of the block before any of its
/// audio, which is the order every caller with no audio to interleave
/// wants. The claim under test is where that parts company with the order
/// the stages describe: not wherever a follower appears, but only where
/// its value reaches audio again inside the same block.
#[test]
fn the_all_stages_helpers_differ_only_where_a_level_reaches_audio() {
    const BLOCK: u32 = 64;
    const QUANTUM: u32 = 32;
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;

    // A ramp, so every block is louder than the one before it and a block
    // of latency cannot hide in a steady signal.
    let played = |staged: bool, ducked: bool| -> Vec<f32> {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let follower = graph.add(
            NodeKind::EnvelopeFollower(EnvelopeFollower {
                detect: Detect::Peak,
                attack: 0.0,
                release: 0.0,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, follower, 0);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 1,
                gains: vec![0.0],
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(mix, 0, output, 0);
        if ducked {
            // The level drives the gain, so it reaches audio.
            graph.connect(follower, 0, mix, 1);
        } else {
            // The level goes to a sub-plugin parameter on an instance the
            // program has no audio for: read by the editor, and by nothing
            // this block renders.
            let sink = param_sink(&mut graph);
            graph.connect(follower, 0, sink, 0);
        }

        let mut engine = Engine::new();
        engine.prepare(BLOCK, &[2]);
        load(&mut engine, &graph);

        let mut heard = Vec::new();
        let mut lanes = vec![0.0; width * 2];
        for block in 0..4u32 {
            let value = 0.1 * (block + 1) as f32;
            let daw_in = vec![value; 2 * BLOCK as usize];
            let mut daw_out = vec![0.0f32; 2 * BLOCK as usize];
            let context = |row: usize| BlockContext {
                sample_rate: RATE,
                tempo_bpm: 120.0,
                frames: QUANTUM,
                offset: row as u32 * QUANTUM,
                row: row as u32,
                block: BLOCK,
            };
            let audio = AudioContext {
                frames: BLOCK,
                quantum: QUANTUM,
                sample_rate: RATE,
                lanes: &[],
                lanes_per_row: width,
            };
            if staged {
                let mut schedule = SlotSchedule::new(width, BLOCK, QUANTUM).unwrap();
                engine.run_block(
                    &mut schedule,
                    &[],
                    &[],
                    BLOCK,
                    QUANTUM,
                    RATE,
                    120.0,
                    &daw_in,
                    &mut daw_out,
                    &mut Adders,
                );
                lanes.copy_from_slice(schedule.rows());
            } else {
                engine.begin_block(&[]);
                for row in 0..2usize {
                    engine.run(&context(row), &mut lanes[row * width..(row + 1) * width]);
                }
                engine.run_audio(
                    &AudioContext {
                        lanes: &lanes,
                        ..audio
                    },
                    &daw_in,
                    &mut daw_out,
                    &mut Adders,
                );
            }
            // The gain that came out, not the sample: what lags is the
            // level read, and the signal it is applied to is this block's
            // either way, so the two are not shifted copies of each other.
            heard.push(daw_out[BLOCK as usize - 1] / value);
        }
        heard
    };

    // The level reaches nothing this block renders, so the two orders make
    // the same sound and only the lane is a block old.
    assert_eq!(
        played(true, false),
        played(false, false),
        "a level nothing plays costs nothing to read late"
    );

    // And where it does reach audio, the batched order renders each block
    // against the level of the one before it.
    let staged = played(true, true);
    let batched = played(false, true);
    assert_ne!(staged, batched, "a gain read late is a different sound");
    assert!(
        batched
            .iter()
            .skip(1)
            .zip(&staged)
            .all(|(late, then)| (late - then).abs() < 1e-6),
        "and late by exactly one block: {batched:?} against {staged:?}"
    );
}

/// A second delay somewhere else in the patch does not change what the
/// first one does.
///
/// Both loops run in the same stage, and a `DelayWrite` gives its source
/// buffer up as soon as it is *compiled*, while what it writes runs at the
/// end of the stage. Anything compiled in between can be handed that
/// buffer and fill it with something else, and the line then carries the
/// other loop's signal instead of its own.
///
/// Three things have to line up for it: the write has to be the buffer's
/// last reader — a tap on the
/// delayed side rather than on the sum, which is an ordinary way to wire a
/// delay; the other loop has to be compiled after it; and it has to hold a
/// node that asks the pool for a buffer rather than accumulating into one
/// it already has, which a mix does and a plugin does not.
#[test]
fn a_second_delay_does_not_reach_into_the_first() {
    // in ──> mix in 1 ──> write(0)       the sum goes only to the line
    //        read(0) ─┬─> mix in 2
    //                 └─> out            and the tap is on the delayed side
    let patch = |second: bool| -> Vec<f32> {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 32.0);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: vec![0.0, linear_to_db(0.5)],
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 2);
        graph.connect(mix, 0, write, 0);
        graph.connect(read, 0, output, 0);

        if second {
            // A whole second loop, wired to nothing that is heard. It
            // exists to be compiled after the first one's write and to ask
            // the pool for a buffer while doing it.
            let other = graph.add(
                NodeKind::DelayWrite(DelayWrite {
                    line: 1,
                    ty: PortType::STEREO,
                }),
                [0.0, 0.0],
            );
            let tap = graph.add(
                NodeKind::DelayRead(DelayRead {
                    line: 1,
                    ty: PortType::STEREO,
                    max_time: 0.05,
                    time: seconds(64.0),
                }),
                [0.0, 0.0],
            );
            let plugin = audio_plugin(&mut graph, 0, 0);
            let sum = graph.add(
                NodeKind::Mix(Mix {
                    channels: 2,
                    inputs: 2,
                    gains: vec![0.0, linear_to_db(0.5)],
                }),
                [0.0, 0.0],
            );
            graph.connect(input, 0, sum, 0);
            graph.connect(tap, 0, plugin, 0);
            graph.connect(plugin, 0, sum, 2);
            graph.connect(sum, 0, other, 0);
        }

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);
        let mut heard = Vec::new();
        for block in 0..3 {
            let daw_in = if block == 0 {
                impulse(128, 0)
            } else {
                vec![0.0; 2 * 128]
            };
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
            heard.extend_from_slice(&daw_out[..128]);
        }
        heard
    };

    let alone = patch(false);
    assert!(
        alone.iter().filter(|v| v.abs() > 0.05).count() >= 3,
        "the delay repeats on its own"
    );
    assert_eq!(alone, patch(true), "and repeats the same beside another");
}

fn stereo_in(graph: &mut Graph) -> NodeId {
    graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    )
}

fn stereo_out(graph: &mut Graph) -> NodeId {
    graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    )
}

/// The two halves of an audio delay line, on line 0, `samples` back.
fn audio_delay(graph: &mut Graph, samples: f64) -> (NodeId, NodeId) {
    let write = graph.add(
        NodeKind::DelayWrite(DelayWrite {
            line: 0,
            ty: PortType::STEREO,
        }),
        [0.0, 0.0],
    );
    let read = graph.add(
        NodeKind::DelayRead(DelayRead {
            line: 0,
            ty: PortType::STEREO,
            // Room for the sweeps, without asking for a megabyte of ring.
            max_time: 0.05,
            time: seconds(samples),
        }),
        [0.0, 0.0],
    );
    (write, read)
}

/// An impulse on both channels of a stereo block.
fn impulse(frames: usize, at: usize) -> Vec<f32> {
    let mut daw_in = vec![0.0f32; 2 * frames];
    daw_in[at] = 1.0;
    daw_in[frames + at] = 1.0;
    daw_in
}

/// Verifies that an audio delay line outputs samples delayed by the expected duration.
#[test]
fn an_audio_delay_returns_what_it_was_given_a_delay_later() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let (write, read) = audio_delay(&mut graph, 64.0);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    load(&mut engine, &graph);

    // One impulse in the first block, then silence.
    let mut heard = Vec::new();
    for block in 0..3 {
        let daw_in = if block == 0 {
            impulse(128, 8)
        } else {
            vec![0.0; 2 * 128]
        };
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
        heard.extend_from_slice(&daw_out[..128]);
    }

    let peak = heard
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
        .unwrap();
    assert_eq!(peak.0, 8 + 64, "the impulse comes back 64 samples later");
    assert!((peak.1 - 1.0).abs() < 1e-3, "and at its original height");
}

/// Verifies that delay times shorter than chunk length are clamped to minimum safe distance.
#[test]
fn a_delay_shorter_than_a_chunk_is_held_at_the_chunk() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let (write, read) = audio_delay(&mut graph, 1.0);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    load(&mut engine, &graph);

    let mut heard = Vec::new();
    for block in 0..2 {
        let daw_in = if block == 0 {
            impulse(128, 0)
        } else {
            vec![0.0; 2 * 128]
        };
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
        heard.extend_from_slice(&daw_out[..128]);
    }
    let peak = heard
        .iter()
        .position(|v| v.abs() > 0.5)
        .expect("the impulse came back");
    // The quantum is 32, plus the two samples the interpolator needs ahead
    // of the read pointer. Asked for 1: a delay of 1 would have read this
    // chunk's own writes.
    assert_eq!(peak, 34);
}

/// Verifies consistent delay feedback behavior across different host block sizes.
#[test]
fn a_feedback_loop_sounds_the_same_at_any_block_size() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let (write, read) = audio_delay(&mut graph, 64.0);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            // Empty is unity: what a mix did before it had gains.
            gains: Vec::new(),
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, mix, 0);
    graph.connect(read, 0, mix, 2);
    graph.connect(mix, 0, output, 0);
    graph.connect(mix, 0, write, 0);

    let render = |block: usize| -> Vec<f32> {
        let mut engine = Engine::new();
        engine.prepare(512, &[2]);
        load(&mut engine, &graph);
        let mut heard = Vec::new();
        let mut at = 0;
        while at < 512 {
            let mut daw_in = vec![0.0f32; 2 * block];
            if at == 0 {
                daw_in[0] = 1.0;
                daw_in[block] = 1.0;
            }
            let mut daw_out = vec![0.0f32; 2 * block];
            engine.run_audio(&audio_ctx(block as u32), &daw_in, &mut daw_out, &mut Adders);
            heard.extend_from_slice(&daw_out[..block]);
            at += block;
        }
        heard
    };

    let big = render(512);
    let small = render(64);
    assert_eq!(big.len(), small.len());
    let worst = big
        .iter()
        .zip(&small)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-6, "largest difference {worst}");
    assert!(
        big.iter().filter(|v| v.abs() > 0.5).count() >= 4,
        "the loop repeated"
    );
}

/// A plugin called for a sub-block is handed that sub-block, with its
/// channels where the format says they are.
///
/// The pool packs channels at the DAW block's length so that a buffer
/// written whole can be read a piece at a time; a plugin wants them packed
/// at the length of the call. The two agree only when the chunk is the
/// whole block, which is every program without a feedback loop in it — so
/// the gather and scatter that bridge them are reached exactly when
/// nothing else in the suite looks, and a mistake there would put the
/// right samples on the wrong channel.
#[test]
fn a_plugin_called_for_a_sub_block_is_handed_that_sub_block() {
    /// Records every input region it is given, channel by channel.
    #[derive(Default)]
    struct Records {
        heard: Vec<Vec<f32>>,
    }
    impl AudioInstances for Records {
        fn process(
            &mut self,
            _instance: u32,
            _notes: &[Event],
            input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
            _schedule: ScheduleView<'_>,
        ) {
            self.heard.resize(chunk.input_channels as usize, Vec::new());
            for ch in 0..chunk.input_channels {
                self.heard[ch as usize].extend_from_slice(&input[chunk.channel(ch)]);
            }
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
        }
    }

    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let plugin = audio_plugin(&mut graph, 0, 0);
    // The plugin sits *inside* the loop, which is the only way it runs at
    // sub-block granularity. The delay is longer than the block, so within
    // the one block played nothing has come back round yet and what
    // reaches the plugin is the DAW's own input — anything summed into it
    // would hide a packing mistake behind arithmetic.
    let (write, read) = audio_delay(&mut graph, 4096.0);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: Vec::new(),
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, mix, 0);
    graph.connect(read, 0, mix, 2);
    graph.connect(mix, 0, plugin, 0);
    graph.connect(plugin, 0, write, 0);
    graph.connect(plugin, 0, output, 0);

    const BLOCK: usize = 128;
    let mut engine = Engine::new();
    engine.prepare(BLOCK as u32, &[2]);
    load(&mut engine, &graph);
    assert_eq!(
        engine.chunking(),
        Chunking::SubBlock,
        "the loop is what makes the chunks short"
    );

    // Two channels a mixup could not confuse for one another.
    let mut daw_in = vec![0.0f32; 2 * BLOCK];
    for i in 0..BLOCK {
        daw_in[i] = i as f32;
        daw_in[BLOCK + i] = -(i as f32);
    }
    let mut records = Records::default();
    engine.run_audio(
        &audio_ctx(BLOCK as u32),
        &daw_in,
        &mut vec![0.0; 2 * BLOCK],
        &mut records,
    );

    assert_eq!(records.heard.len(), 2, "a stereo plugin heard two channels");
    assert_eq!(
        records.heard[0],
        daw_in[..BLOCK],
        "the chunks join back into the block that was played"
    );
    assert_eq!(records.heard[1], daw_in[BLOCK..], "and so does the other");
}

/// How often a plugin is called is settled by the shape of the patch, not
/// by a number somebody is turning.
///
/// The plugin here feeds a delay line and nothing brings its output back
/// round, so it is not in a loop and is called once for the block —
/// whatever the delay time is set to. A plugin that *is* in a loop is
/// covered by `a_plugin_called_for_a_sub_block_is_handed_that_sub_block`.
#[test]
fn moving_the_delay_time_does_not_change_how_often_a_plugin_runs() {
    struct Counting(usize);
    impl AudioInstances for Counting {
        fn process(
            &mut self,
            _instance: u32,
            _notes: &[Event],
            _input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
            _schedule: ScheduleView<'_>,
        ) {
            self.0 += 1;
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
        }
    }

    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let plugin = audio_plugin(&mut graph, 0, 0);
    let (write, read) = audio_delay(&mut graph, 64.0);
    let time = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    graph.connect(input, 0, plugin, 0);
    graph.connect(plugin, 0, write, 0);
    graph.connect(read, 0, output, 0);
    graph.connect(time, 0, read, 0);

    let program = compile(&graph, SLOTS).unwrap();
    let lane = program
        .audio_ops
        .iter()
        .find_map(|op| match op {
            AudioOp::DelayRead { lane, .. } => *lane,
            _ => None,
        })
        .expect("the wired time control got a lane") as usize;

    let run = |seconds: f64| -> usize {
        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);
        let lanes_per_row = lane + 1;
        let lanes = vec![seconds; lanes_per_row * 4];
        let mut counting = Counting(0);
        engine.run_audio(
            &AudioContext {
                frames: 128,
                quantum: 32,
                sample_rate: RATE,
                lanes: &lanes,
                lanes_per_row,
            },
            &vec![0.0; 2 * 128],
            &mut vec![0.0; 2 * 128],
            &mut counting,
        );
        counting.0
    };
    assert_eq!(run(seconds(64.0)), run(seconds(400.0)));
    assert_eq!(
        run(seconds(64.0)),
        1,
        "one call for the block: the line carries nothing back to it"
    );
}

/// Verifies continuous tape-style pitch modulation during delay time sweeps without clicks.
///
/// The signal written into the line is a ramp of one per sample, so what
/// comes out is `t - d(t)` and the step between output samples is the
/// playback speed. Holding the time still gives a step of exactly 1; a
/// time shortening by a quarter of a sample per sample gives 1.25, which is
/// the pitch moving up.
#[test]
fn sweeping_the_delay_time_moves_the_pitch_without_a_step() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let (write, read) = audio_delay(&mut graph, 200.0);
    let time = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);
    graph.connect(time, 0, read, 0);

    let program = compile(&graph, SLOTS).unwrap();
    let lane = program
        .audio_ops
        .iter()
        .find_map(|op| match op {
            AudioOp::DelayRead { lane, .. } => *lane,
            _ => None,
        })
        .expect("the wired time control got a lane") as usize;
    let lanes_per_row = lane + 1;

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    load(&mut engine, &graph);

    let mut heard = Vec::new();
    let mut clock = 0.0f32;
    // Four blocks of ramp at a fixed 300 samples back to fill the line,
    // then four with the time sweeping from 300 to 268.
    for block in 0..8 {
        let mut daw_in = vec![0.0f32; 2 * 128];
        for i in 0..128 {
            daw_in[i] = clock;
            daw_in[128 + i] = clock;
            clock += 1.0;
        }
        let mut lanes = vec![0.0f64; lanes_per_row * 4];
        for row in 0..4 {
            let swept = (block - 4).max(0) as f64 * 4.0 + row as f64;
            lanes[row * lanes_per_row + lane] = seconds(300.0 - swept * 8.0);
        }
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(
            &AudioContext {
                frames: 128,
                quantum: 32,
                sample_rate: RATE,
                lanes: &lanes,
                lanes_per_row,
            },
            &daw_in,
            &mut daw_out,
            &mut Adders,
        );
        if block >= 4 {
            heard.extend_from_slice(&daw_out[..128]);
        }
    }

    // The first sub-block of the sweep is still coming up to speed: it
    // ramps from where the held pointer was, so it is the one chunk whose
    // step is 1. Everything after it is at the sweep's own rate.
    let steps: Vec<f32> = heard[32..].windows(2).map(|w| w[1] - w[0]).collect();
    // Eight samples of sweep every 32 of output: a quarter faster. Every
    // step, not just the average — one step out of line is what a click is.
    let worst = steps
        .iter()
        .map(|s| (s - 1.25).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 0.02, "largest departure from 1.25 was {worst}");
}

/// A mix of one input is a gain, which is the whole reason the gains live
/// on `Mix` rather than on a node of their own.
#[test]
fn a_mix_of_one_is_a_gain() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let gain = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 1,
            gains: vec![linear_to_db(0.25)],
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, gain, 0);
    graph.connect(gain, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);

    let daw_in: Vec<f32> = (0..2 * 8).map(|i| i as f32).collect();
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    let want: Vec<f32> = daw_in.iter().map(|v| v * 0.25).collect();
    assert_eq!(daw_out, want);
}

/// Each input has its own gain, and the sum is of the scaled ones. This is
/// what turns a feedback loop's gain down below unity so it decays.
#[test]
fn each_mix_input_is_scaled_before_the_sum() {
    let mut graph = Graph::new();
    let a = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: vec![linear_to_db(0.5), linear_to_db(0.25)],
        }),
        [0.0, 0.0],
    );
    // The same source into both inputs: 0.5 + 0.25 of it should come out.
    graph.connect(a, 0, mix, 0);
    graph.connect(a, 0, mix, 2);
    graph.connect(mix, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);

    let daw_in = vec![4.0f32; 2 * 8];
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
    assert!(
        daw_out.iter().all(|&v| (v - 3.0).abs() < 1e-6),
        "{daw_out:?}"
    );
}

/// A gate with its fades off is a `Mix` of one whose gain the parameter
/// half switches, and this is the whole round trip: the control lands in a
/// lane, the lane becomes a gain, the gain is unity or silence.
#[test]
fn a_gate_passes_or_silences_by_its_control() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let gate = graph.add(
        NodeKind::Gate(Gate {
            channels: 2,
            threshold: 0.5,
            invert: false,
            fade_in_ms: 0.0,
            fade_out_ms: 0.0,
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, gate, 0);
    graph.connect(control, 0, gate, 1);
    graph.connect(gate, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(8, &[2]);
    load(&mut engine, &graph);

    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut render = |control: f64| -> Vec<f32> {
        let mut lanes = vec![0.0; width];
        lanes[0] = control;
        engine.run(&ctx(8), &mut lanes);
        let daw_in = vec![1.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 32,
                sample_rate: RATE,
                lanes: &lanes,
                lanes_per_row: width,
            },
            &daw_in,
            &mut daw_out,
            &mut Adders,
        );
        daw_out
    };

    assert!(
        render(1.0).iter().all(|&v| (v - 1.0).abs() < 1e-6),
        "an open gate is unity gain"
    );
    assert!(
        render(0.0).iter().all(|&v| v.abs() < 1e-6),
        "a shut gate is silence"
    );
}

/// A patch with a gate in it, its fades in milliseconds. Renders one DAW
/// block of 64 over four sub-blocks of 16, and hands back both channels.
fn fading_gate(fade_ms: f64) -> (Graph, Engine) {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let gate = graph.add(
        NodeKind::Gate(Gate {
            channels: 2,
            threshold: 0.5,
            invert: false,
            fade_in_ms: fade_ms,
            fade_out_ms: fade_ms,
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, gate, 0);
    graph.connect(control, 0, gate, 1);
    graph.connect(gate, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    load(&mut engine, &graph);
    (graph, engine)
}

/// One block of [`fading_gate`], the control taking one value per
/// sub-block. The input is unity, so what comes back is the gain itself.
fn gated_block(engine: &mut Engine, control: [f64; 4]) -> Vec<f32> {
    let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
    let mut rows = vec![0.0; width * control.len()];
    for (index, &value) in control.iter().enumerate() {
        let row = &mut rows[index * width..(index + 1) * width];
        row[0] = value;
        engine.run(
            &BlockContext {
                sample_rate: RATE,
                tempo_bpm: 120.0,
                frames: 16,
                offset: index as u32 * 16,
                row: index as u32,
                block: 64,
            },
            row,
        );
    }
    let mut daw_out = vec![0.0f32; 2 * 64];
    engine.run_audio(
        &AudioContext {
            frames: 64,
            quantum: 16,
            sample_rate: RATE,
            lanes: &rows,
            lanes_per_row: width,
        },
        &vec![1.0f32; 2 * 64],
        &mut daw_out,
        &mut Adders,
    );
    daw_out
}

/// The gain slides instead of stepping, and starts sliding at the
/// sub-block the control moved in.
///
/// Two claims, because they are one behaviour: a ramp that waited for the
/// chunk boundary would be smooth and late, and a gate that opened on time
/// by stepping would be on time and audible. The plugins are called once
/// for the block here — how often that happens is a cost decision, and the
/// resolution of what the graph was told is not the same question.
#[test]
fn a_gate_opens_over_its_fade_time_from_the_sub_block_it_was_told() {
    // A millisecond is 48 samples, which is longer than what is left of
    // the block: the fade is caught in flight rather than at its end.
    let (_graph, mut engine) = fading_gate(1.0);
    let step = 1.0 / 48.0;

    assert!(
        gated_block(&mut engine, [0.0; 4]).iter().all(|&v| v == 0.0),
        "a gate that has never opened is silence, and the first block does              not fade into it"
    );

    let heard = gated_block(&mut engine, [0.0, 0.0, 1.0, 1.0]);
    assert!(
        heard[..32].iter().all(|&v| v == 0.0),
        "shut for the two sub-blocks the control was low: {:?}",
        &heard[..34]
    );
    for i in 0..32 {
        let want = (step * (i + 1) as f64) as f32;
        assert!(
            (heard[32 + i] - want).abs() < 1e-5,
            "sample {} of the fade is {} rather than {want}",
            i,
            heard[32 + i]
        );
    }
    assert_eq!(
        heard[..64],
        heard[64..],
        "both channels travel together, or the ramp would move the image"
    );
}

/// A fade in flight is not restarted, nor finished early, by a recompile.
///
/// A recompile happens on every drag of every control, and one landing
/// mid-fade must not step the gain that the fade exists to stop stepping.
#[test]
fn a_fade_in_flight_survives_a_recompile() {
    let (graph, mut engine) = fading_gate(1.0);
    let step = 1.0 / 48.0;
    gated_block(&mut engine, [0.0; 4]);
    gated_block(&mut engine, [0.0, 0.0, 1.0, 1.0]);

    load(&mut engine, &graph);

    let heard = gated_block(&mut engine, [1.0; 4]);
    assert!(
        (heard[0] - (step * 33.0) as f32).abs() < 1e-5,
        "the ramp carries on from where the swap caught it, at {}",
        heard[0]
    );
    assert!(
        heard[15..64].iter().all(|&v| (v - 1.0).abs() < 1e-5),
        "and arrives on the sample it would have arrived on: {:?}",
        &heard[12..20]
    );
}

/// When a gain socket is driven by a parameter source, the parameter value
/// is interpreted as decibels and converted to a linear multiplier for audio.
#[test]
fn a_driven_gain_socket_interprets_its_value_as_decibels() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let gain_ctl = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 1,
            gains: vec![0.0],
        }),
        [0.0, 0.0],
    );
    graph.connect(input, 0, mix, 0);
    // Sockets alternate: input 1 is socket 0, gain 1 is socket 1.
    graph.connect(gain_ctl, 0, mix, 1);
    graph.connect(mix, 0, output, 0);

    let program = compile(&graph, SLOTS).unwrap();
    let lane = program
        .audio_ops
        .iter()
        .find_map(|op| match op {
            AudioOp::Mix { inputs, .. } => inputs[0].lane,
            _ => None,
        })
        .expect("the wired gain control got a lane") as usize;
    let lanes_per_row = lane + 1;
    let lanes = vec![-6.0; lanes_per_row];

    let mut engine = Engine::new();
    engine.prepare(8, &[2]);
    load(&mut engine, &graph);

    let daw_in = vec![1.0f32; 2 * 8];
    let mut daw_out = vec![0.0f32; 2 * 8];
    engine.run_audio(
        &AudioContext {
            frames: 8,
            quantum: 8,
            sample_rate: RATE,
            lanes: &lanes,
            lanes_per_row,
        },
        &daw_in,
        &mut daw_out,
        &mut Adders,
    );

    let want_linear = db_to_linear(-6.0) as f32;
    assert!(
        daw_out.iter().all(|&v| (v - want_linear).abs() < 1e-6),
        "expected {want_linear}, got {daw_out:?}"
    );
}

/// Verifies that resizing max delay time allocates a larger ring buffer while preserving existing samples.
#[test]
fn a_longer_max_time_gets_a_longer_ring_and_keeps_what_was_in_it() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    let (write, read) = audio_delay(&mut graph, 200.0);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let mut program = compile(&graph, SLOTS).unwrap();
    let sized = program.size_rings(RATE, &[]);
    // 0.05 s at 48 kHz, plus the interpolator's headroom.
    assert_eq!(program.audio_ring_len, vec![2404]);
    assert_eq!(program.audio_rings[0].len(), MAX_CHANNELS * 2404);

    // Publishing again with nothing changed hands over no ring at all.
    let mut again = compile(&graph, SLOTS).unwrap();
    let sized_again = again.size_rings(RATE, &sized);
    assert!(
        again.audio_rings[0].is_empty(),
        "an unchanged line is left alone"
    );
    assert_eq!(sized_again, sized);

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    let handoff = Handoff::new();
    handoff.send(Box::new(PreparedProgram {
        fallback_destinations: Vec::new(),
        program,
        publication: 0,
    }));
    assert!(engine.adopt_handoff(&handoff));

    let daw_in = impulse(128, 8);
    let mut daw_out = vec![0.0f32; 2 * 128];
    engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);

    // Now ask for four times the range. The ring has to be reallocated on
    // the main thread, and the impulse still in it has to survive.
    if let Some(NodeKind::DelayRead(DelayRead { max_time, .. })) =
        graph.node_mut(read).map(|n| &mut n.kind)
    {
        *max_time = 0.2;
    }
    let mut wider = compile(&graph, SLOTS).unwrap();
    wider.size_rings(RATE, &sized);
    assert_eq!(wider.audio_ring_len, vec![9604]);
    assert!(
        !wider.audio_rings[0].is_empty(),
        "a changed line gets a new ring"
    );
    let handoff = Handoff::new();
    handoff.send(Box::new(PreparedProgram {
        fallback_destinations: Vec::new(),
        program: wider,
        publication: 0,
    }));
    assert!(engine.adopt_handoff(&handoff));

    let mut daw_out = vec![0.0f32; 2 * 128];
    engine.run_audio(
        &audio_ctx(128),
        &vec![0.0; 2 * 128],
        &mut daw_out,
        &mut Adders,
    );
    assert_eq!(
        daw_out[..128]
            .iter()
            .position(|v| v.abs() > 0.9)
            .expect("the impulse came through the reallocation"),
        8 + 200 - 128
    );
}

/// Verifies that recompiling a patch preserves audio delay buffer contents.
#[test]
fn a_recompile_leaves_the_line_full() {
    let mut graph = Graph::new();
    let input = stereo_in(&mut graph);
    let output = stereo_out(&mut graph);
    // Longer than a block, so the impulse is still inside the line when
    // the swap happens.
    let (write, read) = audio_delay(&mut graph, 200.0);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let mut engine = Engine::new();
    engine.prepare(128, &[2]);
    load(&mut engine, &graph);

    let daw_in = impulse(128, 8);
    let mut daw_out = vec![0.0f32; 2 * 128];
    engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);

    // An edit somewhere else entirely, between the write and the read.
    let constant = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
    let slot = param_sink(&mut graph);
    graph.connect(constant, 0, slot, 0);
    load(&mut engine, &graph);

    let mut daw_out = vec![0.0f32; 2 * 128];
    engine.run_audio(
        &audio_ctx(128),
        &vec![0.0; 2 * 128],
        &mut daw_out,
        &mut Adders,
    );
    let peak = daw_out[..128]
        .iter()
        .fold(0.0f32, |best, v| best.max(v.abs()));
    assert!(peak > 0.9, "the impulse survived the swap, peak {peak}");
    assert_eq!(
        daw_out[..128]
            .iter()
            .position(|v| v.abs() > 0.9)
            .expect("and at the right moment"),
        8 + 200 - 128
    );
}
