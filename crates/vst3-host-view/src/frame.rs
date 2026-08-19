//! The host's `IPlugFrame`: how a plugin asks to be resized.
//!
//! `resizeView` is called *by the plugin*, and the host is then expected to
//! resize the container and call `IPlugView::onSize` back with what it actually
//! got. That round trip is the whole reason a frame object exists.
//!
//! The request is recorded rather than acted on immediately. A plugin may call
//! `resizeView` from inside `attached`, or from a paint handler, and resizing a
//! window synchronously from there re-enters the plugin while it is mid-call.

use std::cell::Cell;

use vst3::Steinberg::{IPlugFrame, IPlugFrameTrait, IPlugView, ViewRect, kResultOk, tresult};
use vst3::{Class, ComWrapper};

use crate::window::Size;

/// Host-side `IPlugFrame`.
pub struct PlugFrame {
    /// A size the plugin asked for and the host has not applied yet.
    requested: Cell<Option<Size>>,
    /// The last size we told the plugin about, so a user-driven resize is only
    /// reported when it actually changed.
    last_reported: Cell<Size>,
    wrapper: std::cell::OnceCell<ComWrapper<FrameImpl>>,
}

/// The COM object itself, separate so `PlugFrame` can be held by value.
pub struct FrameImpl {
    requested: *const Cell<Option<Size>>,
}

// SAFETY: the pointer refers to a `PlugFrame` that outlives this object —
// `EditorWindow` owns both and drops the frame last. All access happens on the
// UI thread, which is where VST3 confines `IPlugFrame` calls.
unsafe impl Send for FrameImpl {}
unsafe impl Sync for FrameImpl {}

impl Class for FrameImpl {
    type Interfaces = (IPlugFrame,);
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
            wrapper: std::cell::OnceCell::new(),
        });
        let requested: *const Cell<Option<Size>> = &frame.requested;
        let _ = frame.wrapper.set(ComWrapper::new(FrameImpl { requested }));
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
}
