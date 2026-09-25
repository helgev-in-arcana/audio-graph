use plugin_host_api::{AudioConfig, AuxBuses, HostContext, RestartReason, SubPluginMain};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use vst3_host::{Module, Vst3Plugin};

static FIXTURE: Mutex<()> = Mutex::new(());

/// Serialises the tests that share the fixture's process-wide state.
///
/// A test that panics while holding the lock poisons it; taking the guard
/// anyway keeps that one failure from being reported again by every test after it.
fn fixture() -> std::sync::MutexGuard<'static, ()> {
    FIXTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// GUI edits, automation and native output converge in plain units without overriding newer edits.
#[test]
fn parameter_values_cross_both_native_threads() {
    let _thread = vst3_host::init_apartment().unwrap();
    use plugin_host_api::*;
    struct Edits(Mutex<Vec<f64>>);
    impl HostContext for Edits {
        fn host_name(&self) -> &str {
            "parameter test"
        }
        fn request_restart(&self, _: RestartReason) {}
        fn param_edited(&self, _: ParamId, value: f64) {
            self.0.lock().unwrap().push(value);
        }
    }
    fn run(processor: &mut Processor, events: &[Event], sink: &mut EventSink) -> f32 {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let input = [1.0; 8];
                    let mut output = [0.0; 8];
                    let mut buffers =
                        AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
                    assert_eq!(
                        processor.process(&mut buffers, events, &TimeContext::default(), sink),
                        ProcessStatus::Continue
                    );
                    output[0]
                })
                .join()
                .unwrap()
        })
    }
    let _lock = fixture();
    let path = fixture_path();
    let observer = unsafe { libloading::Library::new(&path) }.unwrap();
    let edit = unsafe { observer.get::<unsafe extern "C" fn(f64)>(b"audit_vst_gui_edit") }.unwrap();
    let scale = unsafe { observer.get::<unsafe extern "C" fn(f64)>(b"audit_vst_scale") }.unwrap();
    let emit = unsafe { observer.get::<unsafe extern "C" fn()>(b"audit_vst_emit") }.unwrap();
    // The scale is a static inside the fixture, shared with every later test in
    // this process, so it goes back to 1 however this test ends.
    struct Scaled<'a>(libloading::Symbol<'a, unsafe extern "C" fn(f64)>);
    impl Drop for Scaled<'_> {
        fn drop(&mut self) {
            unsafe { (self.0)(1.0) };
        }
    }
    unsafe {
        scale(10.0);
    }
    let _scaled = Scaled(scale);
    let module = Module::open(&path).unwrap();
    let cid = module.audio_modules().unwrap()[0].cid;
    let context = Arc::new(Edits(Mutex::new(Vec::new())));
    let mut plugin = Vst3Plugin::create(&module, cid, context.clone()).unwrap();
    let mut processor = plugin.activate(AudioConfig::default()).unwrap();
    let event = |value| {
        Event::Param(ParamEvent::SetValue {
            id: ParamId(0),
            target: Target::Global,
            value,
            sample_offset: 0,
        })
    };
    let mut sink = EventSink::with_capacity(8);
    assert_eq!(run(&mut processor, &[event(2.5)], &mut sink), 0.25);
    plugin.tick();
    assert_eq!(plugin.snapshot().get(ParamId(0)), Some(2.5));
    assert!(context.0.lock().unwrap().is_empty());
    unsafe {
        edit(0.75);
    }
    plugin.tick();
    assert_eq!(*context.0.lock().unwrap(), [7.5]);
    assert_eq!(run(&mut processor, &[], &mut sink), 0.75);
    // Leave completed feedback pending while a more recent GUI edit arrives.
    unsafe {
        edit(0.9);
    }
    plugin.tick();
    assert_eq!(plugin.snapshot().get(ParamId(0)), Some(9.0));
    assert_eq!(run(&mut processor, &[event(4.0)], &mut sink), 0.4);
    plugin.tick();
    assert_eq!(plugin.snapshot().get(ParamId(0)), Some(4.0));
    unsafe {
        emit();
    }
    assert_eq!(run(&mut processor, &[], &mut sink), 0.6);
    plugin.tick();
    assert_eq!(plugin.snapshot().get(ParamId(0)), Some(6.0));
    assert!(matches!(
        sink.events(),
        [Event::Param(ParamEvent::SetValue { value: 6.0, .. })]
    ));
    let state = plugin.save_state().unwrap();
    unsafe {
        edit(0.1);
    }
    processor.deactivate();
    plugin.load_state(&state).unwrap();
    let mut processor = plugin.activate(AudioConfig::default()).unwrap();
    assert_eq!(run(&mut processor, &[], &mut sink), 0.6);
}

