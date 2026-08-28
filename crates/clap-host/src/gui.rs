//! Container and lifecycle management for CLAP plugin GUI editors.
//!
//! Handles creating, attaching, resizing, and cleanly destroying embedded plugin
//! editor views within a top-level container window. Enforces the strict
//! lifecycle order required by CLAP:
//!
//! 1. `gui.create(api, floating = false)`
//! 2. `gui.set_scale`
//! 3. `gui.set_parent(window)`
//! 4. `gui.set_size` / `gui.get_size`
//! 5. `gui.show`
//!
//! Teardown order:
//! 1. `gui.hide`
//! 2. `gui.destroy`
//! 3. Destroy container window

use clap_sys::ext::gui::{clap_plugin_gui, clap_window, clap_window_handle};
use clap_sys::plugin::clap_plugin;
use host_window::{ContainerWindow, Size};

#[cfg(target_os = "macos")]
pub use clap_sys::ext::gui::CLAP_WINDOW_API_COCOA as WINDOW_API;
/// The name CLAP gives this platform's window handles.
///
/// Not the same strings VST3 uses for the same handles, which is the whole
/// reason `host-window` does not own a constant like this.
#[cfg(windows)]
pub use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32 as WINDOW_API;
#[cfg(all(unix, not(target_os = "macos")))]
pub use clap_sys::ext::gui::CLAP_WINDOW_API_X11 as WINDOW_API;

/// Size used when a plugin will not say how big it wants to be.
///
/// A zero-sized window is worse than a wrong-sized one: the plugin gets no
/// paint at all and the user sees a title bar.
const FALLBACK_SIZE: Size = Size {
    width: 800,
    height: 600,
};

/// An open CLAP editor.
///
/// Borrowed pointers, not owned ones: the instance owns its GUI extension, and
/// this type owns only the window and the obligation to unwind in order.
pub(crate) struct ClapEditor {
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
    window: ContainerWindow,
    /// Last size the plugin was told about, so a user-driven resize is only
    /// forwarded when it is genuinely new.
    last_reported: Size,
    /// Guards against running the teardown twice, since `close` is called both
    /// explicitly and from `Drop`.
    closed: bool,
}

impl ClapEditor {
    /// Create the plugin's editor and attach it to a new container window.
    ///
    /// # Safety
    /// `plugin` and `gui` must be live pointers belonging to one instance, and
    /// the caller must guarantee the editor is dropped before the instance is.
    pub(crate) unsafe fn open(
        plugin: *const clap_plugin,
        gui: *const clap_plugin_gui,
        title: &str,
        owner: *mut std::ffi::c_void,
    ) -> Result<ClapEditor, String> {
        let ext = unsafe { &*gui };

        // Asked before anything is built: a plugin that cannot use our window
        // type has no editor we can show, and finding out after creating a
        // window means unwinding it again.
        let supported = ext.is_api_supported.is_some_and(|f| unsafe {
            // Check support for embedded window mode.
            f(plugin, WINDOW_API.as_ptr(), false)
        });
        if !supported {
            return Err(format!(
                "the plugin's editor does not support embedded {} windows",
                WINDOW_API.to_string_lossy()
            ));
        }

        let create = ext
            .create
            .ok_or("the plugin's gui extension has no create")?;
        if !unsafe { create(plugin, WINDOW_API.as_ptr(), false) } {
            return Err("clap_plugin_gui::create failed".into());
        }

        // From here on every early return has to destroy the GUI again, so the
        // rest is written as one fallible block and unwound in one place.
        match unsafe { Self::attach(plugin, ext, title, owner) } {
            Ok((window, size)) => Ok(ClapEditor {
                plugin,
                gui,
                window,
                last_reported: size,
                closed: false,
            }),
            Err(e) => {
                if let Some(destroy) = ext.destroy {
                    unsafe { destroy(plugin) };
                }
                Err(e)
            }
        }
    }

