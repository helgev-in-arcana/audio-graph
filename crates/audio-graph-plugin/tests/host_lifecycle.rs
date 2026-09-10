mod harness;

use std::sync::{Arc, Mutex};

use audio_graph_plugin::{SUB_HOST, Shared, WrapperParams};
use plugin_host::{AudioConfig, HostContext, RestartReason};
use subhost_adapter::{InstanceIo, SubHost};

struct Context(Arc<Mutex<Vec<std::thread::ThreadId>>>);

impl HostContext for Context {
    fn host_name(&self) -> &str {
        "lifecycle test"
    }

    fn request_restart(&self, _reason: RestartReason) {}
}

impl Drop for Context {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(std::thread::current().id());
    }
}

fn config() -> AudioConfig {
    AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 32,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: true,
    }
}

fn host() -> SubHost {
    SubHost::new(Arc::new(Context(Arc::default())), SUB_HOST)
}

/// Reusing an instance index cannot redirect the return of its previous processor.
#[test]
fn replacing_an_instance_preserves_the_old_processors_owner() {
    let path = harness::fixture_as_clap("instance-return-fixture");
    let mut host = host();
    host.load(0, &path, None).unwrap();
    let mut previous = host.activate(config(), &[], &[]).unwrap();
    host.load(0, &path, None).unwrap();
    let current = host.activate(config(), &[], &[]).unwrap();
    previous.reset();
    previous.deactivate();
    assert!(
        host.activate(config(), &[], &[]).is_err(),
        "the replacement is still active"
    );
    current.deactivate();
    host.activate(config(), &[], &[]).unwrap().deactivate();
}

/// A failed group activation returns processors completed before the failure.
#[test]
fn partial_activation_is_fully_returned() {
    let path = harness::fixture_as_clap("activation-failure-fixture");
    let mut host = host();
    host.load(0, &path, None).unwrap();
    host.load(1, &path, None).unwrap();
    let incompatible = InstanceIo {
        instance: 1,
        input_channels: 2,
        output_channels: 1,
        aux_inputs: Vec::new(),
        aux_outputs: Vec::new(),
    };
    assert!(host.activate(config(), &[incompatible], &[]).is_err());
    host.activate(config(), &[], &[]).unwrap().deactivate();
}

/// The final Shared handle may be released on audio without destroying main-thread resources.
#[test]
fn shared_shutdown_returns_its_active_instances_to_main() {
    let path = harness::fixture_as_clap("shared-shutdown-fixture");
    let drops = Arc::new(Mutex::new(Vec::new()));
    let context = Arc::new(Context(drops.clone()));
    let weak = Arc::downgrade(&context);
    let mut host = SubHost::new(context, SUB_HOST);
    host.load(0, &path, None).unwrap();
    let shared = Shared::new(host, WrapperParams::new());
    shared.main().config = Some(config());
    shared.rebind().unwrap();
    assert!(shared.has_processors());
    std::thread::spawn(move || drop(shared)).join().unwrap();
    assert!(weak.upgrade().is_some());
    assert!(drops.lock().unwrap().is_empty());
    plugin_host::reclaim_main_thread();
    assert!(weak.upgrade().is_none());
    assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
}
