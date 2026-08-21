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

#[derive(Default)]
struct TestHost;

impl HostContext for TestHost {
    fn host_name(&self) -> &str {
        "clap-host tests"
    }
    fn request_restart(&self, _reason: RestartReason) {}
}

/// Where `cargo` put the fixture's shared library.
///
/// A `.clap` on Windows and Linux *is* the shared library, so the artifact is
/// loadable as it stands and nothing has to be copied or renamed.
fn fixture_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/<test>.exe
    let build_dir = exe.parent()?.parent()?;
    let names = [
        "clap_test_plugin.dll",
        "libclap_test_plugin.so",
        "libclap_test_plugin.dylib",
    ];
    names
        .iter()
        .map(|n| build_dir.join(n))
        .find(|p| p.is_file())
}

#[test]
fn the_backend_drives_a_real_clap_module() {
    let Some(path) = fixture_path() else {
        eprintln!("clap-test-plugin has not been built; run `cargo build -p clap-test-plugin`");
        return;
    };

    // --- module and factory ------------------------------------------------

    let module = Module::open(&path).expect("the fixture loads");
    let classes = module.classes().expect("the factory enumerates");
    assert_eq!(classes.len(), 1, "the fixture exports one plugin");
    let class = classes[0].clone();
    assert_eq!(class.id, "dev.audio-graph.clap-test-plugin");
    assert!(class.features.iter().any(|f| f == "audio-effect"));
    assert!(!class.is_instrument(), "the fixture is an effect");

    // ADR-7: a second handle onto the same path must not run the entry point
    // again. The fixture counts, and the count is checked below through the
    // fact that everything still works after the second handle is dropped.
    {
        let again = Module::open(&path).expect("the same module opens twice");
        assert_eq!(again.classes().unwrap().len(), 1);
    }

    let context: Arc<dyn HostContext> = Arc::new(TestHost);

    // --- parameters --------------------------------------------------------

    let mut plugin =
        ClapPlugin::create(&module, &class.id, Arc::clone(&context)).expect("instantiates");

    let params = SubPluginMain::params(&plugin).to_vec();
    assert_eq!(params.len(), 4, "{params:#?}");

    let gain = params.iter().find(|p| p.id == PARAM_GAIN).expect("gain");
    // Plain values with a real range, straight from the plugin — the whole
    // point of ADR-4's data model, and nothing is normalised on the way.
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

    // Formatting is delegated to the plugin, units and all (§4.1).
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
    assert!(
        io.inputs[1].is_aux,
        "the sidechain is the aux socket (§14.2)"
    );
    assert_eq!(io.aux_inputs().len(), 1);
    assert_eq!(io.outputs.len(), 1);
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
    // out = in * gain. An unwired sidechain contributed nothing, which is what
    // §14.11 is about — the port exists, and it was silent.
    assert!(
        output.iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "unwired sidechain leaked: {:?}",
        &output[..4]
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

    // The input region is main-then-aux, packed (§4.3).
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

    // Dropped with the editor still open — §5.3's dangerous path, where the
    // DAW destroys the instance without ever saying "close". If the sequence
    // were wrong this is where it would fault.
    drop(plugin);
}
