//! Keyboard event forwarding to host windows.
//!
//! A plugin editor is a child window, and child windows are the end of the line
//! for keyboard input: the platform delivers the key to whatever has focus and
//! does not pass it on to the ancestor the way it does for, say, a command. So
//! while the editor has focus the DAW hears nothing, and the space bar — which
//! every DAW binds to transport — stops working.
//!
//! Neither VST3 nor the GUI stack has a route for this. `IPlugView::onKeyDown`
//! runs the other way (host to plugin), and baseview reports a key as consumed
//! whether or not anything did anything with it. So the editor decides for
//! itself: if egui had no use for the key, post it to the DAW's own window and
//! let the DAW's accelerators see it.
//!
//! Only the key travels. Modifier state is left to the platform, which is still
//! accurate because the user is physically holding the modifier down while this
//! runs.

use crate::imp;

/// A key on its way back to the DAW.
///
/// Deliberately not a platform code. Windows names these as virtual keys and
/// X11 as keysyms, and the two disagree about nearly all of them, so the name
/// belongs to the backend that speaks it rather than to the caller.
///
/// The set is short on purpose: it is what a node editor has no use for and a
/// DAW binds to something. Punctuation is left out because its platform codes
/// depend on the keyboard layout, and guessing wrong sends the DAW a keystroke
/// the user did not type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `A`..=`Z`, held as the uppercase ASCII byte.
    Letter(u8),
    /// `0`..=`9`, held as the ASCII byte.
    Digit(u8),
    /// F1..=F24, held as the number.
    Function(u8),
    Space,
    Enter,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    /// A letter key, or `None` if `c` is not an ASCII letter.
    pub fn letter(c: char) -> Option<Key> {
        c.is_ascii_alphabetic()
            .then(|| Key::Letter(c.to_ascii_uppercase() as u8))
    }

    /// A digit key, or `None` if `c` is not an ASCII digit.
    pub fn digit(c: char) -> Option<Key> {
        c.is_ascii_digit().then_some(Key::Digit(c as u8))
    }

    /// A function key, or `None` outside F1..=F24.
    pub fn function(n: u8) -> Option<Key> {
        (1..=24).contains(&n).then_some(Key::Function(n))
    }
}

/// Post a key up or down to `window`, as if it had been typed there.
///
/// `window` is a root window handle as returned by [`crate::root_window`]. A
/// zero handle is ignored, so a caller that never found the DAW's window needs
/// no special case.
pub fn forward(window: usize, key: Key, pressed: bool) {
    imp::forward_key(window, key, pressed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_keys_a_daw_binds_are_accepted() {
        assert_eq!(Key::letter('q'), Some(Key::Letter(b'Q')));
        assert_eq!(Key::letter('7'), None);
        assert_eq!(Key::digit('7'), Some(Key::Digit(b'7')));
        assert_eq!(Key::digit('q'), None);
        assert_eq!(Key::function(12), Some(Key::Function(12)));
        assert_eq!(Key::function(0), None);
        assert_eq!(Key::function(25), None);
    }
}
