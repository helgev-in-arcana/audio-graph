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
use std::sync::Arc;

use clap_host::{ClapPlugin, Module};
use plugin_host_api::{
    AudioBuffers, AudioConfig, AuxBuses, BufferLayout, Event, EventSink, HostContext, NoteEvent,
    ParamEvent, ParamFlags, ParamId, ProcessStatus, RestartReason, SubPluginMain, Target,
    TimeContext,
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
/// Mirrors `clap_test_plugin::ask`, spelled out for the same reason as
/// `OUTPUT_PORT_BIT`: this crate does not depend on the fixture's Rust API,
/// only on the module it builds.
const ASK_RESTART: f64 = 1.0;
const ASK_AUDIO_PORTS_RESCAN: f64 = 2.0;
const ASK_NOTE_PORTS_RESCAN: f64 = 3.0;

#[derive(Default)]
struct TestHost;

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
}

impl HostContext for RecordingHost {
    fn host_name(&self) -> &str {
        "clap-host tests"
    }
    fn request_restart(&self, reason: RestartReason) {
        self.reasons.lock().expect("not poisoned").push(reason);
    }
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
    let module = Module::open(fixture_path()).expect("the fixture opens");

    for (ask, expected) in [
        (ASK_RESTART, RestartReason::IoConfig),
        (ASK_AUDIO_PORTS_RESCAN, RestartReason::IoConfig),
        (ASK_NOTE_PORTS_RESCAN, RestartReason::IoConfig),
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

        let seen = host.reasons.lock().unwrap().clone();
        assert_eq!(seen, vec![expected], "ask {ask} was not forwarded");
    }
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
    assert_eq!(params.len(), 7, "{params:#?}");

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
    SubPluginMain::deactivate(&mut plugin, processor);
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
    SubPluginMain::deactivate(&mut plugin, processor);
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
        note_id: 1,
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

    SubPluginMain::deactivate(&mut plugin, processor);

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

    SubPluginMain::deactivate(&mut plugin, processor);

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
    SubPluginMain::deactivate(&mut plugin, processor);

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
