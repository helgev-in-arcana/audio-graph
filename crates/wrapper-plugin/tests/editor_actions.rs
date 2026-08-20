//! Everything the editor's buttons do, without the editor.
//!
//! The egui layer is a thin shell over [`Shared`]: each control resolves to one
//! call here. Driving those calls directly against a real installed plugin
//! covers the part that can actually be wrong — the load/bind/activate ordering
//! — and leaves only the drawing to be checked by eye.
//!
//! One `#[test]`, deliberately. VST3 pins its objects to the thread that created
//! them, and the test harness runs `#[test]` functions on a pool of threads;
//! splitting this would deadlock rather than fail.

use std::sync::Arc;

use plugin_host_api::{AudioConfig, HostContext, RestartReason};
use subhost_adapter::SubHost;
use wrapper_plugin::{Shared, WrapperParams};

struct SilentHost;

impl HostContext for SilentHost {
    fn host_name(&self) -> &str {
        "editor-actions test"
    }
    fn request_restart(&self, _reason: RestartReason) {}
    fn latency_changed(&self, _samples: u32) {}
    fn param_edited(&self, _id: plugin_host_api::ParamId, _value: f64) {}
}

/// Plugins this test will not instantiate.
///
/// Mostly not a judgement on them — they are simply the wrong shape for an
/// unattended test. Sampler and amp-sim hosts scan multi-gigabyte content
/// libraries or put up an authorisation dialog on their first instantiation,
/// and either one stops the run dead with no output.
///
/// Chroma is here for a sharper reason: it puts a **top-level window** on screen
/// during instantiation, before any host has asked for an editor, and that
/// window stops responding unless somebody pumps the message loop. A host that
/// merely instantiates it — a plugin scanner, this test — hangs. It is the same
/// plugin whose editor faults on first paint (ARCHITECTURE §13), and the two are
/// probably the same underlying assumption: that it is running inside a DAW that
/// is already pumping.
///
/// `AUDIO_GRAPH_TEST_SUB` overrides the search entirely when a specific plugin
/// is wanted.
const AVOID: &[&str] = &[
    "AmpliTube",
    "BBC Symphony",
    "Chroma",
    "Kontakt",
    "MODO",
    "Vienna",
    "Sine",
    "OTT",
];

/// How many modules to try before giving up.
const CANDIDATES: usize = 6;

/// A plugin to test against, chosen from whatever is installed.
///
/// Not a fixed name: this has to run on a machine with a different set of
/// plugins from the one it was written on. Anything with parameters will do.
fn a_plugin_with_parameters() -> Option<(std::path::PathBuf, Arc<Shared>)> {
    let mut tried = 0;
    for path in candidate_paths() {
        if tried >= CANDIDATES {
            break;
        }
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if AVOID.iter().any(|a| name.contains(a)) {
            continue;
        }
        tried += 1;
        eprintln!("trying {name}");

        let params = WrapperParams::new();
        let shared = Shared::new(SubHost::new(Arc::new(SilentHost)), params);
        if shared.load(&path).is_err() {
            continue;
        }
        if !shared.main().host.params(0).is_empty() {
            return Some((path, shared));
        }
    }
    None
}

fn candidate_paths() -> Vec<std::path::PathBuf> {
    // A test thread is not an initialised STA and plugins assume one (§13).
    vst3_host::init_apartment();
    if let Ok(explicit) = std::env::var("AUDIO_GRAPH_TEST_SUB") {
        return vec![std::path::PathBuf::from(explicit)];
    }
    vst3_host::default_plugin_directories()
        .iter()
        .flat_map(|d| vst3_host::find_modules(d))
        .collect()
}

#[test]
fn the_editors_actions_work_against_an_installed_plugin() {
    let Some((path, shared)) = a_plugin_with_parameters() else {
        eprintln!("no installed VST3 with parameters; skipping");
        return;
    };
    eprintln!("driving the editor's actions against {}", path.display());

    // The DAW has activated us, so the editor's later loads have a
    // configuration to activate against.
    shared.main().config = Some(AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: 2,
        output_channels: 2,
        offline: true,
    });

    // "Rescan" — the list the editor draws.
    let installed: usize = vst3_host::default_plugin_directories()
        .iter()
        .map(|d| vst3_host::find_modules(d).len())
        .sum();
    assert!(installed > 0, "the plugin list would be empty");

    // Clicking an entry in that list.
    shared.load(&path).expect("reload from the list");
    assert!(shared.main().host.is_loaded(0));
    assert!(
        shared.audio().processor.is_some(),
        "a load while the DAW is running has to leave the sub-plugin processing; \
         otherwise picking a plugin mid-session silently mutes the track"
    );

    // Clicking a parameter row: bind, then re-activate so the processor picks
    // the new target up.
    let first = shared.main().host.params(0)[0].clone();
    shared.main().host.bind_slot(0, 0, first.id).expect("bind");
    shared.rebind().expect("rebind");
    assert!(
        shared.main().host.slots().resolved(0).is_some(),
        "the binding has to resolve against the plugin it was just made from"
    );
    assert!(
        shared.audio().processor.is_some(),
        "still processing after a rebind"
    );

    // Every edit writes the state back, so the DAW always has something current
    // to save.
    shared.store_state();
    let saved = shared.params().state.0.read().unwrap().clone();
    assert!(
        saved.contains(&first.name),
        "the binding is missing from the saved state"
    );

    // The "Open plugin GUI" button is deliberately *not* exercised here.
    // Opening a real editor needs somebody pumping the Win32 message loop, and
    // a test binary is not pumping one; several plugins simply block. That path
    // already has a harness built for it — `host-cli sweep --gui` runs it across
    // every installed plugin, in both teardown orders, one child process each.

    // "x" on a slot row.
    shared.main().host.slots_mut().clear(0);
    shared.rebind().expect("rebind after clearing");
    assert!(shared.main().host.slots().resolved(0).is_none());

    // "Unload".
    shared.unload();
    assert!(!shared.main().host.is_loaded(0));
    assert!(
        shared.audio().processor.is_none(),
        "unloading has to take the processor with it, or the audio thread keeps \
         a processor whose plugin is gone"
    );
}

