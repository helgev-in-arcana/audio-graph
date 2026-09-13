use std::path::PathBuf;
use std::sync::Arc;

use plugin_host::{Format, HostContext, ParamId, RestartReason};
use subhost_adapter::{InstanceState, SubHost, SubHostConfig, SubHostState, SubPluginRef};

struct Host;
impl HostContext for Host {
    fn host_name(&self) -> &str {
        "adapter contract test"
    }
    fn request_restart(&self, _: RestartReason) {}
}

fn host() -> SubHost {
    SubHost::new(
        Arc::new(Host),
        SubHostConfig {
            max_instances: 4,
            slot_count: 2,
            lanes: 4,
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
