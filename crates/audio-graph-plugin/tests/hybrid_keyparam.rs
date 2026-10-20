mod harness;

use audio_graph_engine::{
    AudioIn, AudioOut, Beats, Constant, Graph, KeyParam, KeyParamMode, MidiDelay, Mix, NodeId,
    NodeKind, db_to_linear,
};
use audio_graph_plugin::{Wrapper, WrapperKind};
use harness::{Block, Daw, LIVE, fx_layout};
use nice_plug::prelude::NoteEvent;

fn playing(mode: KeyParamMode) -> (Wrapper, NodeId) {
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    let mut graph = Graph::new();
    let input = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0; 2],
    );
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let param = graph.add(
        NodeKind::KeyParam(KeyParam {
            mode,
            keys: vec![60, 61],
            values: vec![-12.0, -6.0],
            mute_keys: true,
        }),
        [0.0; 2],
    );
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 1,
            gains: vec![0.0],
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
    graph.connect(input, 0, mix, 0);
    graph.connect(notes, 0, param, 0);
    graph.connect(param, 0, mix, 1);
    graph.connect(mix, 0, output, 0);
    wrapper.shared().patch().graph = graph;
    publish(&wrapper);
    (wrapper, param)
}

fn publish(wrapper: &Wrapper) {
    wrapper.shared().publish_graph();
    assert!(
        wrapper.shared().patch().compile_error.is_none(),
        "{:?}",
        wrapper.shared().patch().compile_error
    );
}

fn strike(daw: &mut Daw, key: u8) {
    daw.incoming.push(NoteEvent::NoteOn {
        timing: 0,
        voice_id: None,
        channel: 0,
        note: key,
        velocity: 1.0,
    });
}

fn heard(wrapper: &mut Wrapper, daw: &mut Daw, param: NodeId, db: f64) {
    let mut block = Block::silent(64);
    block.fill(1.0).process(wrapper, daw);
    block.fill(1.0).process(wrapper, daw);
    let actual = wrapper
        .shared()
        .live_sockets()
        .into_iter()
        .find_map(|(socket, value)| (socket == (param, 0)).then_some(value))
        .expect("the key parameter has a displayed output");
    assert_eq!(actual, db);
    assert!((f64::from(block.peak()) - db_to_linear(db)).abs() < 1e-6);
}

/// Publishing preserves the selected value while socket reports follow recompiles and input aliases.
#[test]
fn key_parameter_edits_reach_audio_and_socket_values() {
    let _thread = plugin_host::init_thread().unwrap();
    let (mut wrapper, param) = playing(KeyParamMode::Select);
    let mut daw = Daw::playing();
    heard(&mut wrapper, &mut daw, param, -12.0);
    strike(&mut daw, 61);
    heard(&mut wrapper, &mut daw, param, -6.0);

    {
        let mut patch = wrapper.shared().patch();
        let NodeKind::KeyParam(key_param) = &mut patch.graph.node_mut(param).unwrap().kind else {
            unreachable!()
        };
        key_param.values[1] = -3.0;
    }
    publish(&wrapper);
    heard(&mut wrapper, &mut daw, param, -3.0);

    {
        let mut patch = wrapper.shared().patch();
        let constant = patch
            .graph
            .add(NodeKind::Constant(Constant { value: -18.0 }), [0.0; 2]);
        patch.graph.connect(constant, 0, param, 1);
        patch.graph.disconnect(param, 0);
    }
    publish(&wrapper);
    assert!(
        wrapper.shared().live_sockets().is_empty(),
        "the aliased output must not use the preceding program's register map"
    );
    heard(&mut wrapper, &mut daw, param, -18.0);
    wrapper.deactivate();
}

/// The reset button clears key selection through the wrapper's audio handoff.
#[test]
fn resetting_a_key_parameter_returns_to_its_first_value() {
    let _thread = plugin_host::init_thread().unwrap();
    let (mut wrapper, param) = playing(KeyParamMode::Toggle);
    let mut daw = Daw::playing();
    strike(&mut daw, 60);
    heard(&mut wrapper, &mut daw, param, -6.0);
    wrapper.shared().request_reset();
    heard(&mut wrapper, &mut daw, param, -12.0);
    strike(&mut daw, 60);
    heard(&mut wrapper, &mut daw, param, -6.0);
    wrapper.deactivate();
}

/// A saved preset restores key-parameter settings without serializing its live selection.
#[test]
fn a_key_parameter_preset_starts_a_new_instance_on_its_first_value() {
    let _thread = plugin_host::init_thread().unwrap();
    let (mut wrapper, param) = playing(KeyParamMode::Select);
    let mut daw = Daw::playing();
    strike(&mut daw, 61);
    heard(&mut wrapper, &mut daw, param, -6.0);
    wrapper.store_state();
    let saved = wrapper.shared().params().state.0.read().unwrap().clone();
    wrapper.deactivate();

    let mut restored = Wrapper::default();
    *restored.shared().params().state.0.write().unwrap() = saved;
    restored
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    heard(&mut restored, &mut daw, param, -12.0);
    strike(&mut daw, 61);
    heard(&mut restored, &mut daw, param, -6.0);
    restored.deactivate();
}

/// A reset releases a queued note's DAW voice and prevents its later delivery to a key parameter.
#[test]
fn resetting_a_midi_delay_discards_pending_notes_and_releases_their_voices() {
    let _thread = plugin_host::init_thread().unwrap();
    let (mut wrapper, param) = playing(KeyParamMode::Select);
    const FRAMES: usize = 64;
    {
        let mut patch = wrapper.shared().patch();
        let notes = patch
            .graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, NodeKind::NoteIn))
            .unwrap()
            .id;
        let delay = patch.graph.add(
            NodeKind::MidiDelay(MidiDelay {
                seconds: (4 * FRAMES) as f64 / f64::from(LIVE.sample_rate),
                beats: Beats::new(1, 1),
                sync: false,
            }),
            [0.0; 2],
        );
        patch.graph.connect(notes, 0, delay, 0);
        patch.graph.connect(delay, 0, param, 0);
    }
    publish(&wrapper);
    let value = |wrapper: &Wrapper| {
        wrapper
            .shared()
            .live_sockets()
            .into_iter()
            .find_map(|(socket, value)| (socket == (param, 0)).then_some(value))
            .unwrap()
    };
    let mut daw = Daw::playing();
    daw.incoming.push(NoteEvent::NoteOn {
        timing: 0,
        voice_id: Some(42),
        channel: 0,
        note: 61,
        velocity: 1.0,
    });
    let mut block = Block::silent(FRAMES);
    block.process(&mut wrapper, &mut daw);
    assert_eq!(value(&wrapper), -12.0);
    assert!(
        daw.outgoing.is_empty(),
        "the queued note still owns its voice"
    );

    wrapper.shared().request_reset();
    block.process(&mut wrapper, &mut daw);
    assert!(matches!(
        daw.outgoing.as_slice(),
        [NoteEvent::VoiceTerminated {
            voice_id: Some(42),
            channel: 0,
            note: 61,
            ..
        }]
    ));
    daw.outgoing.clear();
    for _ in 0..6 {
        block.process(&mut wrapper, &mut daw);
        assert_eq!(value(&wrapper), -12.0, "the discarded note must not arrive");
    }
    assert!(daw.outgoing.is_empty(), "the voice is released only once");

    strike(&mut daw, 61);
    for _ in 0..6 {
        block.process(&mut wrapper, &mut daw);
    }
    assert_eq!(
        value(&wrapper),
        -6.0,
        "fresh notes still traverse the delay"
    );
    wrapper.deactivate();
}
