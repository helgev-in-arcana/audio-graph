use plugin_host_api::{HostContext, RestartReason};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use vst3_host::{Module, Vst3Plugin};

static FIXTURE: Mutex<()> = Mutex::new(());

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

/// A view keeps the native instance and module initialized after its main handle is released.
#[test]
fn view_retains_its_native_owner() {
    let _lock = FIXTURE.lock().unwrap();
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
