//! Thread-affinity container for values that must only be accessed on the thread that created them.
//!
//! Both plugin formats require a plugin object to be `Send` — the host may
//! construct it on one thread and use it on another — while VST3 requires the
//! sub-plugin's controller half to be touched only from the thread that owns
//! the host's main loop. Those two rules are compatible in practice, because a
//! DAW does its main-thread work on one thread throughout, but they are not
//! compatible in the type system.
//!
//! Rather than sprinkle `unsafe impl Send` over the adapter and hope, the
//! affinity is made into a value: [`MainThread`] carries the thread it was
//! created on and checks every access. The unsafety is confined here, with a
//! runtime check that turns a violated invariant into a clear panic instead of
//! a corrupted plugin.

use std::thread::ThreadId;

/// A value pinned to the thread that created it.
pub struct MainThread<T> {
    value: T,
    owner: ThreadId,
}

// SAFETY: the value is only ever reachable through `get`/`get_mut`, which
// refuse to hand it out on any thread but its owner. `Send` is therefore
// sound: moving the container between threads cannot produce a reference to
// `T` on the wrong one.
//
// Drop is the one exception — see the `Drop` impl below.
unsafe impl<T> Send for MainThread<T> {}

// SAFETY: the same argument, one step further. `&MainThread<T>` hands out
// nothing on a foreign thread — `get`/`get_mut` panic and `try_get` returns
// `None` — so sharing the reference cannot produce a `&T` anywhere but the
// owning thread. That is what makes it legitimate to put one inside an `Arc`
// that the audio thread also holds: the audio thread can carry the pointer, it
// simply cannot look through it.
unsafe impl<T> Sync for MainThread<T> {}

impl<T> MainThread<T> {
    pub fn new(value: T) -> MainThread<T> {
        MainThread {
            value,
            owner: std::thread::current().id(),
        }
    }

    /// The owning thread, for callers that need to check before acting rather
    /// than be panicked at.
    pub fn is_owner(&self) -> bool {
        std::thread::current().id() == self.owner
    }

    /// # Panics
    /// If called from any thread other than the one that created the value.
    #[track_caller]
    pub fn get(&self) -> &T {
        self.assert_owner();
        &self.value
    }

    /// # Panics
    /// If called from any thread other than the one that created the value.
    #[track_caller]
    pub fn get_mut(&mut self) -> &mut T {
        self.assert_owner();
        &mut self.value
    }

    /// Access the value only if we are on the right thread.
    ///
    /// For paths that must not panic — an audio callback that would like to
    /// peek at main-thread state and can simply do without it.
    pub fn try_get(&self) -> Option<&T> {
        self.is_owner().then_some(&self.value)
    }

    #[track_caller]
    fn assert_owner(&self) {
        assert!(
            self.is_owner(),
            "main-thread-only value touched from another thread; \
             VST3 pins controller calls to the thread that created the instance"
        );
    }
}

impl<T> Drop for MainThread<T> {
    fn drop(&mut self) {
        // Deliberately not asserting. A host that tears an instance down from a
        // different thread is misbehaving, but panicking in a destructor during
        // teardown replaces a possible problem with a certain crash — and in a
        // plugin, that crash is the DAW's. The check exists on the access paths
        // where it can still do something useful.
        if !self.is_owner() {
            log::error!(
                "sub-plugin dropped from a thread other than the one that created it; \
                 this is a host contract violation and may destabilise the plugin"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_on_the_owning_thread_is_fine() {
        let v = MainThread::new(41);
        assert_eq!(*v.get(), 41);
        assert!(v.try_get().is_some());
    }

    #[test]
    fn access_from_another_thread_panics_rather_than_corrupting() {
        let v = MainThread::new(String::from("controller"));
        let result =
            std::thread::scope(|s| s.spawn(|| std::panic::catch_unwind(|| v.get())).join());
        let inner = result.expect("thread joined");
        assert!(inner.is_err(), "expected a panic on the wrong thread");
    }

    #[test]
    fn try_get_declines_instead_of_panicking() {
        let v = MainThread::new(7);
        std::thread::scope(|s| {
            s.spawn(|| assert!(v.try_get().is_none()));
        });
    }
}
