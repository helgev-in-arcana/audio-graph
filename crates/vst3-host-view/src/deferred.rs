//! Running work on the next turn of somebody else's message loop.
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
//! one: **the draw callback may only record what the user asked for.** This
//! type is where the recorded work goes. A message-only window takes a posted
//! message, and by the time its handler runs, the frame is over and the toolkit
//! is no longer inside itself.
//!
//! It also carries the periodic tick the sub-plugin's window needs (§5.2). That
//! cannot live in the draw callback either, for exactly the same reason:
//! answering `resizeView` means resizing a window.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

/// Work queued from a draw callback, run from the message loop.
///
/// Main thread only, and dropping it cancels anything still queued.
pub struct Deferred {
    inner: Rc<Inner>,
    #[cfg(windows)]
    hwnd: imp::Hwnd,
}

struct Inner {
    queue: RefCell<VecDeque<Box<dyn FnOnce()>>>,
    tick: RefCell<Option<Box<dyn Fn()>>>,
    /// Guards against a queued closure that dispatches messages causing its own
    /// handler to be entered again.
    running: Cell<bool>,
}

impl Inner {
    fn drain(&self) {
        if self.running.get() {
            return;
        }
        self.running.set(true);
        loop {
            let Some(task) = self.queue.borrow_mut().pop_front() else { break };
            task();
        }
        self.running.set(false);
    }

    fn tick(&self) {
        if self.running.get() {
            return;
        }
        self.running.set(true);
        // Cloned out of the `RefCell` before running: a tick that queues more
        // work must not find the cell already borrowed.
        let tick = self.tick.borrow_mut().take();
        if let Some(tick) = tick {
            tick();
            *self.tick.borrow_mut() = Some(tick);
        }
        self.running.set(false);
    }
}

impl Deferred {
    /// Queue `task` to run once, on the next turn of this thread's message loop.
    ///
    /// Safe to call from inside a draw callback — that is the entire point.
    pub fn post(&self, task: impl FnOnce() + 'static) {
        self.inner.queue.borrow_mut().push_back(Box::new(task));
        self.wake();
    }

    /// Run `tick` every `interval_ms`, on this thread.
    ///
    /// Replaces any previous tick. Used for the sub-plugin editor's resize
    /// conversation, which has to keep happening whether or not the user is
    /// touching anything.
    pub fn set_tick(&self, interval_ms: u32, tick: impl Fn() + 'static) {
        *self.inner.tick.borrow_mut() = Some(Box::new(tick));
        self.start_timer(interval_ms);
    }
}

#[cfg(windows)]
pub use imp::new;

#[cfg(windows)]
impl Deferred {
    fn wake(&self) {
        imp::wake(self.hwnd);
    }

    fn start_timer(&self, interval_ms: u32) {
        imp::start_timer(self.hwnd, interval_ms);
    }
}

#[cfg(not(windows))]
impl Deferred {
    fn wake(&self) {
        // Without a message loop to post to there is nowhere to defer *to*, so
        // the work runs now. Correct only because the non-Windows window
        // backend is a stub that never opens anything (see `window`).
        self.inner.drain();
    }

    fn start_timer(&self, _interval_ms: u32) {}
}

/// Create a queue bound to this thread's message loop.
#[cfg(not(windows))]
pub fn new() -> Result<Deferred, String> {
    Ok(Deferred {
        inner: Rc::new(Inner {
            queue: RefCell::new(VecDeque::new()),
            tick: RefCell::new(None),
            running: Cell::new(false),
        }),
    })
}

#[cfg(windows)]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
        HWND_MESSAGE, KillTimer, PostMessageW, RegisterClassExW, SetTimer, SetWindowLongPtrW,
        WM_APP, WM_TIMER, WNDCLASSEXW,
    };

    use super::{Deferred, Inner};

    pub type Hwnd = HWND;

    /// The timer id. Any non-zero value; there is only ever one.
    const TIMER_ID: usize = 1;

    const CLASS_NAME: &[u16] = &[
        b'a' as u16, b'u' as u16, b'd' as u16, b'i' as u16, b'o' as u16, b'g' as u16, b'r' as u16,
        b'a' as u16, b'p' as u16, b'h' as u16, b'.' as u16, b'd' as u16, b'e' as u16, b'f' as u16,
        b'e' as u16, b'r' as u16, 0,
    ];

    static REGISTER: Once = Once::new();

    fn register() {
        REGISTER.call_once(|| unsafe {
            let mut class: WNDCLASSEXW = std::mem::zeroed();
            class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            class.lpfnWndProc = Some(wnd_proc);
            class.hInstance = GetModuleHandleW(std::ptr::null());
            class.lpszClassName = CLASS_NAME.as_ptr();
            RegisterClassExW(&class);
        });
    }

    /// Create a queue bound to this thread's message loop.
    pub fn new() -> Result<Deferred, String> {
        register();
        let inner = Rc::new(Inner {
            queue: RefCell::new(VecDeque::new()),
            tick: RefCell::new(None),
            running: Cell::new(false),
        });

        // HWND_MESSAGE: no pixels, no z-order, never shown. It exists purely to
        // own a place in the thread's message queue.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                CLASS_NAME.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("could not create the deferred-work window".into());
        }
        // A borrowed pointer, not an owned one: `Deferred` holds the `Rc` and
        // clears this in `Drop` before the window goes away.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::as_ptr(&inner) as isize) };

        Ok(Deferred { inner, hwnd })
    }

    pub fn wake(hwnd: HWND) {
        unsafe { PostMessageW(hwnd, WM_APP, 0, 0) };
    }

    pub fn start_timer(hwnd: HWND, interval_ms: u32) {
        unsafe { SetTimer(hwnd, TIMER_ID, interval_ms, None) };
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            let inner = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Inner;
            if inner.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            match msg {
                WM_APP => {
                    (*inner).drain();
                    0
                }
                WM_TIMER => {
                    (*inner).tick();
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    impl Drop for Deferred {
        fn drop(&mut self) {
            unsafe {
                KillTimer(self.hwnd, TIMER_ID);
                // Clear the back-pointer before destroying: DestroyWindow
                // dispatches synchronously, and anything still queued refers to
                // state that is going away.
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                DestroyWindow(self.hwnd);
            }
            self.inner.queue.borrow_mut().clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[cfg(windows)]
    #[test]
    fn posted_work_waits_for_the_message_loop() {
        let deferred = new().expect("create");
        let ran = Rc::new(Cell::new(false));
        let flag = ran.clone();
        deferred.post(move || flag.set(true));

        // Nothing has pumped yet, so nothing has run — which is the whole
        // guarantee this type exists to provide.
        assert!(!ran.get());

        crate::window::pump_events();
        assert!(ran.get());
    }

    #[cfg(windows)]
    #[test]
    fn work_queued_from_inside_a_task_still_runs() {
        let deferred = new().expect("create");
        let count = Rc::new(Cell::new(0u32));

        let c = count.clone();
        deferred.post(move || c.set(c.get() + 1));
        let c = count.clone();
        deferred.post(move || c.set(c.get() + 1));

        crate::window::pump_events();
        assert_eq!(count.get(), 2);
    }
}
