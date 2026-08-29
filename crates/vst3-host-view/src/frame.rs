//! Host-side `IPlugFrame` implementation.
//!
//! Receives plugin-initiated resize requests via `IPlugFrame::resizeView` and records
//! them to be applied during the next UI tick. Resizing a window synchronously from a
//! plugin callback re-enters the plugin while it is mid-call, which would crash.
//!
//! On Linux the same object is also the host's `IRunLoop` — see [`run_loop`].
//!
//! # Why the state lives in the COM object
//!
//! Everything the frame remembers is a field of [`FrameImpl`], which is what
//! `ComWrapper` refcounts, rather than of a `PlugFrame` the wrapper points back
//! into. A plugin that queries `IRunLoop` holds it for its own lifetime —
//! unregistering is the last thing it does with it — so it can outlive the
//! `EditorWindow` that set the frame. Held this way it cannot outlive the state
//! it calls into, because they are the same allocation.

use std::cell::Cell;

use vst3::Steinberg::{IPlugFrame, IPlugFrameTrait, IPlugView, ViewRect, kResultOk, tresult};
use vst3::{Class, ComWrapper};

use host_window::Size;

/// Host-side `IPlugFrame`, and on Linux the host's `IRunLoop` with it.
///
/// A handle, not the object: what the plugin holds references to is the
/// `FrameImpl` inside, and it outlives this if the plugin kept one.
pub struct PlugFrame(ComWrapper<FrameImpl>);

/// The COM object, and everything the frame remembers.
pub struct FrameImpl {
    /// A size the plugin asked for and the host has not applied yet.
    requested: Cell<Option<Size>>,
    /// The last size we told the plugin about, so a user-driven resize is only
    /// reported when it actually changed.
    last_reported: Cell<Size>,
    #[cfg(all(unix, not(target_os = "macos")))]
    run_loop: run_loop::RunLoop,
}

// SAFETY: `Cell` is not `Sync`, and this object has to be because the COM
// wrapper is. VST3 confines every call on `IPlugFrame` — and, on Linux, on
// `IRunLoop` — to the UI thread, which is also the only thread `EditorWindow`
// touches it from, so there is never a second thread to race with.
unsafe impl Send for FrameImpl {}
unsafe impl Sync for FrameImpl {}

#[cfg(not(all(unix, not(target_os = "macos"))))]
impl Class for FrameImpl {
    type Interfaces = (IPlugFrame,);
}

/// On Linux a plugin reaches its file descriptors and timers only through the
/// frame, so the two interfaces are one object — see [`run_loop`].
#[cfg(all(unix, not(target_os = "macos")))]
impl Class for FrameImpl {
    type Interfaces = (IPlugFrame, vst3::Steinberg::Linux::IRunLoop);
}

impl IPlugFrameTrait for FrameImpl {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() {
            return kResultOk;
        }
        let rect = unsafe { *new_size };
        let size = Size::new(rect.right - rect.left, rect.bottom - rect.top);
        // Recorded for the next UI tick; see the module comment on why this is
        // not applied inline.
        self.requested.set(Some(size));
        kResultOk
    }
}

impl PlugFrame {
    pub fn new() -> PlugFrame {
        PlugFrame(ComWrapper::new(FrameImpl {
            requested: Cell::new(None),
            last_reported: Cell::new(Size::default()),
            #[cfg(all(unix, not(target_os = "macos")))]
            run_loop: run_loop::RunLoop::default(),
        }))
    }

    /// Borrowed interface pointer to hand to `IPlugView::setFrame`.
    ///
    /// Borrowed, not owned: the plugin does not release what it is given here.
    /// What it *does* release is anything it queries off it, and that keeps the
    /// object alive on its own — see the module comment.
    pub fn com_ptr(&self) -> *mut IPlugFrame {
        self.0
            .as_com_ref::<IPlugFrame>()
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }

