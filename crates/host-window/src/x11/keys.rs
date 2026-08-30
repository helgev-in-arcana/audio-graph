//! X11 side of key forwarding.
//!
//! X has no virtual key codes. A key is a *keysym*, which the keyboard mapping
//! turns into the *keycode* an event actually carries, and the mapping is the
//! user's layout — so the same keysym is a different keycode on two machines
//! and has to be looked up rather than assumed.
//!
//! Whether the DAW acts on what arrives is the DAW's decision: an event put on
//! the wire by a client rather than by the server is marked as such, and a
//! toolkit is entitled to ignore it. There is no way to send one that is not so
//! marked without XTEST, which injects at the server and would land on whatever
//! has focus — which is the editor we are trying to forward *away* from.

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    ConnectionExt as _, KeyPressEvent, KeyReleaseEvent, Keysym, Window as XWindow,
};

use super::conn::conn;
use crate::keys::Key;

pub(crate) fn forward_key(window: usize, key: Key, pressed: bool) {
    let target = window as XWindow;
    if target == x11rb::NONE {
        return;
    }
    let Ok(conn) = conn() else { return };
    let Some(keycode) = conn.keycode(keysym(key)) else {
        return;
    };

    // The modifier mask the user is physically holding, which is what makes a
    // forwarded key part of a shortcut rather than a bare one.
    let (root, state) = match conn.conn.query_pointer(conn.root()).map(|c| c.reply()) {
        Ok(Ok(reply)) => (reply.root, u16::from(reply.mask)),
        _ => (conn.root(), 0),
    };

    let event = KeyPressEvent {
        response_type: if pressed { 2 } else { 3 },
        detail: keycode,
        sequence: 0,
        time: x11rb::CURRENT_TIME,
        root,
        event: target,
        child: x11rb::NONE,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        state: state.into(),
        same_screen: true,
    };

    let _ = if pressed {
        conn.conn.send_event(
            false,
            target,
            x11rb::protocol::xproto::EventMask::KEY_PRESS,
            event,
        )
    } else {
        let event: KeyReleaseEvent = event;
        conn.conn.send_event(
            false,
            target,
            x11rb::protocol::xproto::EventMask::KEY_RELEASE,
            event,
        )
    };
    let _ = conn.conn.flush();
}

/// The keysym X names this key by.
///
/// Letters go over as lowercase: a keyboard mapping puts the unshifted symbol
/// first, so `A` is where the `a` keysym is and asking for the uppercase one
/// finds nothing on most layouts.
fn keysym(key: Key) -> Keysym {
    match key {
        Key::Letter(c) => Keysym::from(c.to_ascii_lowercase()),
        Key::Digit(c) => Keysym::from(c),
        // XK_F1 with the rest running consecutively, as far as F35.
        Key::Function(n) => 0xffbe + Keysym::from(n) - 1,
        Key::Space => 0x0020,
        Key::Enter => 0xff0d,
        Key::Backspace => 0xff08,
        Key::Delete => 0xffff,
        Key::Insert => 0xff63,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        // Named for the scroll they do, not the page they turn to.
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_symbols_match_what_x_names_them() {
        assert_eq!(keysym(Key::Space), 0x0020);
        assert_eq!(keysym(Key::Letter(b'A')), 0x0061);
        assert_eq!(keysym(Key::Digit(b'0')), 0x0030);
        assert_eq!(keysym(Key::Function(1)), 0xffbe);
        assert_eq!(keysym(Key::Function(24)), 0xffd5);
    }
}
