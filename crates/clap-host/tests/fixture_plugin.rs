//! The CLAP backend, driven end to end against `clap-test-plugin`.
//!
//! Unlike the VST3 backend's tests, this one does not need a plugin to be
//! installed on the machine: the fixture is built from this workspace, so the
//! whole path — module load, factory, instantiate, activate, process, state —
//! is exercised on a bare CI box and can assert on exact sample values.
//!
//! **Everything runs inside one `#[test]` on purpose.** CLAP pins these calls to
//! the thread that created the instance, and the harness runs separate tests on
//! separate threads in parallel; one sequential test is the shape the format
//! permits. It is the same reason `vst3-host/tests/lifecycle.rs` is one test.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

static FIXTURE: Mutex<()> = Mutex::new(());

/// Plugin-requested flushes work without host edits and supersede older activation replays.
#[test]
fn plugin_flush_requests_deliver_values_while_inactive_and_active() {
    struct Host(Mutex<Vec<f64>>);
    impl HostContext for Host {
        fn host_name(&self) -> &str {
            "flush test"
        }
        fn request_restart(&self, _: RestartReason) {}
        fn param_edited(&self, id: ParamId, value: f64) {
            if id == PARAM_GAIN {
                self.0.lock().unwrap().push(value);
            }
        }
    }
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let context = Arc::new(Host(Mutex::new(Vec::new())));
    let mut plugin =
        ClapPlugin::create(&module, "dev.audio-graph.clap-test-plugin", context.clone()).unwrap();
    plugin.set_param(PARAM_GAIN, 1.5).unwrap();
    plugin.set_param(PARAM_ASK, 12.0).unwrap();
    plugin.tick();
    assert_eq!(*context.0.lock().unwrap(), [0.375]);
    plugin.tick();
    assert_eq!(context.0.lock().unwrap().len(), 1);
    plugin.set_param(PARAM_ASK, 0.0).unwrap();
    let mut processor = plugin.activate(lifecycle_config()).unwrap();
    let input = [1.0; 8];
    let mut output = [0.0; 8];
    let mut sink = EventSink::with_capacity(8);
    let mut run = || {
        let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
        assert_eq!(
            processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink),
            ProcessStatus::Continue
        );
        output[0]
    };
    assert_eq!(run(), 0.375);
    plugin.set_param(PARAM_GAIN, 1.0).unwrap();
    plugin.set_param(PARAM_ASK, 12.0).unwrap();
    assert_eq!(run(), 1.0);
    plugin.tick();
    plugin.tick();
    assert_eq!(run(), 0.375);
    assert!(matches!(
        sink.events(),
        [Event::Param(ParamEvent::SetValue { value: 0.375, .. })]
    ));
}

mod allocations {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    thread_local! { static COUNTS: Cell<Option<(usize, usize)>> = const { Cell::new(None) }; }
    struct Allocator;
    #[global_allocator]
    static ALLOCATOR: Allocator = Allocator;
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = COUNTS.try_with(|c| {
                if let Some((a, d)) = c.get() {
                    c.set(Some((a + 1, d)));
                }
            });
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            let _ = COUNTS.try_with(|c| {
                if let Some((a, d)) = c.get() {
                    c.set(Some((a, d + 1)));
                }
            });
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    /// Filling a native input batch performs no host allocations or frees on the audio thread.
    #[test]
    fn a_full_native_input_batch_does_not_allocate() {
        let _fixture = FIXTURE.lock().unwrap();
        let module = Module::open(fixture_path()).unwrap();
        let mut plugin = ClapPlugin::create(
            &module,
            "dev.audio-graph.clap-test-plugin",
            Arc::new(TestHost),
        )
        .unwrap();
        let mut processor = plugin.activate(lifecycle_config()).unwrap();
        let events = vec![
            Event::Param(ParamEvent::SetValue {
                id: PARAM_GAIN,
                target: Target::Global,
                value: 1.0,
                sample_offset: 0
            });
            2048
        ];
        let mut sink = EventSink::with_capacity(8);
        let input = [1.0; 8];
        let mut output = [0.0; 8];
        let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
        COUNTS.with(|c| c.set(Some((0, 0))));
        let status = processor.process(&mut buffers, &events, &TimeContext::default(), &mut sink);
        let counts = COUNTS.with(|c| c.replace(None)).unwrap();
        assert_eq!(status, ProcessStatus::Continue);
        assert_eq!(counts, (0, 0));
    }
}

/// Rejected input cannot partially update native state or consume queued main-thread edits.
#[test]
fn input_overflow_is_rejected_before_native_delivery() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let mut plugin = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    let mut processor = plugin.activate(lifecycle_config()).unwrap();
    let input = [1.0; 8];
    let mut output = [9.0; 8];
    let mut sink = EventSink::with_capacity(8);
    plugin.set_param(PARAM_GAIN, 0.5).unwrap();
    let mut events = vec![
        Event::Param(ParamEvent::SetValue {
            id: PARAM_GAIN,
            target: Target::Global,
            value: 0.0,
            sample_offset: 0
        });
        2048
    ];
    events.push(Event::Note(NoteEvent::NoteOff {
        note_id: Some(7),
        port: 0,
        channel: 0,
        key: 60,
        velocity: 0.0,
        sample_offset: 0,
    }));
    let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
    assert_eq!(
        processor.process(&mut buffers, &events, &TimeContext::default(), &mut sink),
        ProcessStatus::Error
    );
    assert_eq!(output, [0.0; 8]);
    let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
    assert_eq!(
        processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink),
        ProcessStatus::Continue
    );
    assert_eq!(output, [0.5; 8]);
}

