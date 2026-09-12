//! Throwing away what the running graph is holding.
//!
//! The engine's own tests say what a reset empties. These say the request gets
//! there: from the editor's button, across to the audio thread, and into the
//! block that comes next — and that an All Notes Off arriving on the wire does
//! not take the patch with it.

mod harness;

use harness::{Block, Daw, LIVE, fx_layout};

use audio_graph_engine::{
    AudioIn, AudioOut, DelayRead, DelayWrite, Graph, NodeKind, Plugin, PluginPorts, PortType,
};
use audio_graph_plugin::{Wrapper, WrapperKind};

/// One block, and the delay's length.
const FRAMES: usize = 256;

/// What the wrapper puts on the input.
const LEVEL: f32 = 0.5;

/// A running wrapper whose patch is one delay line, a block long.
///
/// No sub-plugin anywhere in it: a delay is state the engine holds itself, so
/// this asks about the wrapper and the engine and nothing else.
fn one_block_of_delay() -> Wrapper {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the wrapper activates");

    {
        let mut patch = wrapper.shared().patch();
        patch.graph = Graph::new();
        let input = patch.graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let write = patch.graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::STEREO,
            }),
            [200.0, 0.0],
        );
        let seconds = FRAMES as f64 / LIVE.sample_rate as f64;
        let read = patch.graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::STEREO,
                max_time: seconds,
                time: seconds,
            }),
            [400.0, 0.0],
        );
        let output = patch.graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [600.0, 0.0],
        );
        patch.graph.connect(input, 0, write, 0);
        patch.graph.connect(read, 0, output, 0);
    }
    wrapper.shared().publish_graph();
    assert!(
        wrapper.shared().patch().compile_error.is_none(),
        "the patch compiles: {:?}",
        wrapper.shared().patch().compile_error
    );
    wrapper
}

/// Put one loud block in, and give back what comes out of the silent block
/// after it — which is the loud one, a delay later.
///
/// `between` runs after the line has been filled and before the block that
/// would empty it, because that is where a reset has anything to undo: asked
/// for before the loud block, it would be followed by the very sound it was
/// meant to stop.
fn echo(wrapper: &mut Wrapper, daw: &mut Daw, between: impl FnOnce(&Wrapper, &mut Daw)) -> f32 {
    let mut loud = Block::silent(FRAMES);
    loud.fill(LEVEL);
    loud.process(wrapper, daw);

    between(wrapper, daw);

    let mut quiet = Block::silent(FRAMES);
    quiet.process(wrapper, daw);
    quiet.peak()
}

/// The button empties the delay line the patch is still repeating.
///
/// A reset that only reaches the engine's own bookkeeping and not the audio it
/// is holding would pass every note-level check and still be heard carrying
/// on, which is the thing the user pressed it to stop.
#[test]
fn the_reset_button_empties_what_the_graph_is_still_holding() {
    let mut wrapper = one_block_of_delay();
    let mut daw = Daw::playing();

    let heard = echo(&mut wrapper, &mut daw, |_, _| {});
    assert!(
        heard > LEVEL / 2.0,
        "the delay repeats to start with, or there is nothing here to reset: peak {heard}"
    );

    let silent = echo(&mut wrapper, &mut daw, |wrapper, _| {
        wrapper.shared().request_reset();
    });
    assert!(
        silent < 1e-6,
        "the line came back with {silent} still in it"
    );
}

/// An All Notes Off stops the notes and leaves the patch alone.
///
/// A DAW is free to send one on every transport stop. A delay line emptied
/// each time — or a key switch thrown back — would make the patch unusable for
/// the sake of a message that only ever meant "nothing is playing".
#[test]
fn an_all_notes_off_does_not_empty_the_patch() {
    let mut wrapper = one_block_of_delay();
    let mut daw = Daw::playing();

    let heard = echo(&mut wrapper, &mut daw, |_, _| {});
    assert!(heard > LEVEL / 2.0, "the delay repeats to start with");

    let still_there = echo(&mut wrapper, &mut daw, |_, daw| {
        daw.incoming.push(nice_plug::prelude::NoteEvent::MidiCC {
            timing: 0,
            channel: 0,
            cc: 123,
            value: 0.0,
        });
    });
    assert!(
        still_there > LEVEL / 2.0,
        "an All Notes Off emptied the delay line: peak {still_there}"
    );
}

