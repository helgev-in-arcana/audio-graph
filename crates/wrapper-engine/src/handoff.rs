//! Giving the audio thread a new value without it ever blocking or freeing.
//!
//! ARCHITECTURE.md §9.1 asks for two things at once: the audio thread must
//! swap in a newly compiled `Program` without taking a lock, and the old one
//! must be reclaimed on the UI thread rather than dropped in the callback.
//! A plain `AtomicPtr` gives the first and not the second — whoever swaps the
//! new pointer in ends up holding the old one, and on the audio thread that
//! means a `free` inside `process`.
//!
//! So the traffic goes both ways. One slot carries values down to the audio
//! thread; a small fixed set of slots carries the displaced ones back up. The
//! audio thread checks it has somewhere to *put* the old value before it takes
//! the new one, which turns the only failure mode — the return slots being
//! full — into "keep running the current program for one more block". That is
//! the correct behaviour anyway: an edit arriving a millisecond late is not
//! something anyone can hear, and it costs nothing to be exactly right about
//! it rather than nearly right.
//!
//! Single producer (the main thread), single consumer (the audio thread).

use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// How many displaced values may be waiting for the main thread at once.
///
/// The main thread drains on every send and on its editor tick, so in practice
/// the depth never exceeds one. Four is room for a burst of edits landing
/// between two turns of the message loop.
const RETURN_SLOTS: usize = 4;

/// A one-value channel down to the audio thread, with a return path for the
/// value it displaces.
pub struct Handoff<T> {
    /// Main thread → audio thread. Null when the audio thread has taken it.
    incoming: AtomicPtr<T>,
    /// Audio thread → main thread. Null slots are free.
    returned: [AtomicPtr<T>; RETURN_SLOTS],
}

impl<T> Default for Handoff<T> {
    fn default() -> Self {
        Handoff::new()
    }
}

impl<T> Handoff<T> {
    pub fn new() -> Handoff<T> {
        Handoff {
            incoming: AtomicPtr::new(ptr::null_mut()),
            returned: [(); RETURN_SLOTS].map(|_| AtomicPtr::new(ptr::null_mut())),
        }
    }

    /// Publish a value for the audio thread. Main thread only.
    ///
    /// Anything the audio thread had not yet picked up is dropped here, which
    /// is right: it was superseded before it was ever used.
    pub fn send(&self, value: Box<T>) {
        let previous = self.incoming.swap(Box::into_raw(value), Ordering::AcqRel);
        if !previous.is_null() {
            // SAFETY: only `send` stores non-null here, and only the audio
            // thread's `take` removes one — either way the pointer we got back
            // came from `Box::into_raw` and nobody else holds it.
            drop(unsafe { Box::from_raw(previous) });
        }
        self.reclaim();
    }

    /// Swap in the pending value, parking whatever `held` contained on the
    /// return path.
    ///
    /// Audio thread only. Returns whether anything changed; `held` is left
    /// exactly as it was when there is nothing new, or when the main thread has
    /// not yet collected enough of the old values for there to be room.
    ///
    /// It takes `&mut Option<Box<T>>` rather than consuming and returning one
    /// so that the decline path cannot drop the caller's value by accident —
    /// which, on the audio thread, would be the one thing this whole type
    /// exists to prevent.
    ///
    /// Never allocates, never frees, never blocks.
    pub fn take(&self, held: &mut Option<Box<T>>) -> bool {
        if self.incoming.load(Ordering::Acquire).is_null() {
            return false;
        }

        // Find somewhere to put the old value *first*. Taking the new one and
        // then discovering there is no room would leave us holding a value we
        // are not allowed to free.
        let slot = match held {
            None => None,
            Some(_) => match self
                .returned
                .iter()
                .find(|slot| slot.load(Ordering::Relaxed).is_null())
            {
                Some(slot) => Some(slot),
                // Return path full: decline, and try again next block.
                None => return false,
            },
        };

        let new = self.incoming.swap(ptr::null_mut(), Ordering::AcqRel);
        if new.is_null() {
            // Cannot happen with one producer, but the alternative to checking
            // is dereferencing null in an audio callback.
            return false;
        }

        // SAFETY: came from `Box::into_raw` in `send`, and the swap above means
        // no one else can observe it.
        let previous = held.replace(unsafe { Box::from_raw(new) });
        if let (Some(slot), Some(old)) = (slot, previous) {
            slot.store(Box::into_raw(old), Ordering::Release);
        }
        true
    }