use clap_host::{ClapPlugin, Module};
use plugin_host_api::{
    AudioBuffers, AudioConfig, AuxBuses, BufferLayout, Event, EventSink, HostContext, NoteEvent,
    ParamEvent, ParamFlags, ParamId, ProcessStatus, RestartReason, SubPluginMain,
    SubPluginProcessor, Target, TimeContext,
};

/// Mirrors the fixture's own constants; a drift between the two should fail the
/// test rather than be papered over by importing them.
const SIDECHAIN_GAIN: f32 = 0.5;
const NOTE_LEVEL: f32 = 0.25;
const PARAM_GAIN: ParamId = ParamId(0);
const PARAM_OFFSET: ParamId = ParamId(1);
const PARAM_LATENCY: ParamId = ParamId(3);
const PARAM_ACTIVE_PORTS: ParamId = ParamId(4);
/// Mirrors `clap_test_plugin::OUTPUT_PORT_BIT`, spelled out here so the test
/// reads without a second file open.
const OUTPUT_PORT_BIT: u32 = 8;
const PARAM_ASK: ParamId = ParamId(5);
const PARAM_RENDER_MODE: ParamId = ParamId(6);
#[cfg(all(unix, not(target_os = "macos")))]
const PARAM_FD_CALLS: ParamId = ParamId(7);
/// Mirrors `clap_test_plugin::ask`, spelled out for the same reason as
/// `OUTPUT_PORT_BIT`: this crate does not depend on the fixture's Rust API,
/// only on the module it builds.
const ASK_RESTART: f64 = 1.0;
const ASK_AUDIO_PORTS_RESCAN: f64 = 2.0;
const ASK_NOTE_PORTS_RESCAN: f64 = 3.0;
const ASK_LATENCY_CHANGED: f64 = 4.0;
/// What the fixture is told to claim before it says its latency moved.
const NEW_LATENCY: u32 = 128;

#[derive(Default)]
struct TestHost;

struct LifetimeHost(Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>>);

impl HostContext for LifetimeHost {
    fn host_name(&self) -> &str {
        "lifetime test"
    }

    fn request_restart(&self, _reason: RestartReason) {}
}

impl Drop for LifetimeHost {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(std::thread::current().id());
    }
}

fn lifecycle_config() -> AudioConfig {
    AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 32,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: AuxBuses::default(),
        aux_outputs: AuxBuses::default(),
        offline: true,
    }
}

/// Rejected blocks cannot apply native parameter edits or write beyond their declared output.
#[test]
fn mismatched_blocks_never_enter_native_processing() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let mut plugin = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    let config = lifecycle_config();
    assert!(
        plugin
            .activate(AudioConfig {
                sample_rate: f64::NAN,
                ..config
            })
            .is_err()
    );
    let mut processor = plugin.activate(config).unwrap();
    let mut sink = EventSink::with_capacity(8);
    let input = [0.5; 128];
    for (channels, frames, layout, aux) in [
        (1, 4, BufferLayout::Planar, AuxBuses::default()),
        (2, 4, BufferLayout::Interleaved, AuxBuses::default()),
        (2, 4, BufferLayout::Planar, AuxBuses::new(&[1])),
        (2, 33, BufferLayout::Planar, AuxBuses::default()),
    ] {
        let mut output = [9.0; 128];
        let mut buffers =
            AudioBuffers::new(&input, &mut output, channels, channels, frames, layout)
                .with_aux_inputs(aux);
        let event = Event::Param(ParamEvent::SetValue {
            id: PARAM_GAIN,
            target: Target::Global,
            value: 0.0,
            sample_offset: 0,
        });
        assert_eq!(
            processor.process(&mut buffers, &[event], &TimeContext::default(), &mut sink),
            ProcessStatus::Error
        );
        let used = (channels * frames) as usize;
        assert!(output[..used].iter().all(|&v| v == 0.0));
        assert!(output[used..].iter().all(|&v| v == 9.0));
    }
    let mut output = [0.0; 8];
    let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
    assert_eq!(
        processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink),
        ProcessStatus::Continue
    );
    assert_eq!(
        output, [0.5; 8],
        "rejected edits cannot reach the native gain"
    );
}

/// Both native scratch loss and caller capacity loss remain visible across process calls.
#[test]
fn output_overflow_is_propagated_and_not_cleared_by_processing() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let mut plugin = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    let mut processor = plugin.activate(lifecycle_config()).unwrap();
    let input = [0.0; 8];
    let mut output = [0.0; 8];
    let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
    let burst = Event::Param(ParamEvent::SetValue {
        id: PARAM_ASK,
        target: Target::Global,
        value: 5.0,
        sample_offset: 0,
    });
    for capacity in [0, 1, 4096] {
        let mut sink = EventSink::with_capacity(capacity);
        processor.process(&mut buffers, &[burst], &TimeContext::default(), &mut sink);
        assert!(sink.overflowed());
        let retained = sink.events().len();
        assert_eq!(retained, capacity.min(2048));
        processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink);
        assert!(sink.overflowed());
        assert_eq!(sink.events().len(), retained);
        sink.clear();
        processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink);
        assert!(!sink.overflowed());
    }
}

