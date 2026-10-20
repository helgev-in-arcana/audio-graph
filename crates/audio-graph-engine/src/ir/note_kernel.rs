use std::fmt::Debug;

use super::{KernelState, NodeId, Runtime, RuntimeEntry, Stateful};
use crate::engine::NoteEvalContext;

/// Immutable settings with separately owned, typed state. Evaluation must not
/// allocate, free, lock, or keep evolving state through interior mutability.
pub(crate) trait NoteKernel: Debug + Send + Sync + 'static {
    type State: KernelState;

    fn create_state(&self) -> Self::State;
    fn eval(&self, state: &mut Self::State, cx: &mut NoteEvalContext<'_>);
}

pub(crate) trait NoteProcessor: Runtime {
    fn eval(&mut self, cx: &mut NoteEvalContext<'_>);
}

impl<K: NoteKernel> NoteProcessor for Stateful<K, K::State> {
    fn eval(&mut self, cx: &mut NoteEvalContext<'_>) {
        self.kernel.eval(&mut self.state, cx);
    }
}

#[derive(Debug)]
pub(crate) struct NoteParamBinding {
    pub lane: Option<u16>,
    pub fallback: f64,
}

#[derive(Debug)]
pub(crate) struct CustomNote {
    pub node: NodeId,
    pub parameters: Box<[NoteParamBinding]>,
    pub processor: Box<dyn NoteProcessor>,
}

impl CustomNote {
    pub(crate) fn new<K: NoteKernel>(
        node: NodeId,
        parameters: Box<[NoteParamBinding]>,
        kernel: K,
    ) -> Self {
        let state = kernel.create_state();
        Self {
            node,
            parameters,
            processor: Box::new(Stateful { kernel, state }),
        }
    }
}

impl RuntimeEntry for CustomNote {
    fn node(&self) -> NodeId {
        self.node
    }

    fn kind(&self) -> super::KernelKind {
        self.processor.kind()
    }

    fn runtime(&mut self) -> &mut dyn Runtime {
        &mut *self.processor
    }
}
