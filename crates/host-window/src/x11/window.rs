//! X11 container window.
//!
//! A plugin editor on Linux is an X window the plugin creates on its own
//! connection and reparents into the id we hand it — the same id both formats
//! ask for, under the names `X11EmbedWindowID` and `x11`. So this window draws
//! nothing and has no children of its own making; it is a titled rectangle with
//! a stable id.
//!
//! Unlike Win32 there is no frame to add: an X window's width and height *are*
//! its client area, and the window manager decorates around the outside.

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;

use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::connection::Connection as _;
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Window as XWindow, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::conn::{Conn, conn, set_title};
use crate::window::{Size, WindowState};

pub(crate) struct Window {
    conn: Rc<Conn>,
    id: Cell<XWindow>,
    /// Kept alive for as long as the window can still receive events: the
    /// connection's dispatch table holds a weak reference into it.
    _state: Rc<WindowState>,
}

impl Window {
    pub(crate) fn new(
        title: &str,
        size: Size,
        owner: *mut c_void,
        state: Rc<WindowState>,
    ) -> Result<Window, String> {
        let conn = conn()?;
        let screen = conn.screen();
        let root = screen.root;

        let id = conn
            .conn
            .generate_id()
            .map_err(|e| format!("out of X resource ids: {e}"))?;

        // Roughly centred. A window that opens under the DAW's own is worse
        // than one that opens in a slightly odd place.
        let (x, y) = centre(
            i32::from(screen.width_in_pixels),
            i32::from(screen.height_in_pixels),
            size,
        );

        let attributes = CreateWindowAux::new()
            // Something to look at until the plugin's own window covers it. An
            // unset background is whatever was on screen before.
            .background_pixel(screen.black_pixel)
            // ConfigureNotify is how the size the window manager actually gave
            // us gets back to `WindowState`.
            .event_mask(EventMask::STRUCTURE_NOTIFY);

        conn.conn
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                id,
                root,
                x as i16,
                y as i16,
                size.width.max(1) as u16,
                size.height.max(1) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &attributes,
            )
            .map_err(|e| format!("could not create the container window: {e}"))?;

        set_title(&conn, id, title);
        declare(&conn, id, size, owner as XWindow);

        // A round trip, not just a flush: the plugin is about to be handed this
        // id and will reparent into it from a connection of its own, which can
        // only work once the server has actually created the window.
        conn.conn
            .get_input_focus()
            .map_err(|e| format!("could not create the container window: {e}"))?
            .reply()
            .map_err(|e| format!("could not create the container window: {e}"))?;

        conn.register_window(id, &state);
        Ok(Window {
            conn,
            id: Cell::new(id),
            _state: state,
        })
    }

    pub(crate) fn handle(&self) -> *mut c_void {
        self.id.get() as usize as *mut c_void
    }

    pub(crate) fn set_client_size(&self, size: Size) {
        if size.width <= 0 || size.height <= 0 {
            return;
        }
        let aux = ConfigureWindowAux::new()
            .width(size.width as u32)
            .height(size.height as u32);
        let _ = self.conn.conn.configure_window(self.id.get(), &aux);
        // The window manager may still say otherwise, and ConfigureNotify will
        // report what it settled on. Meanwhile the hints have to agree with the
        // request or a resize can be bounced straight back.
        size_hints(size)
            .set(&self.conn.conn, self.id.get(), AtomEnum::WM_NORMAL_HINTS)
            .ok();
        let _ = self.conn.conn.flush();
    }

    pub(crate) fn show(&self) {
        let _ = self.conn.conn.map_window(self.id.get());
        let _ = self.conn.conn.flush();
    }

    pub(crate) fn scale_factor(&self) -> f64 {
        super::scale_factor(&self.conn)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let id = self.id.replace(x11rb::NONE);
        if id == x11rb::NONE {
            return;
        }
        // Off the dispatch table first: destroying the window produces events
        // that name it, and the state they would be delivered to is about to go
        // away.
        self.conn.unregister_window(id);
        let _ = self.conn.conn.destroy_window(id);
        let _ = self.conn.conn.flush();
    }
}

