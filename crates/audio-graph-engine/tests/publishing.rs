use audio_graph_engine::{
    AudioIn, AudioOut, Engine, Graph, MAX_AUDIO_LANES, MAX_GRAPH_PARAMS, NodeKind, PortType,
    ProgramPublisher, compile,
};
use subhost_adapter::{NoInstances, SlotSchedule};

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
    let mut schedule = SlotSchedule::new(MAX_GRAPH_PARAMS + MAX_AUDIO_LANES, FRAMES, 32);
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
