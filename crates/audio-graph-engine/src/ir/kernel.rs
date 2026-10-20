use std::any::{Any, TypeId, type_name};
use std::fmt::{self, Debug};

use super::NodeId;
use crate::notes::NoteLedger;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct KernelKind {
    id: TypeId,
    name: &'static str,
}

impl KernelKind {
    pub(crate) fn of<T: 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            name: type_name::<T>(),
        }
    }
}

impl Debug for KernelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetReason {
    Notes,
    Transport,
    All,
}

pub(crate) struct NoteOwnership<'a> {
    ledger: &'a mut NoteLedger,
}

impl<'a> NoteOwnership<'a> {
    pub(crate) fn new(ledger: &'a mut NoteLedger) -> Self {
        Self { ledger }
    }

    pub(crate) fn retain(&mut self, id: i32) {
        self.ledger.delivered(id);
    }

    pub(crate) fn release(&mut self, id: i32) {
        self.ledger.finished(id);
    }
}

/// State is prepared off-thread. Reset and release must neither allocate nor
/// free; release returns held notes before an unmatched state is retired.
pub(crate) trait KernelState: Any + Debug + Send + Sync {
    fn reset(&mut self, reason: ResetReason);
    fn release(&mut self, _notes: &mut NoteOwnership<'_>) {}
}

impl KernelState for () {
    fn reset(&mut self, _reason: ResetReason) {}
}

pub(crate) trait Runtime: Debug + Send + Sync {
    fn kind(&self) -> KernelKind;
    fn state_mut(&mut self) -> &mut dyn Any;
    fn swap_state(&mut self, other: &mut dyn Any);
    fn reset(&mut self, reason: ResetReason);
    fn release(&mut self, notes: &mut NoteOwnership<'_>);
}

/// Dispatch borrows these fields separately, so kernels keep immutable
/// settings while their concrete state needs no per-call type check.
#[derive(Debug)]
pub(crate) struct Stateful<K, S> {
    pub kernel: K,
    pub state: S,
}

impl<K: Debug + Send + Sync + 'static, S: KernelState> Runtime for Stateful<K, S> {
    fn kind(&self) -> KernelKind {
        KernelKind::of::<Self>()
    }

    fn state_mut(&mut self) -> &mut dyn Any {
        &mut self.state
    }

    fn swap_state(&mut self, other: &mut dyn Any) {
        std::mem::swap(
            &mut self.state,
            other.downcast_mut::<S>().expect("matching kernel state"),
        );
    }

    fn reset(&mut self, reason: ResetReason) {
        self.state.reset(reason);
    }

    fn release(&mut self, notes: &mut NoteOwnership<'_>) {
        self.state.release(notes);
    }
}

pub(crate) trait RuntimeEntry {
    fn node(&self) -> NodeId;
    fn kind(&self) -> KernelKind;
    fn runtime(&mut self) -> &mut dyn Runtime;
}