/// Tell the window manager what kind of window this is and how to treat it.
fn declare(conn: &Conn, id: XWindow, size: Size, owner: XWindow) {
    // Without this the close button kills the connection outright rather than
    // asking, and the plugin's child window goes with it unannounced.
    let _ = conn.conn.change_property32(
        PropMode::REPLACE,
        id,
        conn.atoms.wm_protocols,
        AtomEnum::ATOM,
        &[conn.atoms.wm_delete_window],
    );

    // The X analogue of a Win32 owner: the window stays above the DAW's and
    // minimises with it, instead of being buried the moment the user clicks
    // anywhere else.
    if owner != x11rb::NONE {
        let _ = conn.conn.change_property32(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[owner],
        );
    }

    let _ = conn.conn.change_property32(
        PropMode::REPLACE,
        id,
        conn.atoms.net_wm_window_type,
        AtomEnum::ATOM,
        &[conn.atoms.net_wm_window_type_utility],
    );

    // So a window manager can tell which process to blame, and so that
    // `_NET_WM_PID` consumers group the window with the DAW that loaded us.
    let _ = conn.conn.change_property32(
        PropMode::REPLACE,
        id,
        conn.atoms.net_wm_pid,
        AtomEnum::CARDINAL,
        &[std::process::id()],
    );

    // instance and class, NUL separated, as WM_CLASS has wanted since X10.
    let _ = conn.conn.change_property8(
        PropMode::REPLACE,
        id,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"audio-graph\0AudioGraph\0",
    );

    size_hints(size)
        .set(&conn.conn, id, AtomEnum::WM_NORMAL_HINTS)
        .ok();
}

/// The size the window is asking for, as WM_NORMAL_HINTS.
///
/// Only the size is stated. Leaving the minimum and the increments unset is
/// what makes the window freely resizable, which is the behaviour a plugin that
/// can resize wants and the one a plugin that cannot is no worse off for.
fn size_hints(size: Size) -> WmSizeHints {
    let mut hints = WmSizeHints::new();
    hints.size = Some((
        WmSizeHintsSpecification::ProgramSpecified,
        size.width.max(1),
        size.height.max(1),
    ));
    hints
}

/// Where to put a window of `size` so that it sits in the middle of the screen.
fn centre(screen_width: i32, screen_height: i32, size: Size) -> (i32, i32) {
    if screen_width <= 0 || screen_height <= 0 {
        return (0, 0);
    }
    (
        ((screen_width - size.width) / 2).max(0),
        ((screen_height - size.height) / 2).max(0),
    )
}

/// The top-level window `handle` belongs to.
///
/// Walks up until the next step would be the root, which is the DAW's own frame
/// rather than the desktop — the same window `GetAncestor(GA_ROOT)` answers
/// with. A window manager that reparents for decorations puts its frame in this
/// chain, and the frame is the right answer: it is what the user drags and what
/// a transient window should be transient for.
pub(crate) fn root_window(handle: *mut c_void) -> *mut c_void {
    let id = handle as usize as XWindow;
    if id == x11rb::NONE {
        return handle;
    }
    let Ok(conn) = conn() else { return handle };
    let root = conn.root();

    let mut current = id;
    // Bounded: a window tree is shallow, and a cycle would otherwise hang the
    // UI thread on a malformed server reply.
    for _ in 0..32 {
        let Ok(tree) = conn.conn.query_tree(current) else {
            break;
        };
        let Ok(tree) = tree.reply() else { break };
        if tree.parent == x11rb::NONE || tree.parent == root {
            return current as usize as *mut c_void;
        }
        current = tree.parent;
    }
    current as usize as *mut c_void
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_centred_on_the_screen() {
        assert_eq!(centre(1920, 1080, Size::new(800, 600)), (560, 240));
    }

    #[test]
    fn a_window_larger_than_the_screen_starts_at_the_corner() {
        assert_eq!(centre(800, 600, Size::new(1920, 1080)), (0, 0));
    }

    #[test]
    fn an_unknown_screen_size_is_not_divided_by() {
        assert_eq!(centre(0, 0, Size::new(800, 600)), (0, 0));
    }
}
