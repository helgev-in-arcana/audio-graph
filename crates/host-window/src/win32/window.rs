//! Win32 container window.
//!
//! A window belongs to the thread that created it and the DAW pumps its
//! messages, so there is no event loop of our own to run.

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GA_ROOT, GWLP_USERDATA, GetAncestor, GetSystemMetrics,
    GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG, PM_REMOVE, PeekMessageW, RegisterClassExW,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, WM_CLOSE, WM_NCCREATE, WM_SIZE, WNDCLASSEXW, WS_CLIPCHILDREN,
    WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
};

use crate::window::{Size, WindowState};

const CLASS_NAME: &[u16] = &[
    b'A' as u16,
    b'u' as u16,
    b'd' as u16,
    b'i' as u16,
    b'o' as u16,
    b'G' as u16,
    b'r' as u16,
    b'a' as u16,
    b'p' as u16,
    b'h' as u16,
    b'S' as u16,
    b'u' as u16,
    b'b' as u16,
    b'V' as u16,
    b'i' as u16,
    b'e' as u16,
    b'w' as u16,
    0,
];

/// Registering the same class twice fails, and a plugin may be instantiated
/// many times in one process, so it is registered once per module.
fn ensure_class_registered() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // Not CS_OWNDC or CS_HREDRAW: the plugin's own child window
            // does all the drawing, and repainting the container behind it
            // only causes flicker.
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: COLOR_WINDOW as HBRUSH,
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&class);
    });
}

pub(crate) struct Window {
    hwnd: Cell<HWND>,
    /// Kept alive for as long as the window can still receive messages: the
    /// window procedure holds a raw pointer into it.
    _state: Rc<WindowState>,
}

impl Window {
    pub(crate) fn new(
        title: &str,
        size: Size,
        owner: HWND,
        state: Rc<WindowState>,
    ) -> Result<Window, String> {
        ensure_class_registered();

        let mut wide: Vec<u16> = title.encode_utf16().collect();
        wide.push(0);

        // The requested size is the *client* area the plugin wants; the
        // window has to be larger by its frame or the editor is cropped.
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: size.width,
            bottom: size.height,
        };
        unsafe {
            AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, WS_EX_APPWINDOW);
        }

        // Roughly centred. A window that opens under the DAW's own is worse
        // than one that opens in a slightly odd place.
        let (screen_w, screen_h) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let (x, y) = if screen_w > 0 && screen_h > 0 {
            (
                ((screen_w - width) / 2).max(0),
                ((screen_h - height) / 2).max(0),
            )
        } else {
            (CW_USEDEFAULT, CW_USEDEFAULT)
        };

        let state_ptr = Rc::as_ptr(&state);
        let hwnd = unsafe {
            CreateWindowExW(
                // No WS_EX_APPWINDOW once there is an owner: an owned
                // window that also insists on its own taskbar button is a
                // dialog pretending to be an application.
                if owner.is_null() { WS_EX_APPWINDOW } else { 0 },
                CLASS_NAME.as_ptr(),
                wide.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                x,
                y,
                width,
                height,
                // With WS_OVERLAPPEDWINDOW this argument is the *owner*,
                // not a parent: the window stays top-level and keeps its
                // title bar, but sits in front of the owner instead of
                // behind it. See `ContainerWindow::new` for which window
                // this has to be.
                owner,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                state_ptr as *mut c_void,
            )
        };

        if hwnd.is_null() {
            return Err(format!(
                "CreateWindowEx failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(Window {
            hwnd: Cell::new(hwnd),
            _state: state,
        })
    }

    pub(crate) fn handle(&self) -> *mut c_void {
        self.hwnd.get()
    }

    pub(crate) fn set_client_size(&self, size: Size) {
        let hwnd = self.hwnd.get();
        if hwnd.is_null() {
            return;
        }
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: size.width,
            bottom: size.height,
        };
        unsafe {
            AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, WS_EX_APPWINDOW);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER,
            );
        }
    }

    pub(crate) fn show(&self) {
        let hwnd = self.hwnd.get();
        if !hwnd.is_null() {
            unsafe { ShowWindow(hwnd, SW_SHOW) };
        }
    }

    pub(crate) fn scale_factor(&self) -> f64 {
        let hwnd = self.hwnd.get();
        if hwnd.is_null() {
            return 1.0;
        }
        // GetDpiForWindow reports the DPI this window is actually being
        // shown at, which is what matters on a multi-monitor machine where
        // the system DPI is only one screen's answer.
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let hwnd = self.hwnd.replace(std::ptr::null_mut());
        if !hwnd.is_null() {
            unsafe {
                // Clear the back-pointer first: DestroyWindow sends
                // messages synchronously, and the state it points at is
                // about to go away.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DestroyWindow(hwnd);
            }
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = lparam as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        match msg {
            // Recorded, not obeyed. Destroying the window here would take
            // the plugin's child window with it without the plugin ever
            // being told; the owner tears things down in order.
            WM_CLOSE => {
                (*state).close_requested.set(true);
                0
            }
            WM_SIZE => {
                let width = (lparam & 0xFFFF) as i32;
                let height = ((lparam >> 16) & 0xFFFF) as i32;
                (*state).size.set(Size { width, height });
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// The top-level window `handle` belongs to.
pub(crate) fn root_window(handle: *mut c_void) -> *mut c_void {
    if handle.is_null() {
        return handle;
    }
    // GA_ROOT walks up parents but stops at the first top-level window,
    // which is the DAW's own frame rather than the desktop.
    unsafe { GetAncestor(handle, GA_ROOT) }
}

pub(crate) fn pump_events() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