    /// Take a pending resize request, if the plugin made one.
    pub fn take_requested_size(&self) -> Option<Size> {
        self.0.requested.take()
    }

    pub fn last_reported_size(&self) -> Size {
        self.0.last_reported.get()
    }

    pub fn set_last_reported_size(&self, size: Size) {
        self.0.last_reported.set(size);
    }

    /// Service whatever the plugin registered with the run loop.
    ///
    /// Call once per UI tick, on the thread the editor was opened from. Does
    /// nothing where there is no run loop to be — see [`run_loop`].
    pub fn tick_run_loop(&self) {
        #[cfg(all(unix, not(target_os = "macos")))]
        self.0.run_loop.dispatch();
    }
}

impl Default for PlugFrame {
    fn default() -> PlugFrame {
        PlugFrame::new()
    }
}

/// The host's `IRunLoop`, which on Linux only the plugin frame can provide.
///
/// A Linux plugin has no event loop of its own to wait on: the host owns the
/// one X connection the process is drawing from, so a plugin that wants to be
/// told about a file descriptor or a timer has to ask the host to watch it. The
/// interface is queried off `IPlugFrame`, which is why it lives here rather than
/// with the window.
///
/// nice-plug, which the wrapper itself is built on, hands the host a socket this
/// way and posts its main-thread work through it — so without this the wrapper
/// loaded as a VST3 on Linux has no route to the main thread at all. Third-party
/// plugins are less forgiving still: several refuse to open an editor when the
/// interface is missing.
///
/// The bookkeeping is `host_window::watch`, shared with the CLAP backend, which
/// is asked for the same thing under a different name. Only the vocabulary is
/// here.
#[cfg(all(unix, not(target_os = "macos")))]
mod run_loop {
    use std::time::Duration;

    use host_window::watch::{FdWatch, Interest, TimerWheel};
    use vst3::Steinberg::Linux::{
        FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoopTrait, ITimerHandler,
        ITimerHandlerTrait, TimerInterval,
    };
    use vst3::Steinberg::{kInvalidArgument, kResultOk, tresult};

    use super::FrameImpl;

    /// The handlers are the keys: VST3 unregisters by handler, not by id.
    #[derive(Default)]
    pub(super) struct RunLoop {
        events: FdWatch<*mut IEventHandler>,
        timers: TimerWheel<*mut ITimerHandler>,
    }

    // SAFETY: the same argument as `FrameImpl`'s — these are COM pointers the
    // UI thread owns and no other thread ever sees.
    unsafe impl Send for RunLoop {}
    unsafe impl Sync for RunLoop {}

    impl RunLoop {
        /// Tell the plugin about every descriptor that has data and every timer
        /// that has come due.
        pub(super) fn dispatch(&self) {
            self.events.dispatch(|handler, fd, _| {
                if let Some(handler) = unsafe { vst3::ComRef::from_raw(handler) } {
                    unsafe { handler.onFDIsSet(fd) };
                }
            });
            self.timers.dispatch(|handler| {
                if let Some(handler) = unsafe { vst3::ComRef::from_raw(handler) } {
                    unsafe { handler.onTimer() };
                }
            });
        }
    }

    impl IRunLoopTrait for FrameImpl {
        unsafe fn registerEventHandler(
            &self,
            handler: *mut IEventHandler,
            fd: FileDescriptor,
        ) -> tresult {
            if handler.is_null() {
                return kInvalidArgument;
            }
            // A refused registration is still `kResultOk`: VST3 says nothing
            // about what refusal looks like and nice-plug asserts on the
            // result, so a full table hands the plugin a descriptor that is
            // never ready rather than aborting the process.
            self.run_loop.events.watch(handler, fd, Interest::READ);
            kResultOk
        }

        unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
            // By handler, which is all VST3 gives us.
            self.run_loop.events.forget_by(|key, _| *key == handler);
            kResultOk
        }

