//! Win32 side of the deferred queue.
//!
//! A message-only window holds a place in the thread's message queue. Posting
//! to it hands the work to the DAW's own pump, which runs it once the frame
//! that queued it is over.

use std::rc::Rc;
use std::sync::Once;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HWND_MESSAGE,
    PostMessageW, RegisterClassExW, SetWindowLongPtrW, WM_APP, WNDCLASSEXW,
};

use crate::deferred::{Deferred, Inner};

/// The message-only window the queue posts to.
pub(crate) type DeferredHandle = HWND;

const CLASS_NAME: &[u16] = &[
    b'a' as u16,
    b'u' as u16,
    b'd' as u16,
    b'i' as u16,
    b'o' as u16,
    b'g' as u16,
    b'r' as u16,
    b'a' as u16,
    b'p' as u16,
    b'h' as u16,
    b'.' as u16,
    b'd' as u16,
    b'e' as u16,
    b'f' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
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
pub(crate) fn new_deferred() -> Result<Deferred, String> {
    register();
    let inner = Inner::new();

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

    Ok(Deferred::from_parts(inner, hwnd))
}

pub(crate) fn wake_deferred(hwnd: &HWND) {
    unsafe { PostMessageW(*hwnd, WM_APP, 0, 0) };
}

pub(crate) fn destroy_deferred(hwnd: &HWND) {
    unsafe {
        // Clear the back-pointer before destroying: DestroyWindow dispatches
        // synchronously, and anything still queued refers to state that is
        // going away.
        SetWindowLongPtrW(*hwnd, GWLP_USERDATA, 0);
        DestroyWindow(*hwnd);
    }
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
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
