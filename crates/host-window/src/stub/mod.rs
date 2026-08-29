//! Backend for platforms that have none yet.
//!
//! A real stub rather than a silently broken window: returning an error is the
//! honest answer until `NSWindow` and `NSView` embedding are written, which is
//! main-thread-only work with an event loop shaped unlike either of the other
//! two.

use std::ffi::c_void;
use std::rc::Rc;

use crate::keys::Key;
use crate::window::{Size, WindowState};

/// Whether the host, rather than this crate, drives the event source our
/// windows live on. See [`crate::poll`].
///
/// True by default, which is the answer that makes a caller do nothing.
pub(crate) const HOST_DRIVES_EVENTS: bool = true;

pub(crate) struct Window;

impl Window {
    pub(crate) fn new(
        _title: &str,
        _size: Size,
        _owner: *mut c_void,
        _state: Rc<WindowState>,
    ) -> Result<Window, String> {
        Err("sub-plugin editor windows are not implemented on this platform yet".into())
    }

    pub(crate) fn handle(&self) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(crate) fn set_client_size(&self, _size: Size) {}

    pub(crate) fn show(&self) {}

    pub(crate) fn scale_factor(&self) -> f64 {
        1.0
    }
}

pub(crate) fn pump_events() {}

pub(crate) fn forward_key(_window: usize, _key: Key, _pressed: bool) {}

pub(crate) fn root_window(handle: *mut c_void) -> *mut c_void {
    handle
}