/// Native completion identifies the input note, independently of output note ports.
#[test]
fn clap_note_ports_report_native_completion() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let mut plugin = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    assert_eq!(plugin.note_end_ports(), [0]);
    let mut processor = plugin.activate(lifecycle_config()).unwrap();
    let mut output = [0.0; 8];
    let mut buffers = AudioBuffers::new(&[0.0; 8], &mut output, 2, 2, 4, BufferLayout::Planar);
    let mut sink = EventSink::with_capacity(2);
    let on = Event::Note(NoteEvent::NoteOn {
        note_id: Some(17),
        port: 0,
        channel: 0,
        key: 60,
        velocity: 1.0,
        sample_offset: 0,
    });
    processor.process(&mut buffers, &[on], &TimeContext::default(), &mut sink);
    assert!(sink.is_empty());
    let off = Event::Note(NoteEvent::NoteOff {
        note_id: Some(17),
        port: 0,
        channel: 0,
        key: 60,
        velocity: 0.0,
        sample_offset: 2,
    });
    processor.process(&mut buffers, &[off], &TimeContext::default(), &mut sink);
    assert!(matches!(
        sink.events(),
        [Event::Note(NoteEvent::NoteEnd {
            note_id: Some(17),
            sample_offset: 2,
            ..
        })]
    ));
}

/// A running processor retains its instance, module, and callbacks after main is dropped.
#[test]
fn the_processor_outlives_main_and_returns_to_its_owner() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = Arc::new(LifetimeHost(drops.clone()));
    let weak = Arc::downgrade(&context);
    let mut plugin =
        ClapPlugin::create(&module, "dev.audio-graph.clap-test-plugin", context).unwrap();
    let mut processor = plugin.activate(lifecycle_config()).unwrap();
    drop(plugin);
    drop(module);
    assert!(
        weak.upgrade().is_some(),
        "the active instance retains its callbacks"
    );

    std::thread::spawn(move || {
        let input = [0.5; 64];
        let mut output = [0.0; 64];
        let mut sink = EventSink::new();
        let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 32, BufferLayout::Planar);
        assert_eq!(
            processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink),
            ProcessStatus::Continue
        );
        assert!(output.iter().all(|&sample| sample == 0.5));
        processor.deactivate();
    })
    .join()
    .unwrap();

    assert!(
        drops.lock().unwrap().is_empty(),
        "the audio thread cannot destroy callbacks"
    );
    plugin_host_api::reclaim_main_thread();
    assert!(
        weak.upgrade().is_none(),
        "the owner reclaims the complete instance"
    );
    assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
}

/// Returning one activation leaves another instance active and permits its own next activation.
#[test]
fn processor_returns_are_bound_to_their_own_activation() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let mut first = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    let mut second = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    let first_processor = first.activate(lifecycle_config()).unwrap();
    let second_processor = second.activate(lifecycle_config()).unwrap();
    assert!(first.activate(lifecycle_config()).is_err());
    assert!(
        first.load_state(&[]).is_err(),
        "state cannot replace an active configuration"
    );
    std::thread::spawn(move || drop(first_processor))
        .join()
        .unwrap();
    let restarted = first.activate(lifecycle_config()).unwrap();
    assert!(
        second.activate(lifecycle_config()).is_err(),
        "returning first cannot stop second"
    );
    restarted.deactivate();
    second_processor.deactivate();
}

impl HostContext for TestHost {
    fn host_name(&self) -> &str {
        "clap-host tests"
    }
    fn request_restart(&self, _reason: RestartReason) {}
}

/// A host that writes down what it was asked for instead of ignoring it.
#[derive(Default)]
struct RecordingHost {
    reasons: std::sync::Mutex<Vec<RestartReason>>,
    latencies: std::sync::Mutex<Vec<u32>>,
    threads: std::sync::Mutex<Vec<std::thread::ThreadId>>,
}