        unsafe fn registerTimer(
            &self,
            handler: *mut ITimerHandler,
            milliseconds: TimerInterval,
        ) -> tresult {
            if handler.is_null() {
                return kInvalidArgument;
            }
            self.run_loop
                .timers
                .arm(handler, Duration::from_millis(milliseconds));
            kResultOk
        }

        unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
            self.run_loop.timers.disarm(handler);
            kResultOk
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_request_is_recorded_for_the_next_tick() {
        let frame = PlugFrame::new();
        assert!(frame.take_requested_size().is_none());

        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 640,
            bottom: 480,
        };
        let ptr = frame.com_ptr();
        assert!(!ptr.is_null());
        unsafe {
            let com = vst3::ComRef::<IPlugFrame>::from_raw(ptr).expect("frame pointer");
            com.resizeView(std::ptr::null_mut(), &mut rect);
        }

        assert_eq!(frame.take_requested_size(), Some(Size::new(640, 480)));
        // Taken means taken: applying the same resize twice would fight a user
        // who dragged the window in between.
        assert!(frame.take_requested_size().is_none());
    }

    #[test]
    fn a_null_rect_is_ignored_rather_than_dereferenced() {
        let frame = PlugFrame::new();
        unsafe {
            let com = vst3::ComRef::<IPlugFrame>::from_raw(frame.com_ptr()).unwrap();
            assert_eq!(
                com.resizeView(std::ptr::null_mut(), std::ptr::null_mut()),
                kResultOk
            );
        }
        assert!(frame.take_requested_size().is_none());
    }

    /// nice-plug asserts on the result of `registerEventHandler`, and a plugin
    /// that finds no run loop at all has no route to the main thread — so the
    /// frame has to answer to the interface, not merely exist.
    ///
    /// What happens to a descriptor once registered is `host_window::watch`'s
    /// to test, and it does.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_frame_is_also_the_run_loop() {
        use vst3::Steinberg::Linux::{IEventHandler, IRunLoop, IRunLoopTrait, ITimerHandler};

        let frame = PlugFrame::new();
        let plug_frame =
            unsafe { vst3::ComRef::<IPlugFrame>::from_raw(frame.com_ptr()) }.expect("frame");
        let run_loop = plug_frame
            .cast::<IRunLoop>()
            .expect("the frame is a run loop");

        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "could not make an eventfd to watch");

        let events = plug_frame.as_ptr().cast::<IEventHandler>();
        let timers = plug_frame.as_ptr().cast::<ITimerHandler>();
        unsafe {
            assert_eq!(run_loop.registerEventHandler(events, fd), kResultOk);
            assert_eq!(run_loop.registerTimer(timers, 16), kResultOk);
            frame.tick_run_loop();
            assert_eq!(run_loop.unregisterEventHandler(events), kResultOk);
            assert_eq!(run_loop.unregisterTimer(timers), kResultOk);
            frame.tick_run_loop();
            libc::close(fd);
        }
    }

    /// Why the state moved into the COM object: a plugin holds the run loop
    /// until its own teardown, which is after the host has let the frame go.
    /// Calling into it then must not touch freed memory.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_run_loop_outlives_the_handle_that_made_it() {
        use vst3::Steinberg::Linux::{IRunLoop, IRunLoopTrait, ITimerHandler};

        let frame = PlugFrame::new();
        let held = unsafe { vst3::ComRef::<IPlugFrame>::from_raw(frame.com_ptr()) }
            .expect("frame")
            .cast::<IRunLoop>()
            .expect("run loop");

        // What `EditorWindow` does when the editor closes.
        drop(frame);

        // What a plugin does afterwards, on the reference it kept.
        let handler = held.as_ptr().cast::<ITimerHandler>();
        unsafe {
            assert_eq!(held.registerTimer(handler, 16), kResultOk);
            assert_eq!(held.unregisterTimer(handler), kResultOk);
        }
    }
}
