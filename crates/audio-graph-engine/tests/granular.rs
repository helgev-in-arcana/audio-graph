use audio_graph_engine::{
    AudioIn, AudioOut, Beats, Constant, Engine, Granular, Granularity, Graph, KeyParam,
    KeyParamMode, KeyTrigger, MAX_AUDIO_LANES, MAX_GRAPH_PARAMS, NodeKind, ProgramPublisher,
    compile,
};
use plugin_host::{Event, NoteEvent};
use subhost_adapter::{NoInstances, SlotSchedule};

const RATE: f64 = 1000.0;

struct AudioAllocator;
thread_local! {
    static AUDIO_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AUDIO_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn memory_operation() {
    let _ = AUDIO_ACTIVE.try_with(|active| {
        if active.get() {
            AUDIO_ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
    });
}

unsafe impl std::alloc::GlobalAlloc for AudioAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        memory_operation();
        unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        memory_operation();
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: AudioAllocator = AudioAllocator;

fn audio_thread(run: impl FnOnce() -> bool) -> bool {
    AUDIO_ALLOCATIONS.with(|count| count.set(0));
    AUDIO_ACTIVE.with(|active| active.set(true));
    let result = run();
    AUDIO_ACTIVE.with(|active| active.set(false));
    assert_eq!(
        AUDIO_ALLOCATIONS.with(|count| count.get()),
        0,
        "audio processing must neither allocate nor free memory"
    );
    result
}

struct Rig {
    graph: Graph,
    node: u32,
    input: u32,
    engine: Engine,
    publisher: ProgramPublisher,
    schedule: SlotSchedule,
    resolution: u32,
}

impl Rig {
    fn new() -> Self {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0; 2],
        );
        let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
        let node = graph.add(
            NodeKind::Granular(Granular {
                capacity: 0.128,
                history: 0.032,
                block: Beats::new(1, 64),
                size: 1.0,
                interval: 0.5,
                wet: 1.0,
                latch: true,
                ..Granular::default()
            }),
            [0.0; 2],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0; 2],
        );
        graph.connect(input, 0, node, 0);
        graph.connect(notes, 0, node, 1);
        graph.connect(node, 0, output, 0);
        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        let publisher = ProgramPublisher::default();
        publisher.publish(compile(&graph, 0).unwrap(), RATE);
        assert!(engine.adopt(&publisher));
        Self {
            graph,
            node,
            input,
            engine,
            publisher,
            schedule: SlotSchedule::new(MAX_GRAPH_PARAMS + MAX_AUDIO_LANES, 64, 1).unwrap(),
            resolution: 1,
        }
    }

    fn run(&mut self, input: &[f32], events: &[Event]) -> Vec<f32> {
        let input: Vec<_> = input
            .iter()
            .copied()
            .chain(input.iter().map(|x| -x))
            .collect();
        let mut output = vec![0.0; input.len()];
        assert!(audio_thread(|| self.engine.run_block(
            &mut self.schedule,
            &[],
            events,
            (input.len() / 2) as u32,
            Granularity {
                resolution: self.resolution,
                ..Default::default()
            },
            RATE,
            120.0,
            &input,
            &mut output,
            &mut NoInstances,
        )));
        for (a, b) in output[..output.len() / 2]
            .iter()
            .zip(&output[output.len() / 2..])
        {
            assert_eq!(*a, -*b);
        }
        output.truncate(output.len() / 2);
        output
    }

    fn publish(&mut self) {
        self.publisher
            .publish(compile(&self.graph, 0).unwrap(), RATE);
        self.publisher
            .publish(compile(&self.graph, 0).unwrap(), RATE);
        assert!(audio_thread(|| self.engine.adopt(&self.publisher)));
    }

    fn record(&mut self) {
        self.run(&[0.5; 32], &[note(true, 24, 0, 1.0)]);
        self.run(
            &[0.0; 16],
            &[note(false, 24, 0, 0.0), note(true, 26, 0, 1.0)],
        );
    }
}

fn note(on: bool, key: i16, sample_offset: u32, velocity: f64) -> Event {
    Event::Note(if on {
        NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity,
            sample_offset,
        }
    } else {
        NoteEvent::NoteOff {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity,
            sample_offset,
        }
    })
}

#[test]
fn recording_boundaries_follow_event_offsets_and_host_partitioning_does_not_change_audio() {
    let render = |partition: usize| {
        let mut rig = Rig::new();
        let mut output = Vec::new();
        for base in (0..192).step_by(partition) {
            let input: Vec<_> = (base..base + partition)
                .map(|at| {
                    if (10..30).contains(&at) {
                        0.5
                    } else if at < 32 {
                        99.0
                    } else {
                        0.0
                    }
                })
                .collect();
            let events: Vec<_> = [(10, true, 24), (30, false, 24), (32, true, 26)]
                .into_iter()
                .filter(|(at, _, _)| *at >= base && *at < base + partition)
                .map(|(at, on, key)| note(on, key, (at - base) as u32, 1.0))
                .collect();
            output.extend(rig.run(&input, &events));
        }
        assert!(output[64..].iter().all(|&x| x.abs() <= 0.50001));
        assert!(output[64..].iter().any(|&x| x.abs() > 0.1));
        output
    };
    assert_eq!(render(64), render(16));
    assert_eq!(render(16), render(1));
}

