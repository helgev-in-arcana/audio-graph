use audio_graph_engine::{
    AudioIn, AudioOut, Engine, Graph, MAX_AUDIO_LANES, MAX_GRAPH_PARAMS, NodeKind, PortType,
    ProgramPublisher, compile,
};
use subhost_adapter::{NoInstances, SlotSchedule};

/// An unrelated filter edit cannot reset held notes on a branch whose buffers moved.
#[test]
fn note_state_follows_stream_identity_across_coalesced_publications() {
    use audio_graph_engine::{Follow, NoteFilter, NoteFollow, ParamPort, Plugin, PluginPorts};
    use plugin_host::{Event, NoteEvent};
    let mut graph = Graph::new();
    let input = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let filters = [
        graph.add(NodeKind::NoteFilter(NoteFilter::default()), [0.0; 2]),
        graph.add(
            NodeKind::NoteFilter(NoteFilter {
                channels: vec![1],
                ..Default::default()
            }),
            [0.0; 2],
        ),
    ];
    let sink = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                params: (0..2)
                    .map(|id| ParamPort {
                        id,
                        name: String::new(),
                    })
                    .collect(),
                ..Default::default()
            },
        }),
        [0.0; 2],
    );
    for (port, filter) in filters.into_iter().enumerate() {
        let follow = graph.add(
            NodeKind::NoteFollow(NoteFollow { what: Follow::Gate }),
            [0.0; 2],
        );
        graph.connect(input, 0, filter, 0);
        graph.connect(filter, 0, follow, 0);
        graph.connect(follow, 0, sink, port as u8);
    }
    let publisher = ProgramPublisher::default();
    let mut engine = Engine::new();
    engine.prepare(64, &[]);
    let program = compile(&graph, 0).unwrap();
    let targets = program.param_targets().to_vec();
    publisher.publish(program, 48_000.0);
    assert!(engine.adopt(&publisher));
    let mut schedule = SlotSchedule::new(MAX_GRAPH_PARAMS + MAX_AUDIO_LANES, 64, 32).unwrap();
    let run = |engine: &mut Engine, schedule: &mut SlotSchedule, events: &[Event]| {
        assert!(engine.run_block(
            schedule,
            &[],
            events,
            64,
            32,
            48_000.0,
            120.0,
            &[],
            &mut [],
            &mut NoInstances
        ));
        engine.end_block(&[], &mut Vec::with_capacity(256));
        [schedule.block(0)[0], schedule.block(0)[1]]
    };
    let on = Event::Note(NoteEvent::NoteOn {
        note_id: Some(17),
        port: 0,
        channel: 1,
        key: 60,
        velocity: 0.5,
        sample_offset: 40,
    });
    run(&mut engine, &mut schedule, &[on]);
    assert_eq!(run(&mut engine, &mut schedule, &[]), [1.0, 1.0]);
    for (channels, expected) in [(vec![0], [0.0, 1.0]), (vec![], [1.0, 1.0])] {
        let NodeKind::NoteFilter(filter) = &mut graph.node_mut(filters[0]).unwrap().kind else {
            unreachable!()
        };
        filter.channels = channels;
        let mut skipped = graph.clone();
        let NodeKind::NoteFilter(filter) = &mut skipped.node_mut(filters[1]).unwrap().kind else {
            unreachable!()
        };
        filter.channels = vec![2];
        publisher.publish(compile(&skipped, 0).unwrap(), 48_000.0);
        let program = compile(&graph, 0).unwrap();
        assert_eq!(program.param_targets(), targets);
        publisher.publish(program, 48_000.0);
        assert!(engine.adopt(&publisher));
        assert_eq!(run(&mut engine, &mut schedule, &[]), expected);
    }
    let replacement_input = graph.add(NodeKind::NoteIn, [0.0; 2]);
    graph.connect(replacement_input, 0, filters[1], 0);
    publisher.publish(compile(&graph, 0).unwrap(), 48_000.0);
    engine.adopt(&publisher);
    assert_eq!(run(&mut engine, &mut schedule, &[]), [1.0, 0.0]);
    graph.connect(input, 0, filters[1], 0);
    publisher.publish(compile(&graph, 0).unwrap(), 48_000.0);
    engine.adopt(&publisher);
    run(&mut engine, &mut schedule, &[on]);
    assert_eq!(run(&mut engine, &mut schedule, &[]), [1.0, 1.0]);
    let NodeKind::NoteFilter(filter) = &mut graph.node_mut(filters[1]).unwrap().kind else {
        unreachable!()
    };
    filter.channels = vec![2];
    publisher.publish(compile(&graph, 0).unwrap(), 48_000.0);
    engine.adopt(&publisher);
    assert_eq!(run(&mut engine, &mut schedule, &[]), [1.0, 0.0]);
}

