//! The X connection our windows live on, and the loop that drains it.
//!
//! One connection per thread, opened the first time something needs it. It is
//! not the DAW's connection, which is the fact the rest of this backend is
//! shaped by: nothing the host pumps will ever deliver our events, so [`pump`]
//! has to be called by us — see [`crate::poll`].
//!
//! Sharing one connection between every window on a thread keeps a single queue
//! to drain. Windows register themselves against their X id and are dispatched
//! to from [`pump`].

use std::cell::RefCell;
use std::io::ErrorKind;
use std::rc::{Rc, Weak};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::errors::ConnectError;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, Keycode, Keysym, PropMode, Screen, Window as XWindow,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::window::{Size, WindowState};

thread_local! {
    static CONN: RefCell<Option<Rc<Conn>>> = const { RefCell::new(None) };
}

/// Atoms this backend needs interned once.
pub(crate) struct Atoms {
    pub(crate) wm_protocols: Atom,
    pub(crate) wm_delete_window: Atom,
    pub(crate) net_wm_name: Atom,
    pub(crate) utf8_string: Atom,
    pub(crate) net_wm_window_type: Atom,
    pub(crate) net_wm_window_type_utility: Atom,
    pub(crate) net_wm_pid: Atom,
}

pub(crate) struct Conn {
    pub(crate) conn: RustConnection,
    pub(crate) screen: usize,
    pub(crate) atoms: Atoms,
    /// Container windows, by the X id their events name.
    windows: RefCell<Vec<(XWindow, Weak<WindowState>)>>,
    /// The user's layout, as a keysym for each keycode's unshifted level.
    ///
    /// Cached because forwarding a key needs it and a key arrives far more
    /// often than a layout changes. Emptied on `MappingNotify`, which is the
    /// server saying it just did change.
    keymap: RefCell<Option<Vec<Keysym>>>,
}

impl Conn {
    pub(crate) fn screen(&self) -> &Screen {
        &self.conn.setup().roots[self.screen]
    }

    pub(crate) fn root(&self) -> XWindow {
        self.screen().root
    }

    pub(crate) fn register_window(&self, id: XWindow, state: &Rc<WindowState>) {
        self.windows.borrow_mut().push((id, Rc::downgrade(state)));
    }

    pub(crate) fn unregister_window(&self, id: XWindow) {
        self.windows.borrow_mut().retain(|(other, _)| *other != id);
    }

    /// The keycode this layout puts `target` on, if it puts it anywhere.
    ///
    /// The unshifted level only. A key that produces this symbol solely with a
    /// modifier held would need that modifier sent as well to mean the same
    /// thing, and forwarding half of a chord is worse than forwarding nothing.
    pub(crate) fn keycode(&self, target: Keysym) -> Option<Keycode> {
        let first = self.conn.setup().min_keycode;
        if self.keymap.borrow().is_none() {
            *self.keymap.borrow_mut() = Some(self.read_keymap()?);
        }
        let keymap = self.keymap.borrow();
        let index = keymap.as_ref()?.iter().position(|sym| *sym == target)?;
        Some(first + index as Keycode)
    }

    /// The unshifted keysym of every keycode, in keycode order.
    fn read_keymap(&self) -> Option<Vec<Keysym>> {
        let setup = self.conn.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let mapping = self
            .conn
            .get_keyboard_mapping(first, last - first + 1)
            .ok()?
            .reply()
            .ok()?;
        let per_code = usize::from(mapping.keysyms_per_keycode);
        if per_code == 0 {
            return None;
        }
        Some(
            mapping
                .keysyms
                .chunks(per_code)
                .filter_map(|level| level.first().copied())
                .collect(),
        )
    }

    /// The state for `id`, as an owning handle.
    ///
    /// Taken out of the registry rather than borrowed across the dispatch:
    /// handling an event can create or destroy windows, and that mutates the
    /// very list being walked.
    fn window(&self, id: XWindow) -> Option<Rc<WindowState>> {
        let windows = self.windows.borrow();
        let (_, state) = windows.iter().find(|(other, _)| *other == id)?;
        state.upgrade()
    }
}

/// The connection for this thread, opening one if this is the first call.
pub(crate) fn conn() -> Result<Rc<Conn>, String> {
    CONN.with(|cell| {
        if let Some(conn) = cell.borrow().as_ref() {
            return Ok(Rc::clone(conn));
        }
        let conn = Rc::new(open()?);
        *cell.borrow_mut() = Some(Rc::clone(&conn));
        Ok(conn)
    })
}

/// The connection for this thread, or `None` if nothing has needed one yet.
///
/// For callers that have nothing to do without windows of their own, so that
/// they do not open a connection just to discover that.
fn existing() -> Option<Rc<Conn>> {
    CONN.with(|cell| cell.borrow().as_ref().map(Rc::clone))
}

/// Reach the server, allowing for a handshake that is dropped rather than
/// refused.
///
/// Connecting is a socket and then an exchange over it, and a loaded server
/// will occasionally close one part-way through. That says nothing about
/// whether there is a server to talk to, so it is worth a moment and another
/// go. Every other answer is returned as it came: no display, a refused socket
/// and a rejected cookie are all settled the first time they are given.
fn connect() -> Result<(RustConnection, usize), ConnectError> {
    let mut wait = Duration::from_millis(1);
    for _ in 0..ATTEMPTS - 1 {
        match RustConnection::connect(None) {
            Err(e) if dropped_mid_handshake(&e) => std::thread::sleep(wait),
            settled => return settled,
        }
        wait *= 2;
    }
    RustConnection::connect(None)
}

