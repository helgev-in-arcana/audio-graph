//! Container window implementation for hosting plugin editors.
//!
//! Provides a top-level titled frame to embed child plugin view windows.

use std::cell::Cell;
use std::rc::Rc;

/// A rectangle in the coordinates both VST3 and the platform use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    pub fn new(width: i32, height: i32) -> Size {
        Size { width, height }
    }
}

/// Shared state a window's message handler needs to reach.
#[derive(Default)]
pub struct WindowState {
    /// Set when the user closes the window, so the owner can tear the view down
    /// in the right order rather than the OS destroying it underneath us.
    pub close_requested: Cell<bool>,
    /// Current client size, updated as the user resizes.
    pub size: Cell<Size>,
}

/// A top-level container window for hosting plugin editors.
pub struct ContainerWindow {
    inner: imp::Window,
    state: Rc<WindowState>,
}

impl ContainerWindow {
    /// Create a window sized to the editor's requested client area.
    ///
    /// # Platform note
    /// Must be called on the thread that owns the host's message pump. On
    /// Windows a window belongs to the thread that created it, and messages for
    /// it are only dispatched by that thread's pump.
    /// `owner` is the window this one should float above — the DAW's own
    /// top-level window when there is one, null when running standalone.
    ///
    /// Without an owner the window is a peer of the DAW's, so clicking anywhere
    /// in the DAW buries the plugin's editor behind it. With one it stays in
    /// front of the DAW and minimises with it, which is what every other plugin
    /// window does.
    ///
    /// It must be the DAW's root window, not the wrapper's editor view, so that
    /// closing the wrapper UI does not destroy the child window unexpectedly.
    pub fn new(
        title: &str,
        size: Size,
        owner: *mut std::ffi::c_void,
    ) -> Result<ContainerWindow, String> {
        let state = Rc::new(WindowState {
            close_requested: Cell::new(false),
            size: Cell::new(size),
        });
        let inner = imp::Window::new(title, size, owner, Rc::clone(&state))?;
        Ok(ContainerWindow { inner, state })
    }

    /// The handle to hand to `IPlugView::attached`.
    pub fn platform_handle(&self) -> *mut std::ffi::c_void {
        self.inner.handle()
    }

    pub fn state(&self) -> &Rc<WindowState> {
        &self.state
    }

    /// Resize the client area, for when the plugin asks via `resizeView`.
    pub fn set_client_size(&self, size: Size) {
        self.state.size.set(size);
        self.inner.set_client_size(size);
    }

    pub fn client_size(&self) -> Size {
        self.state.size.get()
    }

    pub fn show(&self) {
        self.inner.show();
    }

    /// Display scale for this window: 1.0 at 96 DPI, 1.5 at 144, and so on.
    pub fn scale_factor(&self) -> f64 {
        self.inner.scale_factor()
    }

    /// Whether the user has asked to close the window.
    pub fn close_requested(&self) -> bool {
        self.state.close_requested.get()
    }

    /// Dispatch any pending messages for this thread.
    ///
    /// Only for standalone use — a plugin must *not* call this, because the
    /// DAW's pump is already dispatching. It exists so the development harness
    /// can drive a window without a DAW.
    pub fn pump_events(&self) {
        imp::pump_events();
    }
}

/// Dispatch pending messages for the calling thread.
///
/// For standalone harnesses only. Inside a plugin the DAW owns the pump and
/// calling this would dispatch its messages on our behalf.
pub fn pump_events() {
    imp::pump_events();
}

/// The top-level window `handle` ultimately belongs to.
///
/// A plugin is handed a view somewhere deep inside the DAW's window tree, but
/// the window a sub-plugin editor should be owned by is the root — see
/// [`ContainerWindow::new`]. Returns null for a null handle.
pub fn root_window(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    imp::root_window(handle)
}

#[cfg(windows)]
mod imp {
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
        SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW,
        SetWindowPos, ShowWindow, TranslateMessage, WM_CLOSE, WM_NCCREATE, WM_SIZE, WNDCLASSEXW,
        WS_CLIPCHILDREN, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
    };

    use super::{Size, WindowState};

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

    pub struct Window {
        hwnd: Cell<HWND>,
        /// Kept alive for as long as the window can still receive messages: the
        /// window procedure holds a raw pointer into it.
        _state: Rc<WindowState>,
    }

    impl Window {
        pub fn new(
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

        pub fn handle(&self) -> *mut c_void {
            self.hwnd.get()
        }

        pub fn set_client_size(&self, size: Size) {
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

        pub fn show(&self) {
            let hwnd = self.hwnd.get();
            if !hwnd.is_null() {
                unsafe { ShowWindow(hwnd, SW_SHOW) };
            }
        }

        pub fn scale_factor(&self) -> f64 {
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
                // Record close request rather than immediately destroying window,
                // allowing proper teardown ordering by the owner.
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

    /// Drain this thread's message queue.
    pub fn root_window(handle: *mut c_void) -> *mut c_void {
        if handle.is_null() {
            return handle;
        }
        // GA_ROOT walks up parents but stops at the first top-level window,
        // which is the DAW's own frame rather than the desktop.
        unsafe { GetAncestor(handle, GA_ROOT) }
    }

    pub fn pump_events() {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::ffi::c_void;
    use std::rc::Rc;

    use super::{Size, WindowState};

    /// Stub implementation for non-Windows platforms.
    pub struct Window;

    impl Window {
        pub fn new(
            _title: &str,
            _size: Size,
            _owner: *mut c_void,
            _state: Rc<WindowState>,
        ) -> Result<Window, String> {
            Err("sub-plugin editor windows are only implemented on Windows so far".into())
        }
        pub fn handle(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn set_client_size(&self, _size: Size) {}
        pub fn show(&self) {}
        pub fn scale_factor(&self) -> f64 {
            1.0
        }
    }

    pub fn pump_events() {}

    pub fn root_window(handle: *mut c_void) -> *mut c_void {
        handle
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_window_can_be_created_and_dropped() {
        let window = ContainerWindow::new("test", Size::new(320, 240), std::ptr::null_mut())
            .expect("create");
        assert!(!window.platform_handle().is_null());
        assert!(!window.close_requested());
        window.pump_events();
        drop(window);
    }

    #[test]
    fn windows_can_be_created_repeatedly() {
        // The window class is registered once per process; a second
        // registration fails, so a naive implementation works exactly once.
        for _ in 0..3 {
            let window = ContainerWindow::new("test", Size::new(200, 100), std::ptr::null_mut())
                .expect("create");
            window.pump_events();
        }
    }
}
