//! What the wrapper tells the DAW to align the track by.
//!
//! The number belongs to the graph, not to any one plugin: a plugin's own
//! latency reaches it through the node that names it, the compiler lines
//! parallel paths up against the longest of them, and a plugin nothing routes
//! through costs nothing. Driven against `clap-test-plugin`, whose latency is
//! ours to set.

use std::path::PathBuf;

use audio_graph_engine::{AudioIn, AudioOut, Graph, NodeId, NodeKind, Plugin, PluginPorts};
use audio_graph_plugin::{Shared, Wrapper, WrapperKind};
use nice_plug::prelude::*;
use plugin_host::ParamId;

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
    let target = build_dir.join("latency-fixture.clap");
    std::fs::copy(&source, &target).expect("the fixture can be copied");
    target
}

/// The fixture's saved state: gain, offset, mode and latency, as little-endian
/// doubles in parameter order.
///
/// Mirrors the fixture's own format rather than importing it, the way the CLAP
/// backend's tests mirror its constants: a drift between the two should fail
/// the test rather than be papered over.
fn fixture_state(latency: f64) -> [u8; 32] {
    let mut blob = [0u8; 32];
    blob[..8].copy_from_slice(&1.0f64.to_le_bytes());
    blob[24..].copy_from_slice(&latency.to_le_bytes());
    blob
}

/// The latency each fixture is asked to report. Any number a plugin might
/// plausibly claim; nothing else in the wrapper depends on which.
const LATENCY: u32 = 128;

/// The fixture's own parameter ids, mirrored rather than imported for the same
/// reason its state layout is: a drift between the two should fail the test.
const PARAM_LATENCY: ParamId = ParamId(3);
const PARAM_ASK: ParamId = ParamId(5);
/// The `ask` value that makes the fixture tell its host the latency moved.
const ASK_LATENCY_CHANGED: f64 = 4.0;

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

const LIVE: BufferConfig = BufferConfig {
    sample_rate: 48_000.0,
    min_buffer_size: None,
    max_buffer_size: 512,
    process_mode: ProcessMode::Realtime,
};

/// Two plugin nodes, both loaded with the fixture, and the wires to hang them
/// on.
struct Patch {
    input: NodeId,
    output: NodeId,
    first: NodeId,
    second: NodeId,
}

fn two_plugins(shared: &Shared, path: &std::path::Path) -> Patch {
    shared.load_into(0, path).expect("the fixture loads");
    shared.load_into(1, path).expect("the fixture loads twice");

    let patch = {
        let mut held = shared.patch();
        let graph = &mut held.graph;
        *graph = Graph::new();
        Patch {
            input: graph.add(
                NodeKind::AudioIn(AudioIn {
                    bus: 0,
                    channels: 2,
                }),
                [40.0, 40.0],
            ),
            output: graph.add(
                NodeKind::AudioOut(AudioOut {
                    bus: 0,
                    channels: 2,
                }),
                [600.0, 40.0],
            ),
            first: graph.add(
                NodeKind::Plugin(Plugin {
                    instance: 0,
                    ports: PluginPorts::default(),
                }),
                [200.0, 40.0],
            ),
            second: graph.add(
                NodeKind::Plugin(Plugin {
                    instance: 1,
                    ports: PluginPorts::default(),
                }),
                [400.0, 40.0],
            ),
        }
    };
    // Sockets before links: `discover_ports` prunes, and a link into a socket
    // the node does not have yet is exactly what it prunes.
    shared.discover_ports(patch.first);
    shared.discover_ports(patch.second);
    patch
}

/// Ask each fixture to claim `samples` of latency from now on.
fn claims(shared: &Shared, instance: usize, samples: f64) {
    shared
        .main()
        .host
        .load_sub_state(instance, &fixture_state(samples))
        .expect("the fixture takes its state");
}

/// The DAW is told what the graph costs, not what one plugin costs.
///
/// The track is aligned by whatever comes back from `activate`, so the answer
/// has to be the latency of the audio that actually reaches the output: the
/// whole path through the canvas, and nothing that is only sitting on it.
/// Asking the first sub-plugin instead is right for exactly one patch — a
/// single plugin wired straight through — and wrong for every other.
#[test]
fn the_daw_is_told_what_the_graph_costs() {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    let layout = fx_layout();

    // The DAW's own order: the track is running long before anyone opens the
    // window and puts something on it.
    wrapper
        .activate(WrapperKind::Effect, &layout, &LIVE)
        .expect("the first activation");
    let patch = two_plugins(wrapper.shared(), &fixture_as_clap());

    // In → first → second → out, each plugin holding the signal up by the same
    // amount.
    {
        let mut held = wrapper.shared().patch();
        let graph = &mut held.graph;
        graph.connect(patch.input, 0, patch.first, 0);
        graph.connect(patch.first, 0, patch.second, 0);
        graph.connect(patch.second, 0, patch.output, 0);
    }
    claims(wrapper.shared(), 0, f64::from(LATENCY));
    claims(wrapper.shared(), 1, f64::from(LATENCY));
    assert_eq!(
        wrapper.activate(WrapperKind::Effect, &layout, &LIVE),
        Some(LATENCY * 2),
        "a signal held up twice on its way through has to be reported twice"
    );

    // The first plugin taken out of the path, and left claiming a latency
    // nobody is waiting on. A plugin sitting unwired on the canvas costs the
    // project nothing.
    wrapper.deactivate();
    {
        let mut held = wrapper.shared().patch();
        let graph = &mut held.graph;
        graph.links.clear();
        graph.connect(patch.input, 0, patch.second, 0);
        graph.connect(patch.second, 0, patch.output, 0);
    }
    claims(wrapper.shared(), 0, 4.0 * f64::from(LATENCY));
    assert_eq!(
        wrapper.activate(WrapperKind::Effect, &layout, &LIVE),
        Some(LATENCY),
        "only the plugins the audio goes through may move the track"
    );

    // A plugin that changes its mind: lookahead switched on after the project
    // is already open. It announces that itself, and the tick is what goes and
    // looks — no activation anywhere in this, which is the whole difficulty.
    wrapper.deactivate();
    {
        let mut state = wrapper.shared().main();
        let deeper = f64::from(LATENCY * 2);
        state
            .host
            .set_sub_param(1, PARAM_LATENCY, deeper)
            .expect("the fixture takes the new latency");
        state
            .host
            .set_sub_param(1, PARAM_ASK, ASK_LATENCY_CHANGED)
            .expect("the fixture announces it");
    }
    wrapper.tick();
    assert_eq!(
        wrapper.shared().latency(),
        LATENCY * 2,
        "a sub-plugin that says its latency moved has to be answered by the          tick: read again, recompiled around, and left as the number the DAW          will be told"
    );

    wrapper.deactivate();
}