/// Publication identifiers describe adoption and remain ordered across activation resets.
#[test]
fn publication_identity_follows_the_adopted_program() {
    let publisher = ProgramPublisher::default();
    let mut engine = Engine::new();
    let first = publisher.publish(audio_graph_engine::Program::empty(), 48_000.0);
    assert_eq!(engine.publication(), 0);
    let second = publisher.publish(audio_graph_engine::Program::empty(), 48_000.0);
    assert!(second > first);
    assert!(engine.adopt(&publisher));
    assert_eq!(engine.publication(), second);
    publisher.reset();
    let third = publisher.publish(audio_graph_engine::Program::empty(), 96_000.0);
    assert!(third > second);
    assert_eq!(engine.publication(), second);
    assert!(engine.adopt(&publisher));
    assert_eq!(engine.publication(), third);
    drop(engine.release());
    assert_eq!(engine.publication(), 0);
}

/// Coalesced publications supply new rings and preserve audio already held by unchanged rings.
#[test]
fn repeated_publications_keep_the_delay_audible() {
    const RATE: f64 = 48_000.0;
    const FRAMES: u32 = 128;
    const DELAY: usize = 200;
    const IMPULSE: usize = 8;

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
    let (write, read) = graph.add_delay(PortType::STEREO, [0.0, 0.0]);
    graph.connect(input, 0, write, 0);
    graph.connect(read, 0, output, 0);

    let publisher = ProgramPublisher::default();
    let mut engine = Engine::new();
    engine.prepare(FRAMES, &[2]);
    let mut schedule = SlotSchedule::new(MAX_GRAPH_PARAMS + MAX_AUDIO_LANES, FRAMES, 32).unwrap();
    let mut nodes = NoInstances;
    let mut daw_in = [0.0; 2 * FRAMES as usize];
    let mut daw_out = [0.0; 2 * FRAMES as usize];

    for max_time in [0.05, 0.2] {
        let NodeKind::DelayRead(delay) = &mut graph.node_mut(read).unwrap().kind else {
            panic!("the delay read node exists");
        };
        delay.time = DELAY as f64 / RATE;
        delay.max_time = max_time;
        publisher.publish(compile(&graph, 0).unwrap(), RATE);
        publisher.publish(compile(&graph, 0).unwrap(), RATE);
        assert!(engine.adopt(&publisher));

        for block in 0..2 {
            daw_in.fill(0.0);
            if block == 0 {
                daw_in[IMPULSE] = 1.0;
                daw_in[FRAMES as usize + IMPULSE] = 1.0;
            } else {
                publisher.publish(compile(&graph, 0).unwrap(), RATE);
                publisher.publish(compile(&graph, 0).unwrap(), RATE);
                assert!(engine.adopt(&publisher));
            }
            assert!(engine.run_block(
                &mut schedule,
                &[],
                &[],
                FRAMES,
                32,
                RATE,
                120.0,
                &daw_in,
                &mut daw_out,
                &mut nodes,
            ));
            if block == 0 {
                assert!(daw_out.iter().all(|&sample| sample == 0.0));
            } else {
                for channel in daw_out.as_chunks::<{ FRAMES as usize }>().0 {
                    assert_eq!(
                        channel.iter().position(|sample| sample.abs() > 0.9),
                        Some(IMPULSE + DELAY - FRAMES as usize),
                        "the delayed impulse survives publication at max_time={max_time}"
                    );
                }
            }
        }
    }
}