/// A full parameter queue rejects its batch without applying a prefix or losing main edits.
#[test]
fn input_overflow_preserves_pending_main_edits() {
    let _thread = vst3_host::init_apartment().unwrap();
    use plugin_host_api::*;
    let _lock = fixture();
    let module = Module::open(fixture_path()).unwrap();
    let cid = module.audio_modules().unwrap()[0].cid;
    let mut plugin = Vst3Plugin::create(&module, cid, Arc::new(Host)).unwrap();
    let mut processor = plugin.activate(AudioConfig::default()).unwrap();
    plugin.set_param(ParamId(0), 0.5).unwrap();
    let events = vec![
        Event::Param(ParamEvent::SetValue {
            id: ParamId(0),
            target: Target::Global,
            value: 0.25,
            sample_offset: 0
        });
        513
    ];
    let mut sink = EventSink::with_capacity(8);
    let input = [1.0; 8];
    let mut output = [9.0; 8];
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

/// Refused widths and missing buses cannot yield a processor for a different configuration.
#[test]
fn activation_requires_the_actual_requested_bus_layout() {
    let _thread = vst3_host::init_apartment().unwrap();
    let _lock = fixture();
    let module = Module::open(fixture_path()).unwrap();
    let cid = module.audio_modules().unwrap()[0].cid;
    let mut plugin = Vst3Plugin::create(&module, cid, Arc::new(Host)).unwrap();
    for config in [
        AudioConfig {
            input_channels: 6,
            output_channels: 6,
            ..AudioConfig::default()
        },
        AudioConfig {
            input_channels: 1,
            output_channels: 1,
            ..AudioConfig::default()
        },
        AudioConfig {
            aux_inputs: AuxBuses::new(&[2]),
            ..AudioConfig::default()
        },
        AudioConfig {
            input_channels: 0,
            aux_inputs: AuxBuses::new(&[2]),
            ..AudioConfig::default()
        },
    ] {
        assert!(plugin.activate(config).is_err());
        plugin
            .activate(AudioConfig::default())
            .unwrap()
            .deactivate();
    }
}

struct Host;
impl HostContext for Host {
    fn host_name(&self) -> &str {
        "contract tests"
    }
    fn request_restart(&self, _: RestartReason) {}
}

fn fixture_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile = exe.parent().unwrap().parent().unwrap();
    let name = format!(
        "{}vst3_test_plugin{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = profile.join(name);
    assert!(
        path.is_file(),
        "build vst3-test-plugin before running its native contract tests"
    );
    path
}

/// Only the owning thread can reuse an initialized binary; release permits a new owner.
#[test]
fn module_ownership_is_shared_locally_and_exclusive_across_threads() {
    let _thread = vst3_host::init_apartment().unwrap();
    let _lock = fixture();
    let path = fixture_path();
    let first = Module::open(&path).unwrap();
    let second = Module::open(&path).unwrap();
    drop(first);
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                assert!(matches!(
                    Module::open(&path),
                    Err(plugin_host_api::HostError::ModuleBusy(_))
                ))
            })
            .join()
            .unwrap();
    });
    drop(second);
    std::thread::spawn(move || assert!(Module::open(path).is_ok()))
        .join()
        .unwrap();
}

/// A view keeps the native instance and module initialized after its main handle is released.
#[test]
fn view_retains_its_native_owner() {
    let _thread = vst3_host::init_apartment().unwrap();
    let _lock = fixture();
    let path = fixture_path();
    // The observer keeps code mapped even if an ownership regression ends the module too early.
    let observer = unsafe { libloading::Library::new(&path) }.unwrap();
    let depth =
        unsafe { observer.get::<unsafe extern "C" fn() -> u32>(b"audit_vst_depth") }.unwrap();
    let exit_views =
        unsafe { observer.get::<unsafe extern "C" fn() -> u32>(b"audit_vst_exit_views") }.unwrap();
    let module = Module::open(&path).unwrap();
    let cid = module.audio_modules().unwrap()[0].cid;
    let plugin = Vst3Plugin::create(&module, cid, Arc::new(Host)).unwrap();
    let view = plugin.create_view().unwrap();
    drop(plugin);
    drop(module);
    assert_eq!(unsafe { depth() }, 1);
    drop(view);
    assert_eq!(unsafe { depth() }, 0);
    assert_eq!(unsafe { exit_views() }, 0);
}

/// A silent VST3 block is only that block: it is not reported as the sleep a `Silent` status promises.
///
/// VST3 silence flags describe one block's output and say nothing about the
/// next, where a delay's echo may still arrive without any new input.
#[test]
fn silence_flags_do_not_claim_lasting_silence() {
    let _thread = vst3_host::init_apartment().unwrap();
    use plugin_host_api::*;
    let _lock = fixture();
    let path = fixture_path();
    let observer = unsafe { libloading::Library::new(&path) }.unwrap();
    let silent =
        unsafe { observer.get::<unsafe extern "C" fn(bool)>(b"audit_vst_silent") }.unwrap();
    struct Silenced<'a>(libloading::Symbol<'a, unsafe extern "C" fn(bool)>);
    impl Drop for Silenced<'_> {
        fn drop(&mut self) {
            unsafe { (self.0)(false) };
        }
    }
    unsafe { silent(true) };
    let _silenced = Silenced(silent);

    let module = Module::open(&path).unwrap();
    let cid = module.audio_modules().unwrap()[0].cid;
    let mut plugin = Vst3Plugin::create(&module, cid, Arc::new(Host)).unwrap();
    let mut processor = plugin.activate(AudioConfig::default()).unwrap();
    let input = [0.0; 8];
    let mut output = [0.0; 8];
    let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
    let mut sink = EventSink::with_capacity(8);
    assert_eq!(
        processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink),
        ProcessStatus::Continue
    );
}
