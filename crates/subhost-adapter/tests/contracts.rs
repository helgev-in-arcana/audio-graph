use std::path::PathBuf;
use std::sync::Arc;

use plugin_host::{Format, ParamId, RestartReason};
use subhost_adapter::{InstanceState, SubHost, SubHostConfig, SubHostState, SubPluginRef};

struct Host;
impl subhost_adapter::SubHostContext for Host {
    fn host_name(&self) -> &str {
        "adapter contract test"
    }
    fn request_restart(&self, _source: subhost_adapter::InstanceId, _: RestartReason) {}
}

fn host() -> SubHost {
    SubHost::new(
        Arc::new(Host),
        SubHostConfig {
            max_instances: 4,
            slot_count: 2,
            lanes: 4,
            target_priority: subhost_adapter::TargetPriority::PreferDirect,
        },
    )
}

fn fixture(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let build = exe.parent().unwrap().parent().unwrap();
    let source = build.join(format!(
        "{}clap_test_plugin{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let directory = build.join("adapter-contracts").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("fixture.clap");
    std::fs::copy(source, &path).expect("build clap-test-plugin before running contract tests");
    path
}

/// Relocation follows caller-selected folders without consulting standard installations.
#[test]
fn restore_uses_the_callers_search_directories() {
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("relocation");
    let mut host = host();
    host.load(0, &path, None).unwrap();
    let mut saved = host.save_state();
    saved.instances[0].reference.path_hint = path.with_extension("missing").display().to_string();
    assert_eq!(host.load_state(&saved, &[]).len(), 1);
    assert!(!host.is_loaded(0));
    let directories = [(Format::Clap, path.parent().unwrap().to_path_buf())];
    assert!(host.load_state(&saved, &directories).is_empty());
    assert!(host.is_loaded(0));
}

/// Missing and out-of-range entries survive saves and reserve their document identities.
#[test]
fn unavailable_instances_survive_until_explicitly_removed() {
    let mut host = host();
    let saved = SubHostState {
        slots: vec![],
        instances: [0, usize::MAX]
            .map(|instance| InstanceState {
                instance,
                reference: SubPluginRef {
                    format: "unavailable-format".into(),
                    plugin_id: "missing".into(),
                    display_name: "Unavailable".into(),
                    path_hint: "missing".into(),
                },
                state: Some("AQID".into()),
            })
            .into(),
    };
    assert_eq!(host.load_state(&saved, &[]).len(), 2);
    assert_eq!(host.save_state().instances, saved.instances);
    assert_eq!(host.free_instance(), Some(1));
    assert_eq!(host.reference(0).unwrap().display_name, "Unavailable");
    host.unload(0);
    assert_eq!(host.free_instance(), Some(0));
    assert_eq!(host.save_state().instances, saved.instances[1..]);
    host.unload_all();
    assert!(host.save_state().instances.is_empty());
}

/// A rejected blob cannot be replaced by the default state of a newly created native plugin.
#[test]
fn failed_native_restoration_keeps_the_original_blob() {
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("rejected-state");
    let mut host = host();
    host.load(0, &path, None).unwrap();
    host.bind_slot(0, 0, ParamId(0)).unwrap();
    let mut saved = host.save_state();
    saved.instances[0].state = Some("AQID".into());
    assert_eq!(host.load_state(&saved, &[]).len(), 1);
    assert!(!host.is_loaded(0));
    assert_eq!(host.save_state(), saved);
    assert!(host.slots().resolved(0).is_none());
    host.load(0, &path, None).unwrap();
    assert!(host.is_loaded(0));
    assert!(host.slots().resolved(0).is_some());
    assert_ne!(
        host.save_state().instances[0].state,
        saved.instances[0].state
    );
}

/// Native save failures preserve the last successful snapshot and can recover on a later save.
#[test]
fn failed_saves_keep_the_last_successful_blob() {
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("failed-save");
    let mut host = host();
    host.load(0, &path, None).unwrap();
    host.set_sub_param(0, ParamId(0), 0.5).unwrap();
    let good = host.save_state();
    host.set_sub_param(0, ParamId(0), 1.5).unwrap();
    host.set_sub_param(0, ParamId(5), 13.0).unwrap();
    assert_eq!(host.save_state(), good);
    assert_ne!(host.save_state(), good);
}

/// Invalid preparation is rejected before native activation and remains retryable.
#[test]
fn activation_validates_lanes_and_instance_io() {
    use subhost_adapter::{InstanceIo, ParamTarget};
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("preparation");
    let mut host = SubHost::new(
        Arc::new(Host),
        SubHostConfig {
            lanes: 2,
            ..host().config()
        },
    );
    host.load(0, &path, None).unwrap();
    assert!(
        host.activate(
            audio_config(),
            &[],
            &[ParamTarget {
                instance: 0,
                param: 0
            }]
        )
        .is_err()
    );
    let mut io = InstanceIo {
        instance: 0,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: vec![],
        aux_outputs: vec![],
    };
    assert!(
        host.activate(audio_config(), &[io.clone(), io.clone()], &[])
            .is_err()
    );
    io.aux_inputs = vec![2; plugin_host::MAX_AUX_BUSES + 1];
    assert!(host.activate(audio_config(), &[io], &[]).is_err());
    let mut processors = host.activate(audio_config(), &[], &[]).unwrap();
    let mut schedule = subhost_adapter::SlotSchedule::new(1, 64, 32).unwrap();
    schedule.begin(32).unwrap();
    let input = [0.0; 64];
    let mut output = [9.0; 64];
    let mut buffers = plugin_host::AudioBuffers::new(
        &input,
        &mut output,
        2,
        2,
        32,
        plugin_host::BufferLayout::Planar,
    );
    assert_eq!(
        processors.get_mut(0).unwrap().process(
            &mut buffers,
            schedule.view(),
            &[],
            0..32,
            &plugin_host::TimeContext::default(),
            &mut plugin_host::EventSink::with_capacity(64)
        ),
        plugin_host::ProcessStatus::Error
    );
    assert_eq!(output, [0.0; 64]);
}

/// Instance-local IDs remain distinguishable after merging and after a slot is replaced.
#[test]
fn notifications_and_outputs_keep_the_native_occupant_identity() {
    use std::sync::Mutex;
    use subhost_adapter::{InstanceEventSink, InstanceId, SubHostContext};
    #[derive(Default)]
    struct Recording(Mutex<Vec<(InstanceId, ParamId, f64)>>);
    impl SubHostContext for Recording {
        fn host_name(&self) -> &str {
            "source test"
        }
        fn request_restart(&self, _: InstanceId, _: RestartReason) {}
        fn param_edited(&self, source: InstanceId, id: ParamId, value: f64) {
            self.0.lock().unwrap().push((source, id, value));
        }
    }
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("source-identity");
    let context = Arc::new(Recording::default());
    let mut host = SubHost::new(context.clone(), host().config());
    for index in 0..2 {
        host.load(index, &path, None).unwrap();
    }
    let sources = [host.source(0).unwrap(), host.source(1).unwrap()];
    for index in 0..2 {
        host.set_sub_param(index, ParamId(5), 12.0).unwrap();
        host.tick_editors();
    }
    assert_eq!(
        *context.0.lock().unwrap(),
        [
            (sources[0], ParamId(0), 0.375),
            (sources[1], ParamId(0), 0.375)
        ]
    );
    let mut old = host.activate(audio_config(), &[], &[]).unwrap();
    let mut sink = InstanceEventSink::with_capacity(8);
    for index in 0..2 {
        host.set_sub_param(index, ParamId(5), 12.0).unwrap();
        run_bound(&mut old, index as u32, &mut sink);
    }
    host.tick_editors();
    host.tick_editors();
    for index in 0..2 {
        run_bound(&mut old, index, &mut sink);
    }
    assert_eq!(
        sink.events().iter().map(|e| e.source).collect::<Vec<_>>(),
        sources
    );
    assert_eq!(sink.events()[0].event, sink.events()[1].event);

    host.set_sub_param(0, ParamId(5), 12.0).unwrap();
    run_bound(&mut old, 0, &mut sink);
    host.tick_editors();
    host.tick_editors();
    host.load(0, &path, None).unwrap();
    assert_ne!(host.source(0), Some(sources[0]));
    sink.clear();
    run_bound(&mut old, 0, &mut sink);
    assert_eq!(sink.events()[0].source, sources[0]);
}

fn run_bound(
    processors: &mut subhost_adapter::SubHostProcessors,
    instance: u32,
    sink: &mut subhost_adapter::InstanceEventSink,
) {
    use subhost_adapter::AudioInstances;
    let mut schedule = subhost_adapter::SlotSchedule::new(4, 64, 32).unwrap();
    schedule.begin(32).unwrap();
    let time = plugin_host::TimeContext::default();
    processors.bind(&time, sink).process(
        instance,
        &[],
        &[1.0; 64],
        &mut [0.0; 64],
        subhost_adapter::AudioChunk {
            input_channels: 2,
            output_channels: 2,
            aux_inputs: Default::default(),
            aux_outputs: Default::default(),
            frames: 32,
            offset: 0,
        },
        schedule.view(),
    );
    assert!(!processors.failed());
}

/// Native transport describes each chunk's first sample, including stopped and looping playback.
#[test]
fn subblocks_advance_transport_without_moving_stopped_playback() {
    use plugin_host::{Event, ParamEvent, Target, TimeContext};
    use subhost_adapter::{AudioChunk, AudioInstances, InstanceEventSink, SlotSchedule};
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("transport");
    let mut host = host();
    host.load(0, &path, None).unwrap();
    let mut processors = host.activate(audio_config(), &[], &[]).unwrap();
    let mut schedule = SlotSchedule::new(4, 64, 32).unwrap();
    schedule.begin(64).unwrap();
    let time = TimeContext {
        playing: true,
        project_time_samples: 48000,
        project_time_music: 2.0,
        ..Default::default()
    };
    let crossing = TimeContext {
        project_time_samples: 95984,
        project_time_music: 4.0 - 32.0 / 48000.0,
        ..time
    };
    let cases = [
        (time, [48032.0 / 48000.0, 2.0 + 64.0 / 48000.0, 0.0]),
        (
            TimeContext {
                playing: false,
                ..time
            },
            [1.0, 2.0, 0.0],
        ),
        (crossing, [96016.0 / 48000.0, 4.0 + 32.0 / 48000.0, 4.0]),
        (
            TimeContext {
                loop_active: true,
                loop_range_seconds: Some((0.0, 2.0)),
                loop_range_music: Some((0.0, 4.0)),
                ..crossing
            },
            [16.0 / 48000.0, 32.0 / 48000.0, 0.0],
        ),
    ];
    let events = [0, 32].map(|sample_offset| {
        Event::Param(ParamEvent::SetValue {
            id: ParamId(5),
            target: Target::Global,
            value: 14.0,
            sample_offset,
        })
    });
    for (time, expected) in cases {
        let mut sink = InstanceEventSink::with_capacity(8);
        let mut bound = processors.bind(&time, &mut sink);
        for offset in [0, 32] {
            let mut output = [0.0; 64];
            bound.process(
                0,
                &events,
                &[0.0; 64],
                &mut output,
                AudioChunk {
                    input_channels: 2,
                    output_channels: 2,
                    frames: 32,
                    offset,
                    aux_inputs: Default::default(),
                    aux_outputs: Default::default(),
                },
                schedule.view(),
            );
            if offset == 32 {
                for (actual, expected) in output.iter().zip(expected) {
                    assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
                }
            }
        }
        assert!(!processors.failed());
    }
}

fn audio_config() -> plugin_host::AudioConfig {
    plugin_host::AudioConfig {
        sample_rate: 48000.0,
        max_block_size: 64,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: true,
    }
}

fn run(processors: &mut subhost_adapter::SubHostProcessors, values: &[f64]) -> f32 {
    let mut schedule = subhost_adapter::SlotSchedule::new(4, 64, 32).unwrap();
    schedule.begin(32).unwrap();
    schedule.fill(values);
    let input = [1.0; 64];
    let mut output = [0.0; 64];
    let mut buffers = plugin_host::AudioBuffers::new(
        &input,
        &mut output,
        2,
        2,
        32,
        plugin_host::BufferLayout::Planar,
    );
    let status = processors.get_mut(0).unwrap().process(
        &mut buffers,
        schedule.view(),
        &[],
        0..32,
        &plugin_host::TimeContext::default(),
        &mut plugin_host::EventSink::with_capacity(64),
    );
    assert_eq!(status, plugin_host::ProcessStatus::Continue);
    output[0]
}

/// Priority depends on the selected source, not on which competing lane most recently changed.
#[test]
fn duplicate_parameter_sources_have_stable_priority() {
    use subhost_adapter::{ParamTarget, TargetPriority};
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture("target-priority");
    for (priority, expected) in [
        (TargetPriority::PreferDirect, [1.6, 1.6]),
        (TargetPriority::PreferSlots, [0.4, 0.6]),
    ] {
        let mut host = SubHost::new(
            Arc::new(Host),
            SubHostConfig {
                target_priority: priority,
                ..host().config()
            },
        );
        host.load(0, &path, None).unwrap();
        for slot in [0, 1] {
            host.bind_slot(0, slot, ParamId(0)).unwrap();
        }
        let mut processors = host
            .activate(
                audio_config(),
                &[],
                &[ParamTarget {
                    instance: 0,
                    param: 0,
                }],
            )
            .unwrap();
        assert_eq!(run(&mut processors, &[0.1, 0.2, 0.8]), expected[0]);
        assert_eq!(run(&mut processors, &[0.5, 0.3, 0.8]), expected[1]);
        assert_eq!(run(&mut processors, &[0.5, 0.3, 0.8]), expected[1]);
    }
    let mut host = SubHost::new(
        Arc::new(Host),
        SubHostConfig {
            target_priority: TargetPriority::RejectConflicts,
            ..host().config()
        },
    );
    host.load(0, &path, None).unwrap();
    for slot in [0, 1] {
        host.bind_slot(0, slot, ParamId(0)).unwrap();
    }
    assert!(host.activate(audio_config(), &[], &[]).is_err());
    host.slots_mut().clear(1);
    host.activate(audio_config(), &[], &[])
        .unwrap()
        .deactivate();
}