/// The other half of what the editor does now: build a graph, publish it, and
/// check the audio thread ends up driving the slot instead of the DAW.
///
/// Needs no plugin: everything from the canvas down to the compiled program is
/// format-agnostic, which is the point of §9.
#[test]
fn a_graph_built_the_way_the_editor_builds_one_drives_a_slot() {
    use wrapper_engine::{BlockContext, Engine, MathOp, NodeKind, Rate, Waveform};

    let params = WrapperParams::new();
    let shared = Shared::new(SubHost::new(Arc::new(SilentHost)), params);

    // Dropping three nodes on the canvas and wiring them up.
    {
        let mut state = shared.main();
        let lfo = state.graph.add(
            NodeKind::Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(2.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            },
            [0.0, 0.0],
        );
        let half = state.graph.add(
            NodeKind::Math {
                op: MathOp::Multiply,
                b: 0.5,
            },
            [200.0, 0.0],
        );
        let out = state.graph.add(NodeKind::SlotOut { slot: 4 }, [400.0, 0.0]);
        state.graph.connect(lfo, 0, half, 0);
        state.graph.connect(half, 0, out, 0);
    }
    shared.publish_graph();
    assert!(
        shared.main().compile_error.is_none(),
        "a valid graph must compile"
    );

    // The audio thread's side of the hand-off.
    let mut engine = Engine::new();
    assert!(
        engine.adopt(shared.programs()),
        "the program has to arrive without a lock"
    );
    assert!(engine.drives(4));
    assert!(!engine.drives(5), "an untouched slot stays the DAW's");

    let mut slots = vec![0.9; subhost_adapter::SLOT_COUNT];
    let mut lowest = f64::INFINITY;
    let mut highest = f64::NEG_INFINITY;
    for _ in 0..2000 {
        slots[4] = 0.9;
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 32,
            },
            &mut slots,
        );
        lowest = lowest.min(slots[4]);
        highest = highest.max(slots[4]);
    }
    assert!(
        highest > 0.45 && lowest < 0.05,
        "the slot should sweep 0..0.5, got {lowest}..{highest}"
    );
    assert_eq!(
        slots[5], 0.9,
        "the graph must not touch a slot it does not drive"
    );

    // A graph the user has broken keeps the working program running.
    {
        let mut state = shared.main();
        let a = state.graph.add(
            NodeKind::Math {
                op: MathOp::Add,
                b: 0.0,
            },
            [0.0, 200.0],
        );
        let b = state.graph.add(
            NodeKind::Math {
                op: MathOp::Add,
                b: 0.0,
            },
            [0.0, 300.0],
        );
        let out = state.graph.add(NodeKind::SlotOut { slot: 6 }, [0.0, 400.0]);
        state.graph.connect(a, 0, b, 0);
        state.graph.connect(b, 0, a, 0);
        state.graph.connect(b, 0, out, 0);
    }
    shared.publish_graph();
    assert!(
        shared.main().compile_error.is_some(),
        "a cycle has to be reported"
    );
    assert!(
        !engine.adopt(shared.programs()),
        "nothing new should have been published"
    );
    assert!(
        engine.drives(4),
        "the last program that compiled keeps running"
    );
}

/// The graph has to survive being saved and reopened, including when the
/// sub-plugin it was built against is not there (§8.3).
#[test]
fn a_graph_survives_the_state_round_trip() {
    use wrapper_engine::{Graph, NodeKind};

    let params = WrapperParams::new();
    let shared = Shared::new(SubHost::new(Arc::new(SilentHost)), params.clone());
    shared.set_quantum(64);
    {
        let mut state = shared.main();
        let c = state
            .graph
            .add(NodeKind::Constant { value: 0.25 }, [10.0, 20.0]);
        let out = state
            .graph
            .add(NodeKind::SlotOut { slot: 2 }, [210.0, 20.0]);
        state.graph.connect(c, 0, out, 0);
    }
    shared.store_state();

    let json = params.state.0.read().unwrap().clone();
    let saved: subhost_adapter::WrapperState = serde_json::from_str(&json).unwrap();
    assert_eq!(saved.sub_block, 64);

    let restored: Graph = serde_json::from_value(saved.graph.expect("a graph was saved")).unwrap();
    assert_eq!(restored, shared.main().graph);
    assert_eq!(
        restored.nodes[0].pos,
        [10.0, 20.0],
        "positions are part of the patch"
    );
    assert!(
        saved.sub_plugin.is_none(),
        "a graph does not need a sub-plugin to be saved"
    );
}
