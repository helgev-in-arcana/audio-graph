//! Destruction on the thread that owns a plugin's control and OS resources.

use std::cell::{RefCell, UnsafeCell};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;

use crate::{AudioBuffers, Event, EventSink, ProcessStatus, SubPluginProcessor, TimeContext};

struct Entry<T> {
    released: AtomicBool,
    value: UnsafeCell<T>,
}

struct Erased {
    pointer: *mut (),
    ready: unsafe fn(*mut ()) -> bool,
    destroy: unsafe fn(*mut ()),
}

impl Erased {
    fn new<T>(pointer: *mut Entry<T>) -> Self {
        unsafe fn ready<T>(pointer: *mut ()) -> bool {
            // SAFETY: the registry retains the allocation until its unique handle releases it.
            unsafe { &*pointer.cast::<Entry<T>>() }
                .released
                .load(Ordering::Acquire)
        }

        unsafe fn destroy<T>(pointer: *mut ()) {
            // SAFETY: only the owning registry calls this, once the handle has been released.
            drop(unsafe { Box::from_raw(pointer.cast::<Entry<T>>()) });
        }

        Self {
            pointer: pointer.cast(),
            ready: ready::<T>,
            destroy: destroy::<T>,
        }
    }
}

#[derive(Default)]
struct Registry {
    entries: Vec<Erased>,
}

