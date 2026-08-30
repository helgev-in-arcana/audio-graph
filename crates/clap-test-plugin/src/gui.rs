//! Minimal test GUI implementation for the fixture plugin.
//!
//! Provides a plain embedded child window implementation used to verify host GUI
//! embedding and lifecycle teardown behavior.

use std::ffi::{CStr, c_char, c_void};

use clap_sys::ext::gui::{clap_gui_resize_hints, clap_plugin_gui, clap_window};
use clap_sys::plugin::clap_plugin;

use crate::Instance;

/// Size the editor asks for, and the size a test asserts it got.
pub const WIDTH: u32 = 420;
pub const HEIGHT: u32 = 260;

pub static EXT_GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(is_api_supported),
    get_preferred_api: Some(get_preferred_api),
    create: Some(create),
    destroy: Some(destroy),
    set_scale: Some(set_scale),
    get_size: Some(get_size),
    can_resize: Some(can_resize),
    get_resize_hints: Some(get_resize_hints),
    adjust_size: Some(adjust_size),
    set_size: Some(set_size),
    set_parent: Some(set_parent),
    set_transient: Some(set_transient),
    suggest_title: Some(suggest_title),
    show: Some(show),
    hide: Some(hide),
};

/// The editor's state, as far as this fixture has any.
#[derive(Default)]
pub struct Gui {
    pub created: bool,
    pub visible: bool,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// The child window, once there is a parent to put it in.
    pub child: imp::Child,
}

unsafe extern "C" fn is_api_supported(
    _plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    if is_floating || api.is_null() {
        // Embedded only: floating windows are not managed by host lifecycle.
        return false;
    }
    (unsafe { CStr::from_ptr(api) }) == imp::WINDOW_API
}

unsafe extern "C" fn get_preferred_api(
    _plugin: *const clap_plugin,
    api: *mut *const c_char,
    is_floating: *mut bool,
) -> bool {
    if api.is_null() || is_floating.is_null() {
        return false;
    }
    unsafe {
        *api = imp::WINDOW_API.as_ptr();
        *is_floating = false;
    }
    true
}

unsafe extern "C" fn create(
    plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    if !unsafe { is_api_supported(plugin, api, is_floating) } {
        return false;
    }
    let Some(instance) = (unsafe { Instance::from_host(plugin) }) else {
        return false;
    };
    if instance.gui.created {
        return false;
    }
    instance.gui = Gui {
        created: true,
        width: WIDTH,
        height: HEIGHT,
        scale: 1.0,
        ..Default::default()
    };
    true
}

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    if let Some(instance) = unsafe { Instance::from_host(plugin) } {
        // Drop the child window during GUI destruction to avoid orphaned windows in the host.
        instance.gui = Gui::default();
    }
}

