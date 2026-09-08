//! What the DAW does around a bounce, and what the wrapper has to survive.
//!
//! A fast export is not a special process call: the host switches the render
//! mode, and a render mode change is a deactivate and an activate with a
//! different buffer configuration. Every block between those two calls is
//! rendered to the file, so anything the wrapper only sets up on the *first*
//! activation is silence in the exported audio and silence on the desk
//! afterwards, until the next edit puts it back.
//!
//! Driven against `clap-test-plugin`, because the paths that break are the ones
//! taken only when a sub-plugin is loaded.

mod harness;

use harness::{BOUNCE, Block, Daw, LIVE, fixture_as_clap, fx_layout};

use audio_graph_engine::{AudioIn, AudioOut, DelayRead, Graph, Mix, NodeKind, PortType};
use audio_graph_plugin::{Shared, Wrapper, WrapperKind};
use nice_plug::prelude::ProcessMode;

/// Draw a quarter-second feedback delay around the sub-plugin.
///
/// ```text
///   AudioIn ─┐                ┌─> AudioOut
///            ├─> Mix ─────────┤
///   DelayRead┘                └─> DelayWrite
/// ```
///
/// A delay is what makes the ring buffers matter: they are allocated on the
/// main thread and ride in on the program, so a program that reaches the audio
/// thread without them leaves the line with nothing to read.
fn feedback_delay(shared: &Shared) {
    let mut patch = shared.patch();
    let graph = &mut patch.graph;
    *graph = Graph::new();
    let input = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [40.0, 40.0],
    );
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [600.0, 40.0],
    );
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: vec![0.0, -6.0],
        }),
        [320.0, 40.0],
    );
    let (write, read) = graph.add_delay(PortType::STEREO, [320.0, 240.0]);
    if let Some(NodeKind::DelayRead(DelayRead { time, max_time, .. })) =
        graph.node_mut(read).map(|n| &mut n.kind)
    {
        *time = 0.25;
        *max_time = 0.5;
    }
    graph.connect(input, 0, mix, 0);
    graph.connect(read, 0, mix, 2);
    graph.connect(mix, 0, output, 0);
    graph.connect(mix, 0, write, 0);
    drop(patch);
    shared.publish_graph();
}

/// Every activation hands the audio thread a program to run.
///
/// The graph is the only route from input to output, so a block that runs
/// without a program is a block of silence. `deactivate` gives the program
/// back to be freed off the audio thread, which means each activation has to
/// put one there again — including the pair a bounce is made of, and the pair
/// that brings the plugin back to the desk once the file is written.
#[test]
fn every_activation_leaves_the_audio_thread_a_program() {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    let layout = fx_layout();

    // The DAW's own order: the track is running long before anyone opens the
    // window and picks something to put on it.
    wrapper
        .activate(WrapperKind::Effect, &layout, &LIVE)
        .expect("the first activation");
    wrapper
        .shared()
        .load(&fixture_as_clap("reactivation-fixture"))
        .expect("the fixture loads");
    // What the editor draws when a plugin is picked with nothing else on the
    // canvas: input, the plugin, output. Then an echo around it.
    wrapper.shared().adopt_default_patch();
    feedback_delay(wrapper.shared());

    // Each activation is observed through a short impulse followed by silence:
    // the delayed echo proves that the prepared graph is running after the
    // activation, rather than only that the direct path is audible.
    for (what, config) in [("the bounce", BOUNCE), ("the return to the desk", LIVE)] {
        wrapper.deactivate();
        wrapper
            .activate(WrapperKind::Effect, &layout, &config)
            .unwrap_or_else(|| panic!("{what} activates"));
        let mut daw = Daw::playing();
        let mut impulse = Block::silent(128);
        impulse.fill(1.0);
        impulse.process(&mut wrapper, &mut daw);
        let mut heard = false;
        for _ in 0..100 {
            let mut block = Block::silent(128);
            block.process(&mut wrapper, &mut daw);
            heard |= block.peak() > 0.1;
        }
        assert!(
            heard,
            "{what} left the audio thread with no running program"
        );
        assert_eq!(
            wrapper
                .shared()
                .main()
                .config
                .expect("activation records the configuration")
                .offline,
            config.process_mode == ProcessMode::Offline,
            "{what}: the sub-plugin is told the wrong render mode"
        );
    }

    wrapper.deactivate();
}
