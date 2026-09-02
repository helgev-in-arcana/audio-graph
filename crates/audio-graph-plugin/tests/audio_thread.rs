//! Concurrency and real-time invariants for the audio thread.
//!
//! Routine main-thread operations (drawing the editor, ticking sub-plugin windows,
//! recompiling the graph during editing) must not be able to make the audio thread miss a block.
//! The audio thread's only non-wait-free instruction is `try_lock`ing a superseded graph, and it
//! never hits a system wait. Heavy operations (such as loading or unloading sub-plugins)
//! are permitted to block.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use audio_graph_engine::{
    BlockContext, Engine, Lfo, Math, MathOp, NodeKind, ParamPort, Plugin, PluginPorts, Rate,
    Waveform,
};
use audio_graph_plugin::{SLOT_COUNT, SUB_HOST};
use audio_graph_plugin::{Shared, WrapperParams};
use plugin_host::{AudioConfig, HostContext, RestartReason};
use subhost_adapter::SubHost;

struct SilentHost;
impl HostContext for SilentHost {
    fn host_name(&self) -> &str {
        "audio-thread test"
    }
    fn request_restart(&self, _reason: RestartReason) {}
    fn latency_changed(&self, _samples: u32) {}
    fn param_edited(&self, _id: plugin_host::ParamId, _value: f64) {}
}

/// Helper to add a plugin node with a parameter port to the graph.
///
/// Maps the target parameter to the first lane after the slot table (`SINK_LANE`).
fn param_sink(graph: &mut audio_graph_engine::Graph) -> audio_graph_engine::NodeId {
    graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                params: vec![ParamPort {
                    id: 0,
                    name: "p".into(),
                }],
                ..PluginPorts::default()
            },
        }),
        [200.0, 0.0],
    )
}

/// The lane `param_sink`'s parameter is driven through.
const SINK_LANE: usize = SLOT_COUNT;

fn shared() -> Arc<Shared> {
    Shared::new(
        SubHost::new(Arc::new(SilentHost), SUB_HOST),
        WrapperParams::new(),
    )
}

/// Build the graph the editor builds when someone drops an LFO on the canvas.
fn lfo_into(shared: &Arc<Shared>, rate: f64) {
    let mut state = shared.patch();
    state.graph = audio_graph_engine::Graph::new();
    let lfo = state.graph.add(
        NodeKind::Lfo(Lfo {
            waveform: Waveform::Saw,
            rate: Rate::Hz(rate),
            phase: 0.0,
            depth: 0.5,
            offset: 0.5,
        }),
        [0.0, 0.0],
    );
    let out = param_sink(&mut state.graph);
    state.graph.connect(lfo, 0, out, 0);
}

#[test]
fn editing_the_graph_never_makes_the_audio_thread_miss_a_block() {
    let shared = shared();
    // A configuration, as if the DAW had activated us. Nothing is loaded, so
    // the audio side's lock is uncontended unless somebody puts contention
    // there — which is exactly what is being measured.
    shared.main().config = Some(AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: true,
    });

    let stop = Arc::new(AtomicBool::new(false));
    let edits = Arc::new(AtomicUsize::new(0));

    // The audio thread. It does what `Wrapper::process` does on the paths that
    // matter: take the program if one is waiting, then try the audio lock.
    let audio = {
        let shared = shared.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut engine = Engine::new();
            let mut slots = vec![0.0; SLOT_COUNT];
            let mut blocks = 0usize;
            let mut missed = 0usize;
            let mut adopted = 0usize;

            while !stop.load(Ordering::Relaxed) {
                if engine.adopt(shared.programs()) {
                    adopted += 1;
                }
                match shared.try_audio() {
                    Some(_state) => {
                        engine.run(
                            &BlockContext {
                                sample_rate: 48_000.0,
                                tempo_bpm: 120.0,
                                frames: 32,
                                offset: 0,
                                row: 0,
                            },
                            &mut slots,
                        );
                    }
                    None => missed += 1,
                }
                blocks += 1;
            }
            (blocks, missed, adopted)
        })
    };

    // The main thread, dragging an LFO's rate control. Every frame of that drag
    // is a recompile and a publish.
    for i in 0..20_000 {
        lfo_into(&shared, 0.5 + (i % 100) as f64 * 0.1);
        shared.publish_graph();
        // The editor's tick, which also frees what the audio thread returns.
        shared.reclaim();
        edits.fetch_add(1, Ordering::Relaxed);
    }
    stop.store(true, Ordering::Relaxed);

    let (blocks, missed, adopted) = audio.join().expect("the audio thread panicked");
    assert!(blocks > 0, "the audio thread never ran");
    assert_eq!(
        missed, 0,
        "{missed} of {blocks} blocks were dropped while the graph was being edited; \
         editing must not reach the audio thread's lock at all"
    );
    assert!(
        adopted > 0,
        "the audio thread never picked up any of the {} published programs",
        edits.load(Ordering::Relaxed)
    );
}