/// How many times to try before the answer is the answer.
///
/// Small on purpose: the wait doubles from a millisecond, and a window that
/// takes a noticeable moment to appear is worse than one that reports why it
/// did not.
const ATTEMPTS: usize = 4;

fn dropped_mid_handshake(e: &ConnectError) -> bool {
    let ConnectError::IoError(e) = e else {
        return false;
    };
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::Interrupted
            | ErrorKind::TimedOut
    )
}

fn open() -> Result<Conn, String> {
    let (conn, screen) = connect().map_err(|e| format!("could not reach the X server: {e}"))?;

    // Asked for all at once and collected afterwards: interning is a round trip
    // each, and seven in series is seven times the latency.
    let mut cookies = Vec::new();
    for name in [
        b"WM_PROTOCOLS".as_slice(),
        b"WM_DELETE_WINDOW",
        b"_NET_WM_NAME",
        b"UTF8_STRING",
        b"_NET_WM_WINDOW_TYPE",
        b"_NET_WM_WINDOW_TYPE_UTILITY",
        b"_NET_WM_PID",
    ] {
        cookies.push(
            conn.intern_atom(false, name)
                .map_err(|e| format!("could not intern an atom: {e}"))?,
        );
    }
    let mut atoms = Vec::new();
    for cookie in cookies {
        atoms.push(
            cookie
                .reply()
                .map_err(|e| format!("could not intern an atom: {e}"))?
                .atom,
        );
    }

    Ok(Conn {
        conn,
        screen,
        atoms: Atoms {
            wm_protocols: atoms[0],
            wm_delete_window: atoms[1],
            net_wm_name: atoms[2],
            utf8_string: atoms[3],
            net_wm_window_type: atoms[4],
            net_wm_window_type_utility: atoms[5],
            net_wm_pid: atoms[6],
        },
        windows: RefCell::new(Vec::new()),
        keymap: RefCell::new(None),
    })
}

/// Hand everything the server has sent us to whichever window it is for.
///
/// Never blocks: an empty queue is the ordinary case, and a caller runs this
/// once per frame. Nothing here runs a caller's code — an event only ever
/// writes to a [`WindowState`] — so there is no turn of this that is unsafe to
/// be inside of.
pub(crate) fn pump() {
    let Some(conn) = existing() else { return };

    loop {
        let event = match conn.conn.poll_for_event() {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(e) => {
                log::warn!("audio-graph: lost the X connection: {e}");
                break;
            }
        };
        dispatch(&conn, &event);
    }

    // Anything the events queued is only a request until it is written out.
    let _ = conn.conn.flush();
}

fn dispatch(conn: &Conn, event: &Event) {
    match event {
        // The window manager asking, rather than telling: the close button
        // arrives here because the window advertises WM_DELETE_WINDOW. Recorded
        // and not obeyed, exactly as on Windows — destroying the window here
        // would take the plugin's child with it without the plugin ever being
        // told.
        Event::ClientMessage(e) if e.type_ == conn.atoms.wm_protocols => {
            if e.format == 32
                && e.data.as_data32()[0] == conn.atoms.wm_delete_window
                && let Some(state) = conn.window(e.window)
            {
                state.close_requested.set(true);
            }
        }
        // The layout changed under us, so what was cached is now a guess.
        Event::MappingNotify(_) => {
            *conn.keymap.borrow_mut() = None;
        }
        // The size the window actually got, which is not always the size that
        // was asked for: the window manager has the last word.
        Event::ConfigureNotify(e) => {
            if let Some(state) = conn.window(e.window) {
                state
                    .size
                    .set(Size::new(i32::from(e.width), i32::from(e.height)));
            }
        }
        _ => {}
    }
}

/// Write a UTF-8 string property, in both the modern and the legacy spelling.
///
/// Window managers are not consistent about which of the two they read, and a
/// window with no title in the taskbar is a window the user cannot find.
pub(crate) fn set_title(conn: &Conn, window: XWindow, title: &str) {
    let _ = conn.conn.change_property8(
        PropMode::REPLACE,
        window,
        conn.atoms.net_wm_name,
        conn.atoms.utf8_string,
        title.as_bytes(),
    );
    let _ = conn.conn.change_property8(
        PropMode::REPLACE,
        window,
        Atom::from(AtomEnum::WM_NAME),
        Atom::from(AtomEnum::STRING),
        title.as_bytes(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dropped_handshake_is_worth_a_second_go() {
        let dropped = |kind| ConnectError::IoError(std::io::Error::from(kind));
        assert!(dropped_mid_handshake(&dropped(ErrorKind::ConnectionReset)));
        assert!(dropped_mid_handshake(&dropped(ErrorKind::Interrupted)));

        // There is no server, or it will not have us. Trying again only makes
        // the failure slower.
        assert!(!dropped_mid_handshake(&dropped(
            ErrorKind::ConnectionRefused
        )));
        assert!(!dropped_mid_handshake(&dropped(ErrorKind::NotFound)));
        assert!(!dropped_mid_handshake(&ConnectError::ZeroIdMask));
    }
}