fn reclaim_entries(entries: &mut Vec<Erased>) {
    loop {
        let mut reclaimed = false;
        let mut index = 0;
        while index < entries.len() {
            // SAFETY: every entry belongs to this thread and remains allocated while registered.
            if unsafe { (entries[index].ready)(entries[index].pointer) } {
                let entry = entries.swap_remove(index);
                // SAFETY: the acquire above observes the end of all access through the handle.
                unsafe { (entry.destroy)(entry.pointer) };
                reclaimed = true;
            } else {
                index += 1;
            }
        }
        if !reclaimed {
            break;
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        reclaim_entries(&mut self.entries);
        // Outstanding handles can outlive this thread. Retaining their allocations is safer
        // than destroying a plugin while its audio thread can still be using it.
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Reclaims released resources belonging to the calling thread.
///
/// Call from the host's main-thread pump, and before shutting that pump down.
/// Unreturned resources outlive an exiting owner thread; they are retained instead
/// of being destroyed on another thread. No destructor runs while the registry is borrowed.
pub fn reclaim_main_thread() {
    let Ok(Some(mut entries)) = REGISTRY.try_with(|registry| {
        registry
            .try_borrow_mut()
            .ok()
            .map(|mut registry| std::mem::take(&mut registry.entries))
    }) else {
        return;
    };
    reclaim_entries(&mut entries);
    let _ = REGISTRY.try_with(|registry| registry.borrow_mut().entries.extend(entries));
}

struct Release {
    released: NonNull<AtomicBool>,
    owner: ThreadId,
}

impl Drop for Release {
    fn drop(&mut self) {
        let on_owner = std::thread::current().id() == self.owner;
        // SAFETY: this is the allocation's only release handle. Nothing dereferences it
        // after this store, which permits its owner to reclaim it concurrently.
        unsafe { self.released.as_ref() }.store(true, Ordering::Release);
        if on_owner {
            reclaim_main_thread();
        }
    }
}

fn retain<T: 'static>(value: T) -> (NonNull<T>, Release) {
    let entry = Box::into_raw(Box::new(Entry {
        released: AtomicBool::new(false),
        value: UnsafeCell::new(value),
    }));
    // SAFETY: Box gives stable, non-null addresses until the registry destroys it.
    let (value, released) = unsafe {
        (
            NonNull::new_unchecked((*entry).value.get()),
            NonNull::from(&(*entry).released),
        )
    };
    // During thread-local teardown the registry may already be gone. Retain the
    // allocation in that case too; a release must never choose a foreign destructor.
    let _ = REGISTRY.try_with(|registry| registry.borrow_mut().entries.push(Erased::new(entry)));
    (
        value,
        Release {
            released,
            owner: std::thread::current().id(),
        },
    )
}

/// A value accessed and destroyed only on the thread that created it.
///
/// Moving or sharing this handle does not move that responsibility. A foreign
/// release is reclaimed by [`reclaim_main_thread`] on the owner thread.
pub struct MainThread<T: 'static> {
    value: NonNull<T>,
    release: Release,
}

// SAFETY: access is restricted to the owner, including destruction. Foreign
// threads can only publish the end of the handle's lifetime through its atomic flag.
unsafe impl<T> Send for MainThread<T> {}
// SAFETY: shared access to T is available only on its owning thread.
unsafe impl<T> Sync for MainThread<T> {}

impl<T> MainThread<T> {
    pub fn new(value: T) -> Self {
        let (value, release) = retain(value);
        Self { value, release }
    }

    pub fn is_owner(&self) -> bool {
        std::thread::current().id() == self.release.owner
    }

    /// # Panics
    /// Panics if called outside the owning thread.
    #[track_caller]
    pub fn get(&self) -> &T {
        assert!(
            self.is_owner(),
            "main-thread resource accessed from another thread"
        );
        // SAFETY: the live handle retains T, and this is its only permitted thread.
        unsafe { self.value.as_ref() }
    }

    /// # Panics
    /// Panics if called outside the owning thread.
    #[track_caller]
    pub fn get_mut(&mut self) -> &mut T {
        assert!(
            self.is_owner(),
            "main-thread resource accessed from another thread"
        );
        // SAFETY: the unique handle borrow excludes every other borrow of T.
        unsafe { self.value.as_mut() }
    }

    pub fn try_get(&self) -> Option<&T> {
        self.is_owner().then(|| self.get())
    }
}

/// A running processor whose resources return to their creation thread on release.
///
/// The backend must retain its native instance and module inside the processor.
/// Its destructor stops the activation. Returning the handle addresses that exact
/// activation, without accepting a separate main-side instance as a destination.
pub struct Processor {
    value: NonNull<dyn SubPluginProcessor>,
    _release: Release,
}

// SAFETY: the payload implements Send, processing requires exclusive access,
// and destruction waits for that exclusive handle to be released.
unsafe impl Send for Processor {}

impl Processor {
    pub fn new(value: impl SubPluginProcessor + 'static) -> Self {
        let (value, release) = retain(value);
        Self {
            value,
            _release: release,
        }
    }

    /// Stops this activation immediately on its owner thread, or schedules its
    /// stop for the owner's next [`reclaim_main_thread`] call.
    pub fn deactivate(self) {}
}

impl SubPluginProcessor for Processor {
    #[inline]
    fn process(
        &mut self,
        buffers: &mut AudioBuffers<'_>,
        events: &[Event],
        context: &TimeContext,
        out_events: &mut EventSink,
    ) -> ProcessStatus {
        // SAFETY: the unique live handle is the only accessor to the Send payload.
        unsafe { self.value.as_mut() }.process(buffers, events, context, out_events)
    }

    #[inline]
    fn reset(&mut self) {
        // SAFETY: as in process; no registry access or ownership change is needed.
        unsafe { self.value.as_mut() }.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    struct Dropped(Arc<Mutex<Vec<ThreadId>>>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(std::thread::current().id());
        }
    }

    /// A foreign release never executes the destructor of a non-Send value.
    #[test]
    fn foreign_release_is_reclaimed_by_the_owner() {
        let drops = Arc::new(Mutex::new(Vec::new()));
        let value = MainThread::new((Rc::new(1), Dropped(drops.clone())));
        std::thread::spawn(move || drop(value)).join().unwrap();
        assert!(drops.lock().unwrap().is_empty());
        reclaim_main_thread();
        assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
    }

    /// Access checks remain effective even though the handle can cross threads.
    #[test]
    fn foreign_access_is_rejected() {
        let value = MainThread::new(41);
        std::thread::scope(|scope| {
            scope.spawn(|| assert!(value.try_get().is_none()));
        });
        assert_eq!(*value.get(), 41);
    }

    /// Dropping an owner also reclaims children released by its destructor.
    #[test]
    fn reclamation_handles_nested_resources() {
        let drops = Arc::new(Mutex::new(Vec::new()));
        let child = MainThread::new(Dropped(drops.clone()));
        let parent = MainThread::new(child);
        drop(parent);
        assert_eq!(*drops.lock().unwrap(), [std::thread::current().id()]);
    }

    /// An exiting owner cannot destroy a resource still owned by another thread.
    #[test]
    fn a_handle_can_outlive_its_owner_without_foreign_destruction() {
        let drops = Arc::new(Mutex::new(Vec::new()));
        let observed = drops.clone();
        let value = std::thread::spawn(move || MainThread::new(Dropped(observed)))
            .join()
            .unwrap();
        drop(value);
        reclaim_main_thread();
        assert!(drops.lock().unwrap().is_empty());
    }

    /// Thread exit drains resources already returned by other threads.
    #[test]
    fn the_owner_drains_returns_before_exiting() {
        let drops = Arc::new(Mutex::new(Vec::new()));
        let observed = drops.clone();
        let owner = std::thread::spawn(move || {
            let value = MainThread::new(Dropped(observed));
            std::thread::spawn(move || drop(value)).join().unwrap();
            std::thread::current().id()
        })
        .join()
        .unwrap();
        assert_eq!(*drops.lock().unwrap(), [owner]);
    }
}