impl HostContext for RecordingHost {
    fn host_name(&self) -> &str {
        "clap-host tests"
    }
    fn request_restart(&self, reason: RestartReason) {
        self.reasons.lock().expect("not poisoned").push(reason);
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
    fn latency_changed(&self, samples: u32) {
        self.latencies.lock().expect("not poisoned").push(samples);
    }
}

/// Repeated audio-thread requests reach HostContext once, on a headless main-thread tick.
#[test]
fn audio_requests_are_delivered_only_by_the_main_thread() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let host = Arc::new(RecordingHost::default());
    let mut plugin =
        ClapPlugin::create(&module, "dev.audio-graph.clap-test-plugin", host.clone()).unwrap();
    let mut processor = plugin.activate(lifecycle_config()).unwrap();
    let processor = std::thread::spawn(move || {
        let mut output = [0.0; 8];
        let mut buffers = AudioBuffers::new(&[0.0; 8], &mut output, 2, 2, 4, BufferLayout::Planar);
        let request = Event::Param(ParamEvent::SetValue {
            id: PARAM_ASK,
            target: Target::Global,
            value: ASK_RESTART,
            sample_offset: 0,
        });
        for _ in 0..2 {
            processor.process(
                &mut buffers,
                &[request],
                &TimeContext::default(),
                &mut EventSink::new(),
            );
        }
        processor
    })
    .join()
    .unwrap();
    assert!(host.reasons.lock().unwrap().is_empty());
    plugin.tick();
    assert_eq!(*host.reasons.lock().unwrap(), [RestartReason::IoConfig]);
    assert_eq!(*host.threads.lock().unwrap(), [std::thread::current().id()]);
    processor.deactivate();
    plugin.tick();
    drop(plugin);
    assert_eq!(host.reasons.lock().unwrap().len(), 1);
}

/// A request raised during a native main callback remains pending for the next tick.
#[test]
fn callback_requests_are_not_lost_while_servicing() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).unwrap();
    let host = Arc::new(RecordingHost::default());
    let mut plugin =
        ClapPlugin::create(&module, "dev.audio-graph.clap-test-plugin", host.clone()).unwrap();
    plugin.set_param(PARAM_ASK, 6.0).unwrap();
    plugin.tick();
    assert!(host.reasons.lock().unwrap().is_empty());
    plugin.tick();
    assert_eq!(*host.reasons.lock().unwrap(), [RestartReason::IoConfig]);
    plugin.tick();
    assert_eq!(host.reasons.lock().unwrap().len(), 1);
}

/// Descriptors change atomically, and only structural changes require giving back the processor.
#[test]
fn metadata_refresh_obeys_activation_and_preserves_failed_requests() {
    let _fixture = FIXTURE.lock().unwrap();
    use plugin_host_api::MetadataUpdate;
    let module = Module::open(fixture_path()).unwrap();
    let mut plugin = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::new(TestHost),
    )
    .unwrap();
    let config = lifecycle_config();
    let mut processor = plugin.activate(config).unwrap();
    let mut send = |value| {
        let mut output = [0.0; 8];
        let mut buffers = AudioBuffers::new(&[0.0; 8], &mut output, 2, 2, 4, BufferLayout::Planar);
        let event = Event::Param(ParamEvent::SetValue {
            id: PARAM_ASK,
            target: Target::Global,
            value,
            sample_offset: 0,
        });
        assert_eq!(
            processor.process(
                &mut buffers,
                &[event],
                &TimeContext::default(),
                &mut EventSink::new()
            ),
            ProcessStatus::Continue
        );
    };
    send(8.0);
    plugin.tick();
    assert_eq!(
        plugin.refresh_metadata().unwrap(),
        MetadataUpdate::Refreshed
    );
    assert_eq!(plugin.params()[0].name, "Level");
    send(7.0);
    assert_eq!(
        plugin.refresh_metadata().unwrap(),
        MetadataUpdate::NeedsDeactivation
    );
    assert_eq!(plugin.io_layout().main_input_channels(), 2);
    assert_eq!(plugin.params().len(), 8);
    processor.deactivate();
    assert_eq!(
        plugin.refresh_metadata().unwrap(),
        MetadataUpdate::Refreshed
    );
    assert_eq!(plugin.io_layout().main_input_channels(), 1);
    assert_eq!(plugin.params().len(), 7);
    assert_eq!(plugin.params()[0].max, 4.0);
    assert!(!plugin.params().iter().any(|p| p.id == PARAM_OFFSET));
    plugin
        .activate(AudioConfig {
            input_channels: 1,
            output_channels: 1,
            ..config
        })
        .unwrap()
        .deactivate();
    let params = plugin.params().to_vec();
    plugin.set_param(PARAM_ASK, 9.0).unwrap();
    assert!(plugin.refresh_metadata().is_err());
    assert_eq!(plugin.params(), params);
    assert!(plugin.activate(config).is_err());
    plugin.set_param(PARAM_ASK, 8.0).unwrap();
    assert_eq!(
        plugin.refresh_metadata().unwrap(),
        MetadataUpdate::Refreshed
    );
    plugin.set_param(PARAM_ASK, 10.0).unwrap();
    assert!(plugin.refresh_metadata().is_err());
    assert_eq!(
        plugin.refresh_metadata().unwrap(),
        MetadataUpdate::Refreshed
    );
    assert_eq!(
        plugin.refresh_metadata().unwrap(),
        MetadataUpdate::Unchanged
    );
    let mut state = plugin.save_state().unwrap();
    state[16..24].copy_from_slice(&0.0f64.to_le_bytes());
    plugin.load_state(&state).unwrap();
    assert_eq!(plugin.params().len(), 8);
    assert_eq!(plugin.io_layout().main_input_channels(), 2);
}

