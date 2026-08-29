//! Host-side `IPlugFrame` implementation.
//!
//! Receives plugin-initiated resize requests via `IPlugFrame::resizeView` and records
//! them to be applied during the next UI tick. Resizing a window synchronously from a
//! plugin callback re-enters the plugin while it is mid-call, which would crash.
//!
//! On Linux the same object is also the host's `IRunLoop` — see [`run_loop`].

use std::cell::Cell;

use vst3::Steinberg::{IPlugFrame, IPlugFrameTrait, IPlugView, ViewRect, kResultOk, tresult};
use vst3::{Class, ComWrapper};

use host_window::Size;

/// Host-side `IPlugFrame`.
pub struct PlugFrame {
    /// A size the plugin asked for and the host has not applied yet.
    requested: Cell<Option<Size>>,
    /// The last size we told the plugin about, so a user-driven resize is only
    /// reported when it actually changed.
    last_reported: Cell<Size>,
    #[cfg(all(unix, not(target_os = "macos")))]
    run_loop: run_loop::RunLoop,
    wrapper: std::cell::OnceCell<ComWrapper<FrameImpl>>,
}

/// The COM object itself, separate so `PlugFrame` can be held by value.
pub struct FrameImpl {
    requested: *const Cell<Option<Size>>,
    #[cfg(all(unix, not(target_os = "macos")))]
    run_loop: *const run_loop::RunLoop,
}

// SAFETY: the pointer refers to a `PlugFrame` that outlives this object —
// `EditorWindow` owns both and drops the frame last. All access happens on the
// UI thread, which is where VST3 confines `IPlugFrame` calls.
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
        unsafe { (*self.requested).set(Some(size)) };
        kResultOk
    }
}

impl PlugFrame {
    pub fn new() -> std::rc::Rc<PlugFrame> {
        let frame = std::rc::Rc::new(PlugFrame {
            requested: Cell::new(None),
            last_reported: Cell::new(Size::default()),
            #[cfg(all(unix, not(target_os = "macos")))]
            run_loop: run_loop::RunLoop::default(),
            wrapper: std::cell::OnceCell::new(),
        });
        let requested: *const Cell<Option<Size>> = &frame.requested;
        #[cfg(all(unix, not(target_os = "macos")))]
        let run_loop: *const run_loop::RunLoop = &frame.run_loop;
        let _ = frame.wrapper.set(ComWrapper::new(FrameImpl {
            requested,
            #[cfg(all(unix, not(target_os = "macos")))]
            run_loop,
        }));
        frame
    }

    /// Borrowed interface pointer to hand to `IPlugView::setFrame`.
    ///
    /// Borrowed, not owned: the plugin does not release what it is given here,
    /// so this object has to outlive the view's use of it.
    pub fn com_ptr(&self) -> *mut IPlugFrame {
        self.wrapper
            .get()
            .and_then(|w| w.as_com_ref::<IPlugFrame>())
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }

    /// Take a pending resize request, if the plugin made one.
    pub fn take_requested_size(&self) -> Option<Size> {
        self.requested.take()
    }

    pub fn last_reported_size(&self) -> Size {
        self.last_reported.get()
    }

    pub fn set_last_reported_size(&self, size: Size) {
        self.last_reported.set(size);
    }

