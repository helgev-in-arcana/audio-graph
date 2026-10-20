use crate::ir::{NoteOwnership, ResetReason, RuntimeEntry};
use crate::notes::NoteLedger;

pub(super) fn adopt<T: RuntimeEntry>(
    current: &mut Vec<T>,
    prepared: &mut Vec<T>,
    ledger: &mut NoteLedger,
) {
    for next in prepared.iter_mut() {
        if let Some(old) = current
            .iter_mut()
            .find(|old| old.node() == next.node() && old.kind() == next.kind())
        {
            next.runtime().swap_state(old.runtime().state_mut());
        }
    }
    let mut notes = NoteOwnership::new(ledger);
    for old in current.iter_mut() {
        old.runtime().release(&mut notes);
    }
    // The prepared program carries retired boxes back through the handoff.
    // Moving the vectors leaves destruction on the publishing thread.
    std::mem::swap(current, prepared);
}

pub(super) fn reset<T: RuntimeEntry>(entries: &mut [T], reason: ResetReason) {
    for entry in entries {
        entry.runtime().reset(reason);
    }
}
