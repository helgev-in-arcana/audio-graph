use std::fmt::Debug;

use super::{KernelKind, KernelState, NodeId, Runtime, RuntimeEntry, Stateful};
use crate::engine::AudioEvalContext;

/// Immutable settings with separately owned, typed state. Evaluation must not
/// allocate, free, lock, or keep evolving state through interior mutability.
pub(crate) trait AudioKernel: Debug + Send + Sync + 'static {
    type State: KernelState;
    fn create_state(&self) -> Self::State;
    fn eval(&self, state: &mut Self::State, cx: &mut AudioEvalContext<'_>);
}

pub(crate) trait AudioProcessor: Runtime {
    fn eval(&mut self, cx: &mut AudioEvalContext<'_>);
}

impl<K: AudioKernel> AudioProcessor for Stateful<K, K::State> {
    fn eval(&mut self, cx: &mut AudioEvalContext<'_>) {
        self.kernel.eval(&mut self.state, cx);
    }
}

#[derive(Debug)]
pub(crate) struct CustomAudio {
    pub node: NodeId,
    pub processor: Box<dyn AudioProcessor>,
}

impl CustomAudio {
    pub(crate) fn new<K: AudioKernel>(node: NodeId, kernel: K) -> Self {
        let state = kernel.create_state();
        Self {
            node,
            processor: Box::new(Stateful { kernel, state }),
        }
    }
}

impl RuntimeEntry for CustomAudio {
    fn node(&self) -> NodeId {
        self.node
    }
    fn kind(&self) -> KernelKind {
        self.processor.kind()
    }
    fn runtime(&mut self) -> &mut dyn Runtime {
        self.processor.as_mut()
    }
}
