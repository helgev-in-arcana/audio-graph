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

use std::path::PathBuf;

use audio_graph_engine::{AudioIn, AudioOut, DelayRead, Graph, Mix, NodeKind, PortType, Program};
use audio_graph_plugin::{Shared, Wrapper, WrapperKind};
use nice_plug::prelude::*;

/// Locates the built CLAP test fixture and copies it under a `.clap` name.
///
/// The facade infers the format from the extension and cargo's artefact is
/// named `.dll`, so it is copied rather than renamed: the original belongs to
/// cargo and the next build would replace it anyway.
///
/// Panics when the fixture is missing rather than skipping, because a skip
/// would make a green run mean nothing.
fn fixture_as_clap() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let build_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the test binary is two levels below the build directory");
    let source = [
        "clap_test_plugin.dll",
        "libclap_test_plugin.so",
        "libclap_test_plugin.dylib",
    ]
    .iter()
    .map(|n| build_dir.join(n))
    .find(|p| p.is_file())
    .unwrap_or_else(|| {
        panic!(
            "clap-test-plugin is not in {}.\n\
             Run `cargo build --workspace` before `cargo test --workspace`: \
             cargo does not build another package's cdylib on its own.",
            build_dir.display()
        )
    });

    // A distinct name per test binary, so two of them cannot copy over each
    // other's file while the other has it loaded.
    let target = build_dir.join("reactivation-fixture.clap");
    std::fs::copy(&source, &target).expect("the fixture can be copied");
    target
}

const STEREO: NonZeroU32 = new_nonzero_u32(2);

/// The effect form's layout, as the DAW hands it over.
fn fx_layout() -> AudioIOLayout {
    AudioIOLayout {
        main_input_channels: Some(STEREO),
        main_output_channels: Some(STEREO),
        aux_input_ports: &[STEREO],
        ..AudioIOLayout::const_default()
    }
}

/// Playing back at the desk.
const LIVE: BufferConfig = BufferConfig {
    sample_rate: 48_000.0,
    min_buffer_size: None,
    max_buffer_size: 512,
    process_mode: ProcessMode::Realtime,
};

/// The same project being written to a file as fast as the machine manages.
/// A larger block is what a host asks for when it no longer has to keep up
/// with a sound card.
const BOUNCE: BufferConfig = BufferConfig {
    max_buffer_size: 4096,
    process_mode: ProcessMode::Offline,
    ..LIVE
};

/// The audio thread's side of the handoff, which is one `Engine::adopt`.
fn adopt(wrapper: &Wrapper, held: &mut Option<Box<Program>>) -> bool {
    wrapper.shared().programs().take(held)
}

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
        .load(&fixture_as_clap())
        .expect("the fixture loads");
    // What the editor draws when a plugin is picked with nothing else on the
    // canvas: input, the plugin, output. Then an echo around it.
    wrapper.shared().adopt_default_patch();
    feedback_delay(wrapper.shared());

    // The audio thread picks that up and the track is heard. From here on the
    // only thing that may send a program is an activation.
    let mut held: Option<Box<Program>> = None;
    assert!(
        adopt(&wrapper, &mut held),
        "the edit reaches the audio thread"
    );

    for (what, config) in [("the bounce", BOUNCE), ("the return to the desk", LIVE)] {
        wrapper.deactivate();
        wrapper
            .activate(WrapperKind::Effect, &layout, &config)
            .unwrap_or_else(|| panic!("{what} activates"));
        assert!(
            adopt(&wrapper, &mut held),
            "{what} left the audio thread with no program: silence until an edit compiles one"
        );
        let program = held.as_ref().expect("adopting leaves a program held");
        for line in 0..program.audio_delay_nodes.len() {
            assert!(
                program.audio_rings.get(line).is_some_and(|r| !r.is_empty()),
                "{what} handed over delay line {line} with no ring: it repeats nothing"
            );
        }
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
