// ============================================================================
//
// HUMAN REVIEW REQUIRED: THIS FILE HAS NOT BEEN REVIEWED BY A HUMAN.
//
// ============================================================================

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use plugin_host::{HostContext, ParamId, RestartReason};

/// A runtime occupant of an instance slot; replacements never reuse its generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceId {
    pub index: u32,
    pub generation: u64,
}

impl InstanceId {
    pub(crate) fn new(index: u32) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("instance generation exhausted");
        Self { index, generation }
    }
}

/// Main-side notifications retain the child identity before the parent chooses how to combine them.
pub trait SubHostContext: Send + Sync {
    fn host_name(&self) -> &str;
    fn request_restart(&self, source: InstanceId, reason: RestartReason);
    fn latency_changed(&self, source: InstanceId, _samples: u32) {
        self.request_restart(source, RestartReason::Latency);
    }
    fn param_edited(&self, _source: InstanceId, _id: ParamId, _plain: f64) {}
}

pub(crate) struct InstanceContext {
    pub source: InstanceId,
    pub parent: Arc<dyn SubHostContext>,
}

impl HostContext for InstanceContext {
    fn host_name(&self) -> &str {
        self.parent.host_name()
    }
    fn request_restart(&self, reason: RestartReason) {
        self.parent.request_restart(self.source, reason);
    }
    fn latency_changed(&self, samples: u32) {
        self.parent.latency_changed(self.source, samples);
    }
    fn param_edited(&self, id: ParamId, plain: f64) {
        self.parent.param_edited(self.source, id, plain);
    }
}