/// The plugin asks; the host has to hear it.
///
/// `request_restart`, `audio-ports.rescan` and `note-ports.rescan` are the
/// three calls a plugin makes when its own shape changes, and none of them is
/// reachable by driving the host — only the plugin can start them. No plugin on
/// this machine makes any of the three (Surge XT Effects gets as far as the
/// latency and parameter notifications and no further), which is why the
/// fixture has a parameter whose whole job is to make the call.
#[test]
fn the_host_forwards_what_the_plugin_asks_for() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).expect("the fixture opens");

    for (ask, expected) in [
        (ASK_RESTART, RestartReason::IoConfig),
        (ASK_AUDIO_PORTS_RESCAN, RestartReason::IoConfig),
        (ASK_NOTE_PORTS_RESCAN, RestartReason::IoConfig),
        (ASK_LATENCY_CHANGED, RestartReason::Latency),
    ] {
        let host = Arc::new(RecordingHost::default());
        let mut plugin = ClapPlugin::create(
            &module,
            "dev.audio-graph.clap-test-plugin",
            Arc::clone(&host) as Arc<dyn HostContext>,
        )
        .expect("instantiates");

        assert!(
            host.reasons.lock().unwrap().is_empty(),
            "nothing asked for yet"
        );
        // Written while inactive, so the value reaches the plugin through
        // `params.flush` — main thread, which is where all three calls are
        // legal.
        SubPluginMain::set_param(&mut plugin, PARAM_ASK, ask).expect("the ask lands");
        assert!(host.reasons.lock().unwrap().is_empty());
        plugin.tick();

        let seen = host.reasons.lock().unwrap().clone();
        if expected == RestartReason::Latency {
            assert_eq!(*host.latencies.lock().unwrap(), [0]);
        } else {
            assert_eq!(seen, vec![expected], "ask {ask} was not forwarded");
        }
    }
}

/// A plugin that changes its latency mid-session is asked again, and the new
/// number reaches the host.
///
/// Saying "it moved" is only half of it: the number itself is read back on the
/// main-thread tick, and a host that records the request without ever
/// re-reading leaves whatever it learned at activate in place — which is
/// exactly the stale figure the plugin was trying to correct.
#[test]
fn a_latency_that_moves_is_read_back() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).expect("the fixture opens");
    let host = Arc::new(RecordingHost::default());
    let mut plugin = ClapPlugin::create(
        &module,
        "dev.audio-graph.clap-test-plugin",
        Arc::clone(&host) as Arc<dyn HostContext>,
    )
    .expect("instantiates");

    assert_eq!(SubPluginMain::latency_samples(&plugin), 0);

    // The plugin decides it needs lookahead, and says so.
    SubPluginMain::set_param(&mut plugin, PARAM_LATENCY, f64::from(NEW_LATENCY))
        .expect("the latency lands");
    SubPluginMain::set_param(&mut plugin, PARAM_ASK, ASK_LATENCY_CHANGED).expect("the ask lands");
    assert!(
        host.latencies.lock().unwrap().is_empty(),
        "the number is read on the tick, not from inside the plugin's own call"
    );

    plugin.tick();
    assert_eq!(
        SubPluginMain::latency_samples(&plugin),
        NEW_LATENCY,
        "the host went on answering for a latency the plugin no longer has"
    );
    assert_eq!(
        *host.latencies.lock().unwrap(),
        vec![NEW_LATENCY],
        "whoever is hosting this host has to be told once, and once only"
    );

    // Nothing new to say on the next turn: a number that has not moved must not
    // be announced again, or every tick restarts the DAW's processing.
    plugin.tick();
    assert_eq!(*host.latencies.lock().unwrap(), vec![NEW_LATENCY]);
}

/// The host has to actually poll the descriptor a plugin registers.
///
/// Linux only, because that is the only platform where the extension exists.
/// The fixture registers a pipe at `init` and leaves a byte in it, so a host
/// that polls calls `on_fd`, the fixture drains and re-arms, and the count it
/// reports keeps rising. A host that answers `register_fd` and then never looks
/// leaves the count at zero — which is the failure this catches, since nothing
/// else about the plugin would look any different.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn the_host_polls_the_descriptors_a_plugin_registers() {
    let _fixture = FIXTURE.lock().unwrap();
    let module = Module::open(fixture_path()).expect("the fixture opens");
    let context: Arc<dyn HostContext> = Arc::new(TestHost);
    let mut plugin = ClapPlugin::create(&module, "dev.audio-graph.clap-test-plugin", context)
        .expect("instantiates");

    assert_eq!(
        SubPluginMain::snapshot(&plugin).get(PARAM_FD_CALLS),
        Some(0.0),
        "nothing has been polled yet"
    );

    plugin.tick();
    let after_one = SubPluginMain::snapshot(&plugin)
        .get(PARAM_FD_CALLS)
        .expect("fd calls");
    assert!(
        after_one >= 1.0,
        "the descriptor was registered and never polled"
    );

    plugin.tick();
    let after_two = SubPluginMain::snapshot(&plugin)
        .get(PARAM_FD_CALLS)
        .expect("fd calls");
    assert!(
        after_two > after_one,
        "polling stopped after one turn: {after_one} then {after_two}"
    );
}