/// A reset hands the DAW back every note the graph was still holding.
///
/// A host allocates a voice per note it sends and frees it when told the note
/// is over. Emptying the ledger without a word would leave it holding one for
/// every key that happened to be down, for the rest of the session.
#[test]
fn a_reset_gives_the_daw_back_the_notes_the_graph_was_holding() {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the wrapper activates");

    // A plugin node with nothing loaded behind it is still where a note is
    // handed over, and nothing hands it back: the note stays the graph's until
    // something says otherwise, which is exactly the state a reset is for.
    {
        let mut patch = wrapper.shared().patch();
        patch.graph = Graph::new();
        let notes = patch.graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = patch.graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_out: vec![2],
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [200.0, 0.0],
        );
        let output = patch.graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [400.0, 0.0],
        );
        patch.graph.connect(notes, 0, synth, 0);
        patch.graph.connect(synth, 0, output, 0);
    }
    wrapper.shared().publish_graph();

    let mut daw = Daw::playing();
    daw.incoming.push(nice_plug::prelude::NoteEvent::NoteOn {
        timing: 0,
        voice_id: None,
        channel: 0,
        note: 60,
        velocity: 1.0,
    });
    Block::silent(FRAMES).process(&mut wrapper, &mut daw);
    assert!(
        daw.outgoing.is_empty(),
        "the note is still being played, so nothing has ended: {:?}",
        daw.outgoing
    );

    wrapper.shared().request_reset();
    Block::silent(FRAMES).process(&mut wrapper, &mut daw);
    assert!(
        daw.outgoing.iter().any(|event| matches!(
            event,
            nice_plug::prelude::NoteEvent::VoiceTerminated { note: 60, .. }
        )),
        "the DAW was not told the note is over: {:?}",
        daw.outgoing
    );
}

/// Event loss and processing failure release DAW voices and reset native held notes.
#[test]
fn output_overflow_resets_the_native_plugin_and_the_note_ledger() {
    plugin_host::init_thread();
    for command in [5.0, 11.0] {
        let mut wrapper = Wrapper::default();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
            .unwrap();
        wrapper
            .shared()
            .load(&harness::fixture_as_clap(&format!(
                "event-failure-reset-{command}"
            )))
            .unwrap();
        wrapper.shared().adopt_default_patch();
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
            let input = patch.graph.add(NodeKind::NoteIn, [0.0, 200.0]);
            patch.graph.connect(input, 0, plugin, note_port);
        }
        wrapper.shared().publish_graph();
        let mut daw = Daw::playing();
        daw.incoming.push(nice_plug::prelude::NoteEvent::NoteOn {
            timing: 0,
            voice_id: Some(42),
            channel: 0,
            note: 60,
            velocity: 1.0,
        });
        let mut held = Block::silent(FRAMES);
        held.process(&mut wrapper, &mut daw);
        assert!(held.peak() > 0.0);
        assert!(daw.outgoing.is_empty());
        wrapper
            .shared()
            .main()
            .host
            .set_sub_param(0, plugin_host::ParamId(5), command)
            .unwrap();
        Block::silent(FRAMES).process(&mut wrapper, &mut daw);
        assert!(daw.outgoing.iter().any(|event| matches!(
            event,
            nice_plug::prelude::NoteEvent::VoiceTerminated {
                voice_id: Some(42),
                ..
            }
        )));
        let mut after = Block::silent(FRAMES);
        after.process(&mut wrapper, &mut daw);
        assert_eq!(after.peak(), 0.0, "the native voice must stop too");
    }
}