    /// # Safety
    /// As [`ClapEditor::open`], and `create` must already have succeeded.
    unsafe fn attach(
        plugin: *const clap_plugin,
        ext: &clap_plugin_gui,
        title: &str,
        owner: *mut std::ffi::c_void,
    ) -> Result<(ContainerWindow, Size), String> {
        let size = unsafe { plugin_size(plugin, ext) }.unwrap_or(FALLBACK_SIZE);
        let window = ContainerWindow::new(title, size, owner)?;

        // Before the parent, so the plugin lays out at the right scale on its
        // very first paint rather than after a correction the user can see.
        if let Some(set_scale) = ext.set_scale {
            unsafe { set_scale(plugin, window.scale_factor()) };
        }

        let parent = clap_window {
            api: WINDOW_API.as_ptr(),
            specific: clap_window_handle {
                ptr: window.platform_handle(),
            },
        };
        let set_parent = ext
            .set_parent
            .ok_or("the plugin's gui extension has no set_parent")?;
        if !unsafe { set_parent(plugin, &parent) } {
            return Err("clap_plugin_gui::set_parent failed".into());
        }

        // A resizable plugin is told the size it is getting; a fixed one is
        // asked again, because it may have chosen differently once it had a
        // parent.
        let resizable = ext.can_resize.is_some_and(|f| unsafe { f(plugin) });
        let size = if resizable {
            if let Some(set_size) = ext.set_size {
                unsafe { set_size(plugin, size.width.max(0) as u32, size.height.max(0) as u32) };
            }
            size
        } else {
            let actual = unsafe { plugin_size(plugin, ext) }.unwrap_or(size);
            if actual != size {
                window.set_client_size(actual);
            }
            actual
        };

        window.show();
        if let Some(show) = ext.show {
            unsafe { show(plugin) };
        }

        Ok((window, size))
    }

    pub(crate) fn window(&self) -> &ContainerWindow {
        &self.window
    }

    pub(crate) fn close_requested(&self) -> bool {
        self.window.close_requested()
    }

    /// Resize the window because the plugin asked, and tell it what it got.
    pub(crate) fn apply_requested_resize(&mut self, width: u32, height: u32) {
        let size = Size::new(width as i32, height as i32);
        if size.width <= 0 || size.height <= 0 {
            return;
        }
        self.window.set_client_size(size);
        if let Some(set_size) = unsafe { (*self.gui).set_size } {
            unsafe { set_size(self.plugin, width, height) };
        }
        self.last_reported = size;
    }

    /// Forward a resize the *user* made by dragging the window edge.
    ///
    /// Call once per UI tick. The plugin-driven direction is handled by
    /// [`ClapEditor::apply_requested_resize`], because it arrives as a host
    /// callback rather than as a window message.
    pub(crate) fn sync_size(&mut self) {
        let current = self.window.client_size();
        if current == self.last_reported || current.width <= 0 || current.height <= 0 {
            return;
        }
        let ext = unsafe { &*self.gui };
        if !ext.can_resize.is_some_and(|f| unsafe { f(self.plugin) }) {
            // A fixed-size editor is told nothing; the window frame is the
            // user's to waste space in.
            self.last_reported = current;
            return;
        }

        let (mut width, mut height) = (current.width.max(0) as u32, current.height.max(0) as u32);
        // The plugin gets to round the request to something it can actually
        // draw — an aspect ratio, a grid — before it is committed.
        if let Some(adjust) = ext.adjust_size {
            unsafe { adjust(self.plugin, &mut width, &mut height) };
        }
        if let Some(set_size) = ext.set_size {
            unsafe { set_size(self.plugin, width, height) };
        }
        let adjusted = Size::new(width as i32, height as i32);
        if adjusted != current {
            self.window.set_client_size(adjusted);
        }
        self.last_reported = adjusted;
    }

    /// Tear the editor down in the order the format requires.
    ///
    /// Safe to call more than once.
    pub(crate) fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let ext = unsafe { &*self.gui };
        unsafe {
            // 1. Stop it drawing while its parent is still alive.
            if let Some(hide) = ext.hide {
                hide(self.plugin);
            }
            // 2. Let it release everything it made inside our window.
            if let Some(destroy) = ext.destroy {
                destroy(self.plugin);
            }
        }
        // 3. The window drops last, with the field.
    }
}

impl Drop for ClapEditor {
    fn drop(&mut self) {
        // Ensure proper teardown sequence is always executed on drop.
        self.close();
    }
}

/// Ask the plugin how big its editor is.
///
/// # Safety
/// `plugin` and `ext` must belong to one live instance whose GUI exists.
unsafe fn plugin_size(plugin: *const clap_plugin, ext: &clap_plugin_gui) -> Option<Size> {
    let get_size = ext.get_size?;
    let (mut width, mut height) = (0u32, 0u32);
    if !unsafe { get_size(plugin, &mut width, &mut height) } {
        return None;
    }
    (width > 0 && height > 0).then(|| Size::new(width as i32, height as i32))
}

/// Whether a plugin's editor can be resized by the user.
///
/// # Safety
/// `plugin` and `gui` must belong to one live instance.
pub(crate) unsafe fn can_resize(plugin: *const clap_plugin, gui: *const clap_plugin_gui) -> bool {
    unsafe { (*gui).can_resize }.is_some_and(|f| unsafe { f(plugin) })
}
