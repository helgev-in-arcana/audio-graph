use std::path::PathBuf;
use std::sync::Arc;

use plugin_host::{Format, HostContext, RestartReason};
use subhost_adapter::{SubHost, SubHostConfig};

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
