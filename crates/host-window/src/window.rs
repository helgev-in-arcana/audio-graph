//! Container window implementation for hosting plugin editors.
//!
//! This window draws nothing. It is a titled, resizable frame to embed a
//! plugin's own view into, and nothing else, so wgpu, Vello, layout and a
//! windowing crate are all beside the point.
//!
//! `winit` in particular does not fit: it is built around owning the event
//! loop, and inside a plugin the DAW owns it. On Windows that is not even a
//! difficulty — a window created on the DAW's UI thread has its messages
//! delivered by the DAW's own pump, so there is no loop to run at all. On X11
//! there is one, because an X connection is per-client and the DAW's is not
//! ours; [`crate::poll`] is how it gets turned.

use std::cell::Cell;
use std::rc::Rc;

use crate::imp;

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
///
/// Deliberately owns no plugin object. The teardown ordering rules live with
/// whoever owns both this and the plugin's view — `vst3-host-view`'s
/// `EditorWindow` — so that the sequence is written once in one place.
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
    /// it are only dispatched by that thread's pump; on X11 the connection this
    /// window is made on is the one [`crate::poll`] drains, and that is also
    /// per thread.
    ///
    /// `owner` is the window this one should float above — the DAW's own
    /// top-level window when there is one, null when running standalone.
    ///
    /// Without an owner the window is a peer of the DAW's, so clicking anywhere
    /// in the DAW buries the plugin's editor behind it. With one it stays in
    /// front of the DAW and minimises with it, which is what every other plugin
    /// window does.
    ///
    /// It must be the DAW's *root* window, not the wrapper's editor view:
    /// Windows destroys owned windows along with their owner, and the editor
    /// view is destroyed every time the user closes the wrapper's UI — which
    /// would take the sub-plugin's window with it, without the plugin ever
    /// being told. The root window only dies when the DAW does.
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

    /// The handle to hand to `IPlugView::attached`, or to CLAP's `set_parent`.
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
    /// Only for standalone use — a plugin must *not* call this, because on
    /// Windows the DAW's pump is already dispatching. It exists so the
    /// development harness can drive a window without a DAW; a plugin wants
    /// [`crate::poll`].
    pub fn pump_events(&self) {
        imp::pump_events();
    }
}

/// Dispatch pending messages for the calling thread.
///
/// For standalone harnesses only. Inside a plugin the DAW may own the pump and
/// calling this would dispatch its messages on our behalf — [`crate::poll`] is
/// the version that is safe there.
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

#[cfg(all(test, any(windows, all(unix, not(target_os = "macos")))))]
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

    #[test]
    fn a_shown_window_reports_the_size_it_was_asked_for() {
        let window = ContainerWindow::new("test", Size::new(400, 300), std::ptr::null_mut())
            .expect("create");
        window.show();
        window.pump_events();
        assert_eq!(window.client_size(), Size::new(400, 300));
    }
}
