//! The wrapper's own `process`, driven the way a DAW drives it.
//!
//! Everything else about the audio path is checked through the bundled binary
//! by `host-cli`. These are the questions that are about the wrapper itself
//! rather than about the graph: what comes out of a block, and what the wrapper
//! says to the host while blocks are going through it.

mod harness;

use harness::{BOUNCE, Block, Daw, LIVE, fixture_as_clap, fixture_state, fx_layout};

use audio_graph_plugin::{Wrapper, WrapperKind};

/// The latency the fixture is asked to claim. Any number a plugin might
/// plausibly want; nothing here depends on which.
const LATENCY: u32 = 128;

/// What the wrapper puts on the input, and the least it may leave on the
/// output for the block to count as having come through.
const LEVEL: f32 = 0.5;

/// A wrapper with the fixture wired between input and output, running.
fn playing(name: &str) -> Wrapper {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the first activation");
    wrapper
        .shared()
        .load(&fixture_as_clap(name))
        .expect("the fixture loads");
    wrapper.shared().adopt_default_patch();
    wrapper
}

/// Main-thread refresh preserves running notes for labels, and rebuilds routing and slots for structural changes.
#[test]
fn metadata_changes_reach_graph_ports_and_parameter_bindings() {
    use audio_graph_engine::NodeKind;
    use plugin_host::ParamId;
    let mut wrapper = playing("metadata-refresh");
    {
        let mut patch = wrapper.shared().patch();
        let (plugin, note_port) = patch
            .graph
            .nodes
            .iter()
            .find_map(|node| {
                if let NodeKind::Plugin(plugin) = &node.kind {
                    Some((node.id, plugin.ports.audio_in.len() as u8))
                } else {
                    None
                }
            })
            .unwrap();
        let notes = patch.graph.add(NodeKind::NoteIn, [0.0; 2]);
        patch.graph.connect(notes, 0, plugin, note_port);
    }
    wrapper
        .shared()
        .main()
        .host
        .bind_slot(0, 0, ParamId(1))
        .unwrap();
    wrapper.shared().publish_graph();
    wrapper.shared().rebind().unwrap();
    let mut daw = Daw::playing();
    daw.incoming.push(nice_plug::prelude::NoteEvent::NoteOn {
        timing: 0,
        voice_id: Some(42),
        channel: 0,
        note: 60,
        velocity: 1.0,
    });
    let mut before = Block::silent(64);
    before.process(&mut wrapper, &mut daw);
    let ask = |wrapper: &mut Wrapper, daw: &mut Daw, value| {
        wrapper
            .shared()
            .main()
            .host
            .set_sub_param(0, ParamId(5), value)
            .unwrap();
        Block::silent(64).process(wrapper, daw);
        wrapper.tick();
    };
    ask(&mut wrapper, &mut daw, 8.0);
    assert_eq!(wrapper.shared().main().host.params(0)[0].name, "Level");
    let mut held = Block::silent(64);
    held.process(&mut wrapper, &mut daw);
    assert_eq!(
        held.peak(),
        before.peak(),
        "renaming cannot deactivate the native voice"
    );
    ask(&mut wrapper, &mut daw, 7.0);
    assert_eq!(wrapper.shared().main().host.params(0)[0].max, 4.0);
    assert!(wrapper.shared().main().host.slots().resolved(0).is_none());
    let patch = wrapper.shared().patch();
    assert!(patch.compile_error.is_none(), "{:?}", patch.compile_error);
    let ports = patch
        .graph
        .nodes
        .iter()
        .find_map(|node| {
            if let NodeKind::Plugin(plugin) = &node.kind {
                Some(&plugin.ports)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(ports.audio_in[0], 1);
    assert_eq!(ports.audio_out[0], 1);
    drop(patch);
    let mut block = Block::silent(64);
    block.fill(0.5);
    block.process(&mut wrapper, &mut daw);
    assert!((block.peak() - 0.5).abs() < 1e-6);
    assert!(
        daw.outgoing.iter().any(|event| matches!(
            event,
            nice_plug::prelude::NoteEvent::VoiceTerminated {
                voice_id: Some(42),
                ..
            }
        )),
        "voices stopped by reconfiguration must also be returned to the DAW"
    );
    ask(&mut wrapper, &mut daw, 9.0);
    assert!(wrapper.shared().patch().compile_error.is_some());
    wrapper
        .shared()
        .main()
        .host
        .set_sub_param(0, ParamId(5), 8.0)
        .unwrap();
    wrapper.tick();
    assert!(wrapper.shared().patch().compile_error.is_none());
    block.process(&mut wrapper, &mut daw);
    wrapper
        .shared()
        .load_sub_state(0, &fixture_state(0.0))
        .unwrap();
    assert_eq!(
        wrapper
            .shared()
            .main()
            .host
            .io_layout(0)
            .main_input_channels(),
        2
    );
    assert!(wrapper.shared().main().host.slots().resolved(0).is_some());
    assert!(wrapper.shared().patch().graph.nodes.iter().any(
        |node| matches!(&node.kind, NodeKind::Plugin(plugin) if plugin.ports.audio_in[0] == 2)
    ));
    block.process(&mut wrapper, &mut daw);
}

/// A bounce renders the audio, and leaves the wrapper able to go on rendering
/// it.
///
/// A render mode change is a deactivate and an activate, and every block of the
/// export goes through the configuration that pair leaves behind. A wrapper
/// that only sets its audio path up on the first of those writes a silent file
/// and stays silent on the desk afterwards.
#[test]
fn a_bounce_renders_the_audio_and_gives_it_back_afterwards() {
    let mut wrapper = playing("audio-path-bounce");
    let mut daw = Daw::playing();

    let mut audible = |wrapper: &mut Wrapper, what: &str| {
        let mut block = Block::silent(256);
        block.fill(LEVEL);
        block.process(wrapper, &mut daw);
        assert!(
            block.peak() > 0.0,
            "{what}: the block came back silent, with a peak of {}",
            block.peak()
        );
    };

    audible(&mut wrapper, "on the configuration it was loaded with");

    for (what, config) in [
        ("after a plain re-activation", LIVE),
        ("during the bounce", BOUNCE),
        ("back at the desk", LIVE),
    ] {
        wrapper.deactivate();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &config)
            .unwrap_or_else(|| panic!("{what} activates"));
        audible(&mut wrapper, what);
    }

    wrapper.deactivate();
}

/// Changing parameter-lane order during playback never pairs a plan with another activation's map.
#[test]
fn changing_parameter_bindings_keeps_each_block_consistent() {
    use audio_graph_engine::{
        AudioIn, AudioOut, Constant, Graph, NodeKind, ParamPort, Plugin, PluginPorts,
    };
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let mut wrapper = playing("audio-path-configurations");
    let shared = wrapper.shared().clone();
    let layout = shared.main().host.io_layout(0);
    let graphs: Vec<_> = [false, true]
        .into_iter()
        .map(|reverse| {
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
            let mut ports = PluginPorts::from_layout(&layout, 0);
            let first_parameter = ports.audio_in.len() + usize::from(ports.accepts_notes);
            let ids = if reverse { [1, 0] } else { [0, 1] };
            ports.params = ids
                .iter()
                .map(|&id| ParamPort {
                    id,
                    name: id.to_string(),
                })
                .collect();
            let plugin = graph.add(NodeKind::Plugin(Plugin { instance: 0, ports }), [0.0, 0.0]);
            graph.connect(input, 0, plugin, 0);
            graph.connect(plugin, 0, output, 0);
            for (index, id) in ids.into_iter().enumerate() {
                let value = if id == 0 { 0.75 } else { 0.25 };
                let constant = graph.add(NodeKind::Constant(Constant { value }), [0.0, 0.0]);
                graph.connect(constant, 0, plugin, (first_parameter + index) as u8);
            }
            graph
        })
        .collect();
    shared.patch().graph = graphs[0].clone();
    shared.publish_graph();
    let mut initial = Block::silent(32);
    initial.fill(0.5).process(&mut wrapper, &mut Daw::playing());
    assert!((initial.peak() - 0.25).abs() < 1e-6);

    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(2));
    let audio = {
        let stop = stop.clone();
        let start = start.clone();
        std::thread::spawn(move || {
            let mut daw = Daw::playing();
            let mut block = Block::silent(32);
            let mut blocks = 0;
            start.wait();
            while !stop.load(Ordering::Acquire) || blocks == 0 {
                block.fill(0.5).process(&mut wrapper, &mut daw);
                let peak = block.peak();
                // Gain 1.5 and offset -0.5 render 0.25. A suspended block passes
                // 0.5 through; swapped parameter targets would instead render 0.75.
                assert!(
                    (peak - 0.25).abs() < 1e-6 || (peak - 0.5).abs() < 1e-6,
                    "inconsistent block: {peak}"
                );
                blocks += 1;
                std::thread::yield_now();
            }
            (wrapper, blocks)
        })
    };
    start.wait();
    for index in 0..500 {
        shared.patch().graph = graphs[index % 2].clone();
        shared.publish_graph();
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Release);
    let (mut wrapper, blocks) = audio.join().unwrap();
    assert!(blocks > 0);
    wrapper.deactivate();
}

/// Failed bus reconfiguration cannot reuse the old processor and can recover.
#[test]
fn a_failed_configuration_is_silent_and_can_be_rebuilt() {
    use audio_graph_engine::{AudioIn, AudioOut, Graph, NodeKind, Plugin, PluginPorts};
    let mut wrapper = playing("audio-path-failed-configuration");
    let shared = wrapper.shared().clone();
    let original = shared.patch().graph.clone();
    let mut mono = Graph::new();
    let input = mono.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0, 0.0],
    );
    let plugin = mono.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_in: vec![2],
                audio_out: vec![1],
                ..Default::default()
            },
        }),
        [0.0, 0.0],
    );
    let output = mono.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 1,
        }),
        [0.0, 0.0],
    );
    mono.connect(input, 0, plugin, 0);
    mono.connect(plugin, 0, output, 0);
    shared.patch().graph = mono;
    assert!(shared.rebind().is_err());
    assert!(
        shared.patch().compile_error.is_none(),
        "the graph is valid but the plugin refuses mono"
    );
    assert!(!shared.has_processors());
    let mut block = Block::silent(32);
    let mut daw = Daw::playing();
    block.fill(0.5).process(&mut wrapper, &mut daw);
    assert_eq!(block.peak(), 0.0);
    shared.patch().graph = original;
    shared.publish_graph();
    assert!(shared.has_processors());
    block.fill(0.5).process(&mut wrapper, &mut daw);
    assert!((block.peak() - 0.5).abs() < 1e-6);
    wrapper.deactivate();
}

