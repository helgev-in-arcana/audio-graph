//! Opening a sub-plugin's editor, and closing it in the right order.
//!
//! ARCHITECTURE.md §5.3 identifies teardown as the crash source, so the whole
//! sequence lives here rather than being spread across callers:
//!
//! ```text
//! IPlugView::removed()   -> tell the plugin its parent is going away
//! release the view       -> drop our reference
//! destroy the container  -> only now is the HWND allowed to die
//! ```
//!
//! The tempting alternative — let the OS destroy the parent and take the
//! child with it — tells the plugin nothing. It keeps posting timers and
//! calling `resizeView` against a dead window, and crashes some time later
//! somewhere unrelated.

use std::rc::Rc;

use vst3::ComPtr;
use vst3::Steinberg::{IPlugFrame, IPlugView, IPlugViewTrait, ViewRect, kResultOk, kResultTrue};

use crate::PLATFORM_TYPE;
use crate::frame::PlugFrame;
use host_window::{ContainerWindow, Size};

/// A sub-plugin editor in its own top-level window.
///
/// Dropping this runs the full teardown sequence, so there is no way to get it
/// wrong by forgetting a step — including on the path that matters most, where
/// the DAW destroys the wrapper without ever telling us to close the editor.
pub struct EditorWindow {
    /// Declared first so it is dropped first. `Drop` runs the sequence
    /// explicitly anyway; the ordering here is the belt to that's braces.
    view: ComPtr<IPlugView>,
    window: ContainerWindow,
    frame: Rc<PlugFrame>,
    /// Guards against running the teardown twice, since `close` is public and
    /// `Drop` calls it too.
    closed: bool,
}

impl EditorWindow {
    /// Attach `view` to a new container window.
    ///
    /// `view` must not already be attached to anything.
    ///
    /// `owner` is the window the editor should float above — the DAW's root
    /// window inside a plugin, null when standalone. See
    /// [`ContainerWindow::new`] for why it must be the root and not the
    /// wrapper's own editor view.
    pub fn open(
        view: ComPtr<IPlugView>,
        title: &str,
        owner: *mut std::ffi::c_void,
    ) -> Result<EditorWindow, String> {
        // Ask before building anything: a plugin that cannot use our platform
        // type has no editor we can show, and finding that out after creating a
        // window means unwinding it again.
        let platform = std::ffi::CString::new(PLATFORM_TYPE).unwrap();
        if unsafe { view.isPlatformTypeSupported(platform.as_ptr()) } != kResultTrue {
            return Err(format!(
                "the plugin's editor does not support {PLATFORM_TYPE} windows"
            ));
        }

        let size = view_size(&view).unwrap_or(Size::new(800, 600));
        let window = ContainerWindow::new(title, size, owner)?;

        // Tell the plugin the display scale before it ever paints. Plugins
        // initialise this to zero and expect the host to supply it; one that
        // divides by the factor while laying out — Chroma does — faults on its
        // first paint if the host never says anything. This has to happen
        // before `attached`, because that is when the first paint can occur.
        if let Some(scale_support) = view.cast::<vst3::Steinberg::IPlugViewContentScaleSupport>() {
            use vst3::Steinberg::IPlugViewContentScaleSupportTrait;
            unsafe { scale_support.setContentScaleFactor(window.scale_factor() as f32) };
        }

        // The frame goes in before attaching. A plugin may call resizeView
        // during `attached`, and a null frame at that moment means either a
        // dropped resize or a crash, depending on the plugin.
        let frame = PlugFrame::new();
        let frame_ptr = frame.com_ptr();
        unsafe { view.setFrame(frame_ptr) };

        // Shown before attaching: a plugin that paints during `attached` needs
        // a parent that already has a valid device context and size.
        window.show();

        let res = unsafe { view.attached(window.platform_handle(), platform.as_ptr()) };
        if res != kResultOk {
            // Undo in reverse. Leaving a frame set on a view we are about to
            // drop leaves the plugin holding a pointer to freed memory.
            unsafe { view.setFrame(std::ptr::null_mut()) };
            return Err(format!("IPlugView::attached failed ({res:#010x})"));
        }

        // The plugin may have chosen a different size while attaching.
        let attached_size = view_size(&view).unwrap_or(size);
        if attached_size != size {
            window.set_client_size(attached_size);
        }
        // Record what the plugin is already at. Without this the first tick
        // sees a difference against a zero default and calls `onSize` for a
        // resize that never happened — which Chroma answers by dividing by
        // zero, and which is wrong regardless of how a given plugin reacts.
        frame.set_last_reported_size(window.client_size());

        Ok(EditorWindow {
            view,
            window,
            frame,
            closed: false,
        })
    }

    pub fn window(&self) -> &ContainerWindow {
        &self.window
    }

    /// Whether the user clicked the window's close button.
    ///
    /// The window records the request rather than acting on it, so the owner
    /// decides when the teardown sequence runs (§5.3).
    pub fn close_requested(&self) -> bool {
        self.window.close_requested()
    }

    /// Carry out any resize the plugin asked for, and any the user made.
    ///
    /// Call once per UI tick. The two directions are handled here together
    /// because they are the same conversation: `resizeView` from the plugin
    /// must be answered with `onSize`, and a user-driven resize must be told to
    /// the plugin the same way.
    pub fn sync_size(&mut self) {
        if let Some(requested) = self.frame.take_requested_size() {
            self.window.set_client_size(requested);
            let mut rect = to_rect(requested);
            // The round trip §5.2 describes: the plugin asks, the host resizes,
            // the host tells the plugin what it actually got.
            unsafe { self.view.onSize(&mut rect) };
            return;
        }

        let current = self.window.client_size();
        if current != self.frame.last_reported_size() && current.width > 0 && current.height > 0 {
            let mut rect = to_rect(current);
            if unsafe { self.view.onSize(&mut rect) } == kResultOk {
                self.frame.set_last_reported_size(current);
            }
        }
    }

    /// Tear the editor down in the order the format requires.
    ///
    /// Safe to call more than once.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;

        unsafe {
            // 1. The plugin removes its child window from ours while both are
            //    still alive.
            self.view.removed();
            // 2. Drop the frame it was given, so it cannot call back into an
            //    object we are about to release.
            self.view.setFrame(std::ptr::null_mut());
        }
        // 3. and 4. happen as the fields drop: the view's reference, then the
        //    container window itself.
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        // The case §5.3 calls the more dangerous one: the DAW destroys the
        // plugin instance while the editor is still open, sometimes without any
        // close notification first. Running the sequence from `Drop` means that
        // path is covered by construction rather than by remembering.
        self.close();
        // `frame` is dropped after the view has been told to forget it.
        let _ = &self.frame;
    }
}

fn to_rect(size: Size) -> ViewRect {
    ViewRect {
        left: 0,
        top: 0,
        right: size.width,
        bottom: size.height,
    }
}

fn view_size(view: &ComPtr<IPlugView>) -> Option<Size> {
    let mut rect = ViewRect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { view.getSize(&mut rect) } != kResultOk {
        return None;
    }
    let size = Size::new(rect.right - rect.left, rect.bottom - rect.top);
    // Some plugins report nothing useful before being attached; a zero-sized
    // window is worse than a default one.
    (size.width > 0 && size.height > 0).then_some(size)
}

/// Whether a view can be resized by the user.
pub fn can_resize(view: &ComPtr<IPlugView>) -> bool {
    unsafe { view.canResize() == kResultTrue }
}

/// Suppress the unused-import warning on platforms without a window backend.
#[allow(dead_code)]
type _Unused = IPlugFrame;