unsafe extern "C" fn set_scale(plugin: *const clap_plugin, scale: f64) -> bool {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) if instance.gui.created => {
            instance.gui.scale = scale;
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn get_size(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    let Some(instance) = (unsafe { Instance::from_host(plugin) }) else {
        return false;
    };
    if width.is_null() || height.is_null() {
        return false;
    }
    // Answered before `create` too: a host asks how big a window to make
    // before there is anything to put in it.
    let (w, h) = if instance.gui.created {
        (instance.gui.width, instance.gui.height)
    } else {
        (WIDTH, HEIGHT)
    };
    unsafe {
        *width = w;
        *height = h;
    }
    true
}

unsafe extern "C" fn can_resize(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn get_resize_hints(
    _plugin: *const clap_plugin,
    hints: *mut clap_gui_resize_hints,
) -> bool {
    if hints.is_null() {
        return false;
    }
    unsafe {
        *hints = clap_gui_resize_hints {
            can_resize_horizontally: true,
            can_resize_vertically: true,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        };
    }
    true
}

/// Round to a 10-pixel grid, so a test can see that the host honoured the
/// plugin's adjustment rather than its own request.
unsafe extern "C" fn adjust_size(
    _plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    if width.is_null() || height.is_null() {
        return false;
    }
    unsafe {
        *width = (*width).max(10) / 10 * 10;
        *height = (*height).max(10) / 10 * 10;
    }
    true
}

unsafe extern "C" fn set_size(plugin: *const clap_plugin, width: u32, height: u32) -> bool {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) if instance.gui.created => {
            instance.gui.width = width;
            instance.gui.height = height;
            instance.gui.child.resize(width, height);
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn set_parent(plugin: *const clap_plugin, window: *const clap_window) -> bool {
    let Some(instance) = (unsafe { Instance::from_host(plugin) }) else {
        return false;
    };
    if !instance.gui.created || window.is_null() {
        return false;
    }
    let w = unsafe { *window };
    if w.api.is_null() || (unsafe { CStr::from_ptr(w.api) }) != imp::WINDOW_API {
        return false;
    }
    let parent = unsafe { w.specific.ptr };
    instance
        .gui
        .child
        .create(parent, instance.gui.width, instance.gui.height)
}

unsafe extern "C" fn set_transient(
    _plugin: *const clap_plugin,
    _window: *const clap_window,
) -> bool {
    false
}

unsafe extern "C" fn suggest_title(_plugin: *const clap_plugin, _title: *const c_char) {}

unsafe extern "C" fn show(plugin: *const clap_plugin) -> bool {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) if instance.gui.created => {
            instance.gui.visible = true;
            instance.gui.child.show(true);
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn hide(plugin: *const clap_plugin) -> bool {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) if instance.gui.created => {
            instance.gui.visible = false;
            instance.gui.child.show(false);
            true
        }
        _ => false,
    }
}

#[cfg(windows)]
pub mod imp {
    use std::ffi::{CStr, c_void};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, EndPaint, FillRect, PAINTSTRUCT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SW_HIDE, SW_SHOW,
        SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, ShowWindow, WM_PAINT, WNDCLASSEXW, WS_CHILD,
        WS_VISIBLE,
    };

    pub const WINDOW_API: &CStr = clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;

    const CLASS_NAME: &[u16] = &[
        b'C' as u16,
        b'l' as u16,
        b'a' as u16,
        b'p' as u16,
        b'T' as u16,
        b'e' as u16,
        b's' as u16,
        b't' as u16,
        b'G' as u16,
        b'u' as u16,
        b'i' as u16,
        0,
    ];

    fn ensure_class() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(std::ptr::null()),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: CLASS_NAME.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&class);
        });
    }

    /// The plugin's own window, living inside the host's.
    #[derive(Default)]
    pub struct Child(HWND);

    impl Child {
        pub fn create(&mut self, parent: *mut c_void, width: u32, height: u32) -> bool {
            if parent.is_null() || !self.0.is_null() {
                return false;
            }
            ensure_class();
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    CLASS_NAME.as_ptr(),
                    std::ptr::null(),
                    WS_CHILD | WS_VISIBLE,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    parent as HWND,
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            self.0 = hwnd;
            !hwnd.is_null()
        }

        pub fn resize(&mut self, width: u32, height: u32) {
            if self.0.is_null() {
                return;
            }
            unsafe {
                SetWindowPos(
                    self.0,
                    std::ptr::null_mut(),
                    0,
                    0,
                    width as i32,
                    height as i32,
                    SWP_NOMOVE | SWP_NOZORDER,
                );
            }
        }

        pub fn show(&mut self, visible: bool) {
            if !self.0.is_null() {
                unsafe { ShowWindow(self.0, if visible { SW_SHOW } else { SW_HIDE }) };
            }
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { DestroyWindow(self.0) };
                self.0 = std::ptr::null_mut();
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
            if msg == WM_PAINT {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                // A colour nothing else would paint, so a screenshot of a
                // failing teardown is unambiguous about who drew what.
                let brush = CreateSolidBrush(0x00A0_5000);
                FillRect(hdc, &ps.rcPaint, brush);
                EndPaint(hwnd, &ps);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub mod imp {
    use std::ffi::{CStr, c_void};

    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, Window as XWindow, WindowClass,
    };
    use x11rb::rust_connection::RustConnection;

    pub const WINDOW_API: &CStr = clap_sys::ext::gui::CLAP_WINDOW_API_X11;

    /// A colour nothing else would paint, so a screenshot of a failing teardown
    /// is unambiguous about who drew what. The same one the Win32 side uses.
    const FILL: u32 = 0x00_50_a0;

    /// The plugin's own window, living inside the host's.
    ///
    /// On a connection of the plugin's own, which is what makes this a fair
    /// test: a real plugin has no access to the host's, and an id is the only
    /// thing that crosses between them.
    #[derive(Default)]
    pub struct Child(Option<Window>);

    struct Window {
        conn: RustConnection,
        id: XWindow,
    }

    impl Child {
        pub fn create(&mut self, parent: *mut c_void, width: u32, height: u32) -> bool {
            let parent = parent as usize as XWindow;
            if parent == x11rb::NONE || self.0.is_some() {
                return false;
            }
            let Ok((conn, _)) = RustConnection::connect(None) else {
                return false;
            };
            let Ok(id) = conn.generate_id() else {
                return false;
            };
            let attributes = CreateWindowAux::new().background_pixel(FILL);
            if conn
                .create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    id,
                    parent,
                    0,
                    0,
                    width.max(1) as u16,
                    height.max(1) as u16,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &attributes,
                )
                .is_err()
            {
                return false;
            }
            // Mapped straight away: CLAP creates the view before the host shows
            // it, and `show` toggles this window rather than creating it.
            let _ = conn.map_window(id);
            let _ = conn.flush();
            self.0 = Some(Window { conn, id });
            true
        }

        pub fn resize(&mut self, width: u32, height: u32) {
            let Some(window) = self.0.as_ref() else {
                return;
            };
            let aux = ConfigureWindowAux::new()
                .width(width.max(1))
                .height(height.max(1));
            let _ = window.conn.configure_window(window.id, &aux);
            let _ = window.conn.flush();
        }

        pub fn show(&mut self, visible: bool) {
            let Some(window) = self.0.as_ref() else {
                return;
            };
            let _ = if visible {
                window.conn.map_window(window.id)
            } else {
                window.conn.unmap_window(window.id)
            };
            let _ = window.conn.flush();
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let Some(window) = self.0.take() else { return };
            let _ = window.conn.destroy_window(window.id);
            let _ = window.conn.flush();
        }
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
pub mod imp {
    use std::ffi::{CStr, c_void};

    /// A window API no host will ask for, so `is_api_supported` says no and the
    /// rest of this module is never reached.
    pub const WINDOW_API: &CStr = c"unsupported";

    #[derive(Default)]
    pub struct Child;

    impl Child {
        pub fn create(&mut self, _parent: *mut c_void, _width: u32, _height: u32) -> bool {
            false
        }
        pub fn resize(&mut self, _width: u32, _height: u32) {}
        pub fn show(&mut self, _visible: bool) {}
    }
}

/// Suppress the unused-import warning where `c_void` is only used by cfg'd code.
#[allow(dead_code)]
type _Unused = *mut c_void;