    /// Free everything the audio thread has handed back. Main thread only.
    pub fn reclaim(&self) {
        for slot in &self.returned {
            let ptr = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !ptr.is_null() {
                // SAFETY: as above; the swap makes us the only owner.
                drop(unsafe { Box::from_raw(ptr) });
            }
        }
    }
}

impl<T> Drop for Handoff<T> {
    fn drop(&mut self) {
        let pending = self.incoming.swap(ptr::null_mut(), Ordering::AcqRel);
        if !pending.is_null() {
            drop(unsafe { Box::from_raw(pending) });
        }
        self.reclaim();
    }
}

// SAFETY: the pointers are owned `Box<T>` in transit. Sending one across the
// channel moves ownership, so `T: Send` is what is required, and the atomics
// order the transfer.
unsafe impl<T: Send> Send for Handoff<T> {}
unsafe impl<T: Send> Sync for Handoff<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn the_audio_side_sees_what_the_main_side_sent() {
        let handoff = Handoff::new();
        let mut held = None;
        assert!(!handoff.take(&mut held), "nothing sent yet");

        handoff.send(Box::new(7));
        assert!(handoff.take(&mut held));
        assert_eq!(held.as_deref(), Some(&7));
        assert!(!handoff.take(&mut held), "no second value to take");
        assert_eq!(
            held.as_deref(),
            Some(&7),
            "declining must not disturb what we hold"
        );
    }

    #[test]
    fn the_displaced_value_goes_back_to_the_main_thread() {
        struct Tracked(Arc<AtomicUsize>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let handoff = Handoff::new();

        let mut held = None;
        handoff.send(Box::new(Tracked(drops.clone())));
        assert!(handoff.take(&mut held));

        handoff.send(Box::new(Tracked(drops.clone())));
        assert!(handoff.take(&mut held));
        // The audio thread parked the first one; nothing has freed it yet.
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        handoff.reclaim();
        assert_eq!(
            drops.load(Ordering::Relaxed),
            1,
            "old value freed on the main thread"
        );

        drop(held);
        drop(handoff);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_full_return_path_makes_the_audio_side_decline_rather_than_free() {
        let handoff = Handoff::new();

        handoff.send(Box::new(0usize));
        let mut held = None;
        assert!(handoff.take(&mut held));

        // Fill every return slot without ever letting the main thread drain.
        for i in 1..=RETURN_SLOTS {
            // `send` reclaims, so store directly to model a main thread that is
            // busy elsewhere.
            handoff
                .incoming
                .store(Box::into_raw(Box::new(i)), Ordering::Release);
            assert!(handoff.take(&mut held));
            assert_eq!(held.as_deref(), Some(&i));
        }

        handoff
            .incoming
            .store(Box::into_raw(Box::new(99usize)), Ordering::Release);
        assert!(
            !handoff.take(&mut held),
            "no room to park the old value, so no swap"
        );
        assert_eq!(
            held.as_deref(),
            Some(&RETURN_SLOTS),
            "and we keep running what we had"
        );
    }

    #[test]
    fn superseding_an_uncollected_value_does_not_leak_it() {
        let handoff = Handoff::new();
        handoff.send(Box::new(1));
        handoff.send(Box::new(2));
        let mut held = None;
        assert!(handoff.take(&mut held));
        assert_eq!(held.as_deref(), Some(&2));
    }

    #[test]
    fn it_survives_a_real_producer_and_consumer() {
        let handoff = Arc::new(Handoff::<usize>::new());
        let producer = {
            let handoff = handoff.clone();
            std::thread::spawn(move || {
                for i in 0..10_000 {
                    handoff.send(Box::new(i));
                }
            })
        };

        let mut held: Option<Box<usize>> = None;
        let mut seen = 0usize;
        let mut last = 0usize;
        let mut finishing = false;
        loop {
            if handoff.take(&mut held) {
                let next = **held.as_ref().expect("take reported a swap");
                assert!(next >= last, "values must not go backwards");
                last = next;
                seen += 1;
            }
            // Standing in for the main thread's message loop, which drains the
            // return path between blocks.
            handoff.reclaim();
            if finishing {
                break;
            }
            finishing = producer.is_finished();
        }
        producer.join().unwrap();
        assert_eq!(last, 9_999, "the consumer must end up on the newest value");
        assert!(seen > 1, "the consumer only ever saw one value");
    }
}