    /// Service whatever the plugin registered with the run loop.
    ///
    /// Call once per UI tick, on the thread the editor was opened from. Does
    /// nothing where there is no run loop to be — see [`run_loop`].
    pub fn tick_run_loop(&self) {
        #[cfg(all(unix, not(target_os = "macos")))]
        self.run_loop.tick();
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
#[cfg(all(unix, not(target_os = "macos")))]
mod run_loop {
    use std::cell::RefCell;
    use std::os::fd::RawFd;
    use std::time::{Duration, Instant};

    use vst3::Steinberg::Linux::{
        FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoopTrait, ITimerHandler,
        ITimerHandlerTrait, TimerInterval,
    };
    use vst3::Steinberg::{kInvalidArgument, kResultOk, tresult};

    use super::FrameImpl;

    /// The shortest period a timer will actually be run at.
    ///
    /// A plugin asking for zero means "as often as you can", and taking that
    /// literally would spend the UI thread on one plugin's timer.
    const MIN_PERIOD: Duration = Duration::from_millis(8);

    /// How many of each a single plugin may register.
    ///
    /// Generous for anything legitimate, and a ceiling on what a plugin that
    /// registers in a loop and never unregisters can cost the UI thread.
    const MAX_HANDLERS: usize = 32;

    struct Timer {
        handler: *mut ITimerHandler,
        period: Duration,
        due: Instant,
    }

    #[derive(Default)]
    pub(super) struct RunLoop {
        /// Watched descriptors and who to tell when one is readable.
        events: RefCell<Vec<(*mut IEventHandler, RawFd)>>,
        timers: RefCell<Vec<Timer>>,
    }

    impl RunLoop {
        /// Tell the plugin about every descriptor that has data and every timer
        /// that has come due.
        pub(super) fn tick(&self) {
            self.poll_descriptors();
            self.fire_timers();
        }

        fn poll_descriptors(&self) {
            let watched = self.events.borrow().clone();
            if watched.is_empty() {
                return;
            }

            let mut fds: Vec<libc::pollfd> = watched
                .iter()
                .map(|(_, fd)| libc::pollfd {
                    fd: *fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // Zero timeout: this runs from the host's UI tick, which has a frame
            // to get back to.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
            if ready <= 0 {
                return;
            }

            for (index, poll) in fds.iter().enumerate() {
                if poll.revents == 0 {
                    continue;
                }
                let (handler, fd) = watched[index];
                // A handler is entitled to unregister another from inside its
                // own callback, so each one is checked against the live list
                // rather than against the copy this loop is walking.
                if !self.watches(handler, fd) {
                    continue;
                }
                if let Some(handler) = unsafe { vst3::ComRef::from_raw(handler) } {
                    unsafe { handler.onFDIsSet(fd) };
                }
            }
        }

        fn fire_timers(&self) {
            let now = Instant::now();
            let due: Vec<*mut ITimerHandler> = self
                .timers
                .borrow_mut()
                .iter_mut()
                .filter(|timer| timer.due <= now)
                .map(|timer| {
                    // From now, not from when it was due: a UI thread that
                    // stalled must not come back to a burst of catch-up ticks.
                    timer.due = now + timer.period;
                    timer.handler
                })
                .collect();

            for handler in due {
                if !self.has_timer(handler) {
                    continue;
                }
                if let Some(handler) = unsafe { vst3::ComRef::from_raw(handler) } {
                    unsafe { handler.onTimer() };
                }
            }
        }

        fn watches(&self, handler: *mut IEventHandler, fd: RawFd) -> bool {
            self.events.borrow().contains(&(handler, fd))
        }

        fn has_timer(&self, handler: *mut ITimerHandler) -> bool {
            self.timers.borrow().iter().any(|t| t.handler == handler)
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
            let run_loop = unsafe { &*self.run_loop };
            let mut events = run_loop.events.borrow_mut();
            if events.len() >= MAX_HANDLERS {
                log::warn!("vst3 plugin asked to watch more than {MAX_HANDLERS} descriptors");
                return kInvalidArgument;
            }
            // Registering the same pair twice would have it told twice per turn.
            if !events.contains(&(handler, fd)) {
                events.push((handler, fd));
            }
            kResultOk
        }

        unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
            let run_loop = unsafe { &*self.run_loop };
            run_loop
                .events
                .borrow_mut()
                .retain(|(other, _)| *other != handler);
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
            let run_loop = unsafe { &*self.run_loop };
            let mut timers = run_loop.timers.borrow_mut();
            if timers.len() >= MAX_HANDLERS {
                log::warn!("vst3 plugin asked for more than {MAX_HANDLERS} timers");
                return kInvalidArgument;
            }
            let period = Duration::from_millis(milliseconds).max(MIN_PERIOD);
            timers.push(Timer {
                handler,
                period,
                due: Instant::now() + period,
            });
            kResultOk
        }

        unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
            let run_loop = unsafe { &*self.run_loop };
            run_loop
                .timers
                .borrow_mut()
                .retain(|t| t.handler != handler);
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
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_frame_is_also_the_run_loop() {
        use vst3::Steinberg::Linux::{IRunLoop, IRunLoopTrait};

        let frame = PlugFrame::new();
        let plug_frame = unsafe { vst3::ComRef::<IPlugFrame>::from_raw(frame.com_ptr()) }
            .expect("frame pointer");
        let run_loop = plug_frame
            .cast::<IRunLoop>()
            .expect("the frame is a run loop");

        // A descriptor that is never readable, so the tick has something to poll
        // and nothing to report.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "could not make an eventfd to watch");

        // Registered as a handler that is never called: `onFDIsSet` only runs
        // for a descriptor with data, and this one has none.
        let handler = plug_frame
            .as_ptr()
            .cast::<vst3::Steinberg::Linux::IEventHandler>();
        assert_eq!(
            unsafe { run_loop.registerEventHandler(handler, fd) },
            kResultOk
        );
        frame.tick_run_loop();
        assert_eq!(
            unsafe { run_loop.unregisterEventHandler(handler) },
            kResultOk
        );
        frame.tick_run_loop();

        unsafe { libc::close(fd) };
    }
}