#[test]
fn recording_survives_publication_input_disconnection_and_transport_reset() {
    let mut rig = Rig::new();
    rig.record();
    rig.graph
        .add(NodeKind::Constant(Constant { value: 3.0 }), [0.0; 2]);
    rig.publish();
    assert!(rig.run(&[0.0; 32], &[]).iter().any(|&x| x > 0.1));
    rig.graph
        .links
        .retain(|link| !(link.from == rig.input && link.to == rig.node));
    rig.publish();
    rig.engine.reset();
    assert!(rig.run(&[0.0; 32], &[]).iter().any(|&x| x > 0.1));
    rig.run(&[0.0; 32], &[note(true, 25, 3, 1.0)]);
    assert!(rig.run(&[0.0; 32], &[]).iter().all(|&x| x == 0.0));
}

#[test]
fn coalesced_capacity_edits_replace_storage_and_allow_a_fresh_recording() {
    let mut rig = Rig::new();
    rig.record();
    let NodeKind::Granular(node) = &mut rig.graph.node_mut(rig.node).unwrap().kind else {
        unreachable!()
    };
    node.capacity = 0.256;
    rig.publish();
    assert!(rig.run(&[0.0; 32], &[]).iter().all(|&x| x == 0.0));
    rig.record();
    assert!(rig.run(&[0.0; 32], &[]).iter().any(|&x| x > 0.1));
}

#[test]
fn recording_and_playback_follow_the_node_when_other_granular_nodes_are_removed() {
    let mut rig = Rig::new();
    let extra = rig
        .graph
        .add(NodeKind::Granular(Granular::default()), [0.0; 2]);
    let output = rig.graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 1,
            channels: 2,
        }),
        [0.0; 2],
    );
    rig.graph.connect(extra, 0, output, 0);
    rig.graph.nodes.reverse();
    rig.publish();
    rig.record();
    rig.graph.remove(extra);
    rig.publish();
    assert!(rig.run(&[0.0; 32], &[]).iter().any(|&x| x > 0.1));
}

#[test]
fn a_velocity_split_operates_record_and_play_without_sharing_their_releases() {
    let mut rig = Rig::new();
    let NodeKind::Granular(node) = &mut rig.graph.node_mut(rig.node).unwrap().kind else {
        unreachable!()
    };
    node.record_key = KeyTrigger::Velocity {
        key: 24,
        min: 1,
        max: 63,
    };
    node.play_key = KeyTrigger::Velocity {
        key: 24,
        min: 64,
        max: 127,
    };
    node.latch = false;
    rig.publish();
    rig.run(&[0.5; 32], &[note(true, 24, 0, 0.25)]);
    let output = rig.run(
        &[0.0; 32],
        &[note(true, 24, 0, 1.0), note(false, 24, 0, 0.0)],
    );
    assert!(output[8..].iter().any(|&x| x > 0.1));
    rig.run(&[0.0; 32], &[note(false, 24, 0, 0.0)]);
    assert!(rig.run(&[0.0; 32], &[]).iter().all(|&x| x == 0.0));
}

#[test]
fn key_parameter_selection_uses_each_events_velocity_and_receipt_order() {
    let mut rig = Rig::new();
    rig.resolution = 16;
    let notes = rig
        .graph
        .nodes
        .iter()
        .find(|node| matches!(node.kind, NodeKind::NoteIn))
        .unwrap()
        .id;
    let selector = rig.graph.add(
        NodeKind::KeyParam(KeyParam {
            mode: KeyParamMode::Select,
            keys: vec![
                KeyTrigger::Velocity {
                    key: 40,
                    min: 1,
                    max: 63,
                },
                KeyTrigger::Velocity {
                    key: 40,
                    min: 64,
                    max: 127,
                },
            ],
            values: vec![0.0, 1.0],
            mute_keys: true,
        }),
        [0.0; 2],
    );
    rig.graph.connect(notes, 0, selector, 0);
    rig.graph.connect(selector, 0, rig.node, 5);
    rig.publish();
    rig.record();
    rig.run(
        &[0.0; 16],
        &[
            note(true, 40, 1, 1.0),
            note(true, 40, 2, 0.1),
            note(true, 60, 3, 1.0),
        ],
    );
    assert!(rig.run(&[0.0; 16], &[]).iter().all(|&x| x == 0.0));
    rig.run(
        &[0.0; 16],
        &[note(true, 40, 1, 0.1), note(true, 40, 2, 1.0)],
    );
    assert!(rig.run(&[0.0; 16], &[]).iter().any(|&x| x > 0.1));
    let NodeKind::KeyParam(node) = &mut rig.graph.node_mut(selector).unwrap().kind else {
        unreachable!()
    };
    node.mode = KeyParamMode::Toggle;
    rig.publish();
    rig.run(
        &[0.0; 16],
        &[note(true, 40, 1, 0.1), note(true, 40, 2, 0.1)],
    );
    assert!(
        rig.run(&[0.0; 16], &[]).iter().any(|&x| x > 0.1),
        "two toggles retain the selected value"
    );
}

#[test]
fn invalid_settings_are_rejected_and_saved_graphs_round_trip() {
    let mut rig = Rig::new();
    let saved = serde_json::to_string(&rig.graph).unwrap();
    let restored: Graph = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        compile(&restored, 0).unwrap(),
        compile(&rig.graph, 0).unwrap()
    );
    let NodeKind::Granular(node) = &mut rig.graph.node_mut(rig.node).unwrap().kind else {
        unreachable!()
    };
    node.history = f64::NAN;
    assert!(compile(&rig.graph, 0).is_err());
}