/// Where `cargo` put the fixture's shared library.
///
/// A `.clap` on Windows and Linux *is* the shared library, so the artifact is
/// loadable as it stands and nothing has to be copied or renamed.
///
/// **Panics rather than skipping when it is missing:** cargo does not build another
/// package's `cdylib` on its own.
pub fn fixture_path() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    // .../target/<profile>/deps/<test>.exe
    let build_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the test binary is two levels below the build directory");
    let names = [
        "clap_test_plugin.dll",
        "libclap_test_plugin.so",
        "libclap_test_plugin.dylib",
    ];
    names
        .iter()
        .map(|n| build_dir.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| {
            panic!(
                "clap-test-plugin is not in {}.\n\
                 Run `cargo build --workspace` before `cargo test --workspace`.",
                build_dir.display()
            )
        })
}

#[test]
fn the_backend_drives_a_real_clap_module() {
    let _fixture = FIXTURE.lock().unwrap();
    let path = fixture_path();

    // --- module and factory ------------------------------------------------

    let module = Module::open(&path).expect("the fixture loads");
    let classes = module.classes().expect("the factory enumerates");
    assert_eq!(classes.len(), 1, "the fixture exports one plugin");
    let class = classes[0].clone();
    assert_eq!(class.id, "dev.audio-graph.clap-test-plugin");
    assert!(class.features.iter().any(|f| f == "audio-effect"));
    assert!(!class.is_instrument(), "the fixture is an effect");

    // Verify that a second handle to the same module path does not re-initialize the entry point.
    {
        let again = Module::open(&path).expect("the same module opens twice");
        assert_eq!(again.classes().unwrap().len(), 1);
    }

    let context: Arc<dyn HostContext> = Arc::new(TestHost);

    // --- parameters --------------------------------------------------------

    let mut plugin =
        ClapPlugin::create(&module, &class.id, Arc::clone(&context)).expect("instantiates");

    let params = SubPluginMain::params(&plugin).to_vec();
    assert_eq!(params.len(), 8, "{params:#?}");

    let gain = params.iter().find(|p| p.id == PARAM_GAIN).expect("gain");
    // Verify raw parameter range as reported by the plugin.
    assert_eq!((gain.min, gain.max, gain.default), (0.0, 2.0, 1.0));
    assert_eq!(gain.name, "Gain");
    assert!(gain.flags.contains(ParamFlags::AUTOMATABLE));
    assert!(gain.flags.contains(ParamFlags::MODULATABLE));
    assert!(!gain.flags.contains(ParamFlags::POLY_MODULATABLE));

    let offset = params
        .iter()
        .find(|p| p.id == PARAM_OFFSET)
        .expect("offset");
    assert_eq!(offset.module, "Tone", "the module path is read");
    assert!(offset.flags.contains(ParamFlags::POLY_MODULATABLE));

    let mode = params.iter().find(|p| p.id == ParamId(2)).expect("mode");
    assert!(mode.flags.contains(ParamFlags::STEPPED));

    let caps = SubPluginMain::capabilities(&plugin);
    assert!(caps.modulation, "CLAP has non-destructive modulation");
    assert!(caps.poly_modulation, "one parameter declares per-note mod");
    assert!(
        caps.note_expression,
        "the note port speaks the CLAP dialect"
    );

    // Formatting is delegated to the plugin.
    assert_eq!(
        SubPluginMain::param_to_text(&plugin, PARAM_GAIN, 1.5).as_deref(),
        Some("1.50 x")
    );
    assert_eq!(
        SubPluginMain::param_to_text(&plugin, ParamId(2), 1.0).as_deref(),
        Some("Half")
    );
    assert_eq!(
        SubPluginMain::param_from_text(&plugin, PARAM_GAIN, "1.75 x"),
        Some(1.75)
    );

    // --- I/O layout --------------------------------------------------------

    let io = SubPluginMain::io_layout(&plugin);
    assert_eq!(io.inputs.len(), 2, "main plus sidechain");
    assert_eq!(io.inputs[0].name, "Main");
    assert!(!io.inputs[0].is_aux);
    assert_eq!(io.inputs[1].name, "Sidechain");
    assert!(io.inputs[1].is_aux, "the sidechain is an auxiliary socket");
    assert_eq!(io.aux_inputs().len(), 1);
    assert_eq!(io.outputs.len(), 2, "main plus auxiliary output");
    assert_eq!(io.outputs[1].name, "Aux Out");
    assert!(io.outputs[1].is_aux);
    assert_eq!(io.main_input_channels(), 2);
    assert!(io.accepts_notes);
    assert!(!io.emits_notes);

    // --- a main-thread edit reaches the plugin while inactive ---------------

    SubPluginMain::set_param(&mut plugin, PARAM_GAIN, 1.5).expect("set gain");
    assert_eq!(
        SubPluginMain::snapshot(&plugin).get(PARAM_GAIN),
        Some(1.5),
        "an inactive edit has to flush, not wait for a block that never comes"
    );

    // --- state round trip --------------------------------------------------

    SubPluginMain::set_param(&mut plugin, PARAM_OFFSET, 0.125).expect("set offset");
    let saved = SubPluginMain::save_state(&plugin).expect("save");
    assert!(!saved.is_empty());

    SubPluginMain::set_param(&mut plugin, PARAM_GAIN, 0.0).expect("clobber");
    SubPluginMain::set_param(&mut plugin, PARAM_OFFSET, -1.0).expect("clobber");
    SubPluginMain::load_state(&mut plugin, &saved).expect("load");
    let restored = SubPluginMain::snapshot(&plugin);
    assert_eq!(restored.get(PARAM_GAIN), Some(1.5));
    assert_eq!(restored.get(PARAM_OFFSET), Some(0.125));

    // A truncated blob has to be refused rather than half-applied.
    assert!(SubPluginMain::load_state(&mut plugin, &saved[..4]).is_err());
    assert!(plugin.activate(lifecycle_config()).is_err());
    plugin.refresh_metadata().unwrap();

    // --- latency -----------------------------------------------------------

    SubPluginMain::set_param(&mut plugin, PARAM_LATENCY, 64.0).expect("set latency");
    SubPluginMain::set_param(&mut plugin, PARAM_GAIN, 1.0).expect("reset gain");
    SubPluginMain::set_param(&mut plugin, PARAM_OFFSET, 0.0).expect("reset offset");

    // --- processing, without a sidechain -----------------------------------

    const FRAMES: u32 = 64;
    let config = AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: FRAMES,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: AuxBuses::default(),
        aux_outputs: AuxBuses::default(),
        offline: true,
    };

    let mut processor = SubPluginMain::activate(&mut plugin, config).expect("activates");
    assert_eq!(
        SubPluginMain::latency_samples(&plugin),
        64,
        "latency is read once the plugin is set up"
    );

    let input = vec![0.5f32; (FRAMES * 2) as usize];
    let mut output = vec![-99.0f32; (FRAMES * 2) as usize];
    let context_time = TimeContext::default();
    let mut sink = EventSink::new();

    {
        let mut buffers =
            AudioBuffers::new(&input, &mut output, 2, 2, FRAMES, BufferLayout::Planar);
        let status = processor.process(&mut buffers, &[], &context_time, &mut sink);
        assert_eq!(status, ProcessStatus::Continue);
    }
    // out = in * gain. Verify that an unwired sidechain input remains silent.
    assert!(
        output.iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "unwired sidechain leaked: {:?}",
        &output[..4]
    );

    // --- the unwired port was switched off, not merely fed silence ---------

    // The fixture reports what it was told through a read-only parameter, and
    // refuses a `set_active` made at the wrong moment or with the wrong sample
    // size — so this failing means the call was wrong, not just absent.
    let mask = SubPluginMain::snapshot(&plugin)
        .get(PARAM_ACTIVE_PORTS)
        .expect("the fixture reports its active ports") as u32;
    assert_eq!(mask & 1, 1, "the main input stays on");
    assert_eq!(mask & 2, 0, "the unwired sidechain should be off");
    assert_eq!(
        mask & (1 << OUTPUT_PORT_BIT),
        1 << OUTPUT_PORT_BIT,
        "the wired output stays on"
    );

    // --- what the plugin says about its voices ------------------------------

    // Mirrors `clap_test_plugin::VOICE_COUNT` / `VOICE_CAPACITY`: two different
    // numbers, so a backend that reported one field twice would fail here.
    let voices = SubPluginMain::voice_info(&plugin).expect("the fixture implements voice-info");
    assert_eq!(voices.count, 3);
    assert_eq!(voices.capacity, 7);
    assert!(voices.overlapping_notes);

    // --- the second output bus is its own signal ---------------------------

    // Nothing asked for the aux output in the config above, so the plugin
    // wrote it into the backend's scratch and the caller's region is
    // untouched. Ask for it and it arrives, packed after the main bus the same
    // way an aux *input* is packed after the main one.
    processor.deactivate();
    let two_out = AudioConfig {
        aux_outputs: AuxBuses::new(&[2]),
        ..config
    };
    let mut processor =
        SubPluginMain::activate(&mut plugin, two_out).expect("activates with an aux output");
    let mut wide = vec![-99.0f32; (FRAMES * 4) as usize];
    {
        let mut buffers = AudioBuffers::new(&input, &mut wide, 2, 4, FRAMES, BufferLayout::Planar)
            .with_aux_outputs(AuxBuses::new(&[2]));
        processor.process(&mut buffers, &[], &context_time, &mut sink);
    }
    let (main_region, aux_region) = wide.split_at((FRAMES * 2) as usize);
    // Mirrors `clap_test_plugin::AUX_OUTPUT_GAIN`.
    for (i, (&m, &a)) in main_region.iter().zip(aux_region).enumerate() {
        assert!(
            (a - m * -0.75).abs() < 1e-6,
            "frame {i}: aux {a} is not the main bus {m} times -0.75"
        );
    }
    assert!(
        main_region.iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "the main bus changed when a second one was asked for: {:?}",
        &main_region[..4]
    );
    processor.deactivate();
    let mut processor = SubPluginMain::activate(&mut plugin, config).expect("activates");

    // --- the plugin was told this is an offline render ---------------------

    // `AudioConfig::offline` is the only thing that says so, and CLAP's only
    // way to pass it on is `clap.render`. The fixture refuses a mode it does
    // not recognise, so a wrong value fails here rather than being stored.
    assert_eq!(
        SubPluginMain::snapshot(&plugin).get(PARAM_RENDER_MODE),
        Some(1.0),
        "an offline config has to reach the plugin as offline render mode"
    );

    // --- parameter events arrive as events ---------------------------------

    let events = [Event::Param(ParamEvent::SetValue {
        id: PARAM_GAIN,
        target: Target::Global,
        value: 2.0,
        sample_offset: 0,
    })];
    {
        let mut buffers =
            AudioBuffers::new(&input, &mut output, 2, 2, FRAMES, BufferLayout::Planar);
        processor.process(&mut buffers, &events, &context_time, &mut sink);
    }
    assert!(
        output.iter().all(|&s| (s - 1.0).abs() < 1e-6),
        "the parameter event did not land: {:?}",
        &output[..4]
    );

    // --- notes -------------------------------------------------------------

    let note_on = [Event::Note(NoteEvent::NoteOn {
        note_id: Some(1),
        port: 0,
        channel: 0,
        key: 60,
        velocity: 1.0,
        sample_offset: 0,
    })];
    {
        let mut buffers =
            AudioBuffers::new(&input, &mut output, 2, 2, FRAMES, BufferLayout::Planar);
        processor.process(&mut buffers, &note_on, &context_time, &mut sink);
    }
    let with_note = 0.5 * 2.0 + NOTE_LEVEL;
    assert!(
        output.iter().all(|&s| (s - with_note).abs() < 1e-6),
        "the note did not reach the plugin: {:?}",
        &output[..4]
    );

    // `reset` has to drop the held note, which is the whole of what it means
    // for this fixture.
    processor.reset();
    {
        let mut buffers =
            AudioBuffers::new(&input, &mut output, 2, 2, FRAMES, BufferLayout::Planar);
        processor.process(&mut buffers, &[], &context_time, &mut sink);
    }
    assert!(
        output.iter().all(|&s| (s - 1.0).abs() < 1e-6),
        "reset left the note held: {:?}",
        &output[..4]
    );

    processor.deactivate();

    // --- processing, with a sidechain --------------------------------------

    let config = AudioConfig {
        aux_inputs: AuxBuses::new(&[2]),
        ..config
    };
    let mut processor = SubPluginMain::activate(&mut plugin, config).expect("activates with aux");

    // The input region is main-then-aux, packed.
    let mut input = vec![0.0f32; (FRAMES * 4) as usize];
    input[..(FRAMES * 2) as usize].fill(0.5);
    input[(FRAMES * 2) as usize..].fill(1.0);
    {
        let mut buffers =
            AudioBuffers::new(&input, &mut output, 4, 2, FRAMES, BufferLayout::Planar)
                .with_aux_inputs(AuxBuses::new(&[2]));
        processor.process(&mut buffers, &[], &context_time, &mut sink);
    }
    let expected = 0.5 * 2.0 + 1.0 * SIDECHAIN_GAIN;
    assert!(
        output.iter().all(|&s| (s - expected).abs() < 1e-6),
        "the sidechain did not arrive: {:?} wanted {expected}",
        &output[..4]
    );

    processor.deactivate();

    // --- and a live take is told it is a live take --------------------------

    // Set on every activate, in both directions: the mode belongs to the
    // instance, so one bounced offline and then played live would otherwise
    // still think it has all the time in the world.
    let realtime = AudioConfig {
        offline: false,
        ..config
    };
    let processor = SubPluginMain::activate(&mut plugin, realtime).expect("activates realtime");
    assert_eq!(
        SubPluginMain::snapshot(&plugin).get(PARAM_RENDER_MODE),
        Some(0.0),
        "the offline mode from the previous activate was never taken back"
    );
    processor.deactivate();

    // --- a configuration the plugin cannot have is refused ------------------

    let mono = AudioConfig {
        output_channels: 1,
        ..config
    };
    assert!(
        SubPluginMain::activate(&mut plugin, mono).is_err(),
        "a width the plugin does not declare has to be refused, not adapted"
    );

    // --- the editor --------------------------------------------------------

    assert!(plugin.has_editor());
    #[cfg(windows)]
    {
        plugin.open_editor(std::ptr::null_mut()).expect("opens");
        assert!(plugin.editor_is_open());
        assert!(plugin.editor_can_resize());
        let size = plugin.editor_window().expect("has a window").client_size();
        assert_eq!(
            (size.width, size.height),
            (420, 260),
            "the window was not made the size the plugin asked for"
        );

        // Opening twice is a no-op rather than a second window: the caller is a
        // UI that may not know whether it already asked.
        plugin
            .open_editor(std::ptr::null_mut())
            .expect("idempotent");

        // A tick with nothing pending must leave it alone.
        plugin.tick();
        assert!(plugin.editor_is_open());

        plugin.close_editor();
        assert!(!plugin.editor_is_open());

        // And again, to prove `gui.destroy` really released everything: a
        // plugin that had not would refuse the second `create`.
        plugin.open_editor(std::ptr::null_mut()).expect("reopens");
        assert!(plugin.editor_is_open());
    }

    // Verify dropping plugin with editor still open executes clean teardown.
    drop(plugin);
}