/// A latency that appears while the project is playing reaches the DAW.
///
/// Dropping a plugin with lookahead onto the canvas mid-take moves the whole
/// track, and the host has to hear about it before the next block is used.
/// `process` is the only place the wrapper is handed anything that can say so
/// between activations, so a wrapper that only answers at activate leaves the
/// track running late until the DAW next restarts it.
#[test]
fn a_latency_that_appears_mid_session_reaches_the_daw() {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the first activation");

    let mut daw = Daw::playing();
    let mut block = Block::silent(256);
    block.process(&mut wrapper, &mut daw);
    assert_eq!(
        daw.latency.get(),
        None,
        "an empty canvas costs nothing, and a block with nothing to report \
         must not restart the DAW's processing to say so"
    );

    // The user picks a plugin, and it comes with lookahead.
    wrapper
        .shared()
        .load(&fixture_as_clap("audio-path-latency"))
        .expect("the fixture loads");
    wrapper
        .shared()
        .load_sub_state(0, &fixture_state(f64::from(LATENCY)))
        .expect("the fixture takes its state");
    wrapper.shared().adopt_default_patch();

    block.process(&mut wrapper, &mut daw);
    assert_eq!(
        daw.latency.get(),
        Some(LATENCY),
        "the track is still being aligned by a latency the patch no longer has"
    );

    wrapper.deactivate();
}