#[test]
fn drawing_the_editor_never_reaches_the_audio_lock() {
    // Verifies that routine GUI redraws and editor ticking do not contend on the audio lock.
    let shared = shared();

    let stop = Arc::new(AtomicBool::new(false));
    let audio = {
        let shared = shared.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut missed = 0usize;
            let mut blocks = 0usize;
            while !stop.load(Ordering::Relaxed) {
                if shared.try_audio().is_none() {
                    missed += 1;
                }
                blocks += 1;
            }
            (blocks, missed)
        })
    };

    for _ in 0..200_000 {
        // What the editor does every frame.
        let state = shared.main();
        let _ = state.host.is_loaded(0);
        let _ = state.host.class(0);
        let _ = state.host.slots().slots().len();
        let _ = state.host.capabilities(0);
        drop(state);
        // And what its timer does.
        shared.main().host.tick_editors();
        let _ = shared.live_slots();
    }
    stop.store(true, Ordering::Relaxed);

    let (blocks, missed) = audio.join().expect("the audio thread panicked");
    assert_eq!(
        missed, 0,
        "{missed} of {blocks} blocks lost to the editor drawing itself"
    );
}

#[test]
fn the_old_program_is_freed_on_the_main_thread() {
    // The audio thread must not run destructors; superseded programs are returned
    // to the main thread for reclamation.
    let shared = shared();
    let mut engine = Engine::new();

    lfo_into(&shared, 1.0);
    shared.publish_graph();
    assert!(engine.adopt(shared.programs()));

    for rate in 1..64 {
        lfo_into(&shared, rate as f64);
        shared.publish_graph();
        assert!(engine.adopt(shared.programs()), "rate {rate} never arrived");
    }

    // The return path is four deep and the audio thread declines rather than
    // freeing when it is full, so a main thread that never drains would show up
    // as a swap that stopped happening. It did not, so it drained.
    shared.reclaim();
}

#[test]
fn a_graph_that_drives_nothing_leaves_the_daws_automation_alone() {
    // A new instance starts with audio in wired to audio out and nothing else,
    // so the DAW's automation has to arrive at the slots untouched — the graph
    // carries audio, but it drives no parameter lane.
    let shared = shared();
    shared.publish_graph();

    let mut engine = Engine::new();
    engine.adopt(shared.programs());
    assert!((0..SLOT_COUNT).all(|lane| !engine.drives_lane(lane)));

    let mut slots = vec![0.42; SLOT_COUNT];
    engine.run(
        &BlockContext {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            frames: 32,
            offset: 0,
            row: 0,
        },
        &mut slots,
    );
    assert!(slots.iter().all(|&v| v == 0.42));
}

#[test]
fn a_graph_edit_that_does_not_compile_leaves_the_audio_running() {
    let shared = shared();
    lfo_into(&shared, 2.0);
    shared.publish_graph();

    let mut engine = Engine::new();
    assert!(engine.adopt(shared.programs()));
    assert!(engine.drives_lane(SINK_LANE));

    // Now the user closes a loop — halfway through rearranging something, and
    // not a state worth stopping the music for.
    {
        let mut state = shared.patch();
        let a = state.graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 100.0],
        );
        let b = state.graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 200.0],
        );
        let sink = param_sink(&mut state.graph);
        state.graph.connect(a, 0, b, 0);
        state.graph.connect(b, 0, a, 0);
        state.graph.connect(b, 0, sink, 0);
    }
    shared.publish_graph();

    assert!(shared.patch().compile_error.is_some());
    assert!(
        !engine.adopt(shared.programs()),
        "a failed compile must publish nothing"
    );
    assert!(
        engine.drives_lane(SINK_LANE),
        "the last good program keeps playing"
    );
}

/// Work the editor cannot do itself reaches the main thread and runs there.
///
/// The route only exists because baseview gives the editor a thread of its own
/// on X11: what a button does — loading a plugin, opening a window — has to be
/// carried out where the sub-plugin host is bound. Posting from another thread
/// is the case worth stating, since it is the only one that happens.
#[test]
fn posted_work_runs_on_the_thread_that_drains_it() {
    let shared = shared();
    let ran_on = Arc::new(std::sync::Mutex::new(None));

    let seen = ran_on.clone();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            shared.post_main(move |_| {
                *seen.lock().unwrap() = Some(std::thread::current().id());
            });
        });
    });

    // Nothing runs until the main thread asks for it. A task that ran where it
    // was posted would be the whole bug this exists to prevent.
    assert!(ran_on.lock().unwrap().is_none());

    shared.run_posted();
    assert_eq!(
        *ran_on.lock().unwrap(),
        Some(std::thread::current().id()),
        "the task ran somewhere other than the draining thread"
    );

    // Drained, not merely run: a second turn must not repeat it.
    *ran_on.lock().unwrap() = None;
    shared.run_posted();
    assert!(ran_on.lock().unwrap().is_none());
}
