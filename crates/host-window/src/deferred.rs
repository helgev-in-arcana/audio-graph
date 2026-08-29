//! Mechanism for deferring execution to the next turn of the platform message loop.
//!
//! A GUI toolkit's draw callback is not a safe place to create, show or destroy
//! a window. Those calls dispatch messages synchronously, and the message lands
//! back inside the toolkit while it is still in the middle of the frame that
//! started the whole thing. egui, through egui-baseview, holds a `RefCell`
//! borrow across the draw callback and panics outright when that happens:
//!
//! ```text
//! pump_events -> egui-baseview renders (inner.borrow_mut() held)
//!             -> our ui() opens the sub-plugin's window
//!             -> ShowWindow dispatches a message to the egui window
//!             -> egui-baseview::on_event -> inner.borrow() -> panic
//! ```
//!
//! The rule that follows is simple and applies to every toolkit, not just this
//! one: **the draw callback may only record what the user asked for.**
//! [`Deferred`] is where the recorded work goes. A message-only window takes a
//! posted message, and by the time its handler runs the frame is over and the
//! toolkit is no longer inside itself.
//!
//! It carries one-shot work only. The periodic tick a sub-plugin's window needs
//! used to live here as well, on a Win32 timer, which quietly meant no tick at
//! all on the platforms whose backend is still a stub. It belongs to the plugin
//! instance now, which reaches the main thread through its host rather than
//! through a window — see `audio-graph-plugin`'s `tick`.
//!
//! # Which thread the work runs on
//!
//! The queue runs its work on the thread that created it, and that thread is
//! the caller's to choose. On Windows and macOS a plugin editor is built on the
//! host's own UI thread, so the two coincide. On X11 baseview gives the editor a
//! thread of its own, and a queue created there is *not* on the host's main
//! thread — see [`crate::pump_events`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::imp;

/// Work queued from a draw callback, run from the message loop.
///
/// Main thread only, and dropping it cancels anything still queued.
pub struct Deferred {
    inner: Rc<Inner>,
    handle: imp::DeferredHandle,
}

pub(crate) struct Inner {
    queue: RefCell<VecDeque<Box<dyn FnOnce()>>>,
    /// Guards against a queued closure that dispatches messages causing its own
    /// handler to be entered again.
    running: Cell<bool>,
}

impl Inner {
    pub(crate) fn new() -> Rc<Inner> {
        Rc::new(Inner {
            queue: RefCell::new(VecDeque::new()),
            running: Cell::new(false),
        })
    }

    pub(crate) fn drain(&self) {
        if self.running.get() {
            return;
        }
        self.running.set(true);
        loop {
            let Some(task) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            task();
        }
        self.running.set(false);
    }

    pub(crate) fn clear(&self) {
        self.queue.borrow_mut().clear();
    }
}

impl Deferred {
    pub(crate) fn from_parts(inner: Rc<Inner>, handle: imp::DeferredHandle) -> Deferred {
        Deferred { inner, handle }
    }

    /// Queue `task` to run once, on the next turn of this thread's message loop.
    ///
    /// Safe to call from inside a draw callback — that is the entire point.
    pub fn post(&self, task: impl FnOnce() + 'static) {
        self.inner.queue.borrow_mut().push_back(Box::new(task));
        imp::wake_deferred(&self.handle);
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        imp::destroy_deferred(&self.handle);
        // Anything still queued refers to state that is going away with the
        // owner of this queue.
        self.inner.clear();
    }
}

/// Create a queue bound to this thread's message loop.
pub fn new() -> Result<Deferred, String> {
    imp::new_deferred()
}

#[cfg(all(test, any(windows, all(unix, not(target_os = "macos")))))]
mod tests {
    use super::*;

    #[test]
    fn posted_work_waits_for_the_message_loop() {
        let deferred = new().expect("create");
        let ran = Rc::new(Cell::new(false));
        let flag = ran.clone();
        deferred.post(move || flag.set(true));

        // Nothing has pumped yet, so nothing has run — which is the whole
        // guarantee this type exists to provide.
        assert!(!ran.get());

        crate::pump_events();
        assert!(ran.get());
    }

    #[test]
    fn work_queued_from_inside_a_task_still_runs() {
        let deferred = new().expect("create");
        let count = Rc::new(Cell::new(0u32));

        let c = count.clone();
        deferred.post(move || c.set(c.get() + 1));
        let c = count.clone();
        deferred.post(move || c.set(c.get() + 1));

        crate::pump_events();
        assert_eq!(count.get(), 2);
    }
}
