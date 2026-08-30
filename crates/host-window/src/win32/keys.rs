//! Win32 side of key forwarding.

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};

use crate::keys::Key;

pub(crate) fn forward_key(window: usize, key: Key, pressed: bool) {
    if window == 0 {
        return;
    }
    let Some(vk) = virtual_key(key) else { return };
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) };

    // The documented lParam layout: repeat count in 0..16, scan code in
    // 16..24, and for a release the "was down" and "transition" bits at 30
    // and 31. Some hosts read the scan code rather than the virtual key, so
    // it is worth filling in properly.
    let mut lparam: usize = 1 | ((scan as usize & 0xff) << 16);
    if !pressed {
        lparam |= (1 << 30) | (1 << 31);
    }

    let message = if pressed { WM_KEYDOWN } else { WM_KEYUP };
    unsafe {
        PostMessageW(
            window as HWND,
            message,
            vk as usize as WPARAM,
            lparam as LPARAM,
        )
    };
}

/// The Win32 virtual key code for a key.
///
/// Letters, digits and function keys are contiguous in both vocabularies, so
/// they need arithmetic rather than a table: `A`..`Z` and `0`..`9` share their
/// ASCII value with their virtual key, and VK_F1 is 0x70 with the rest running
/// consecutively.
fn virtual_key(key: Key) -> Option<u16> {
    Some(match key {
        Key::Letter(c) => u16::from(c),
        Key::Digit(c) => u16::from(c),
        Key::Function(n) => 0x6f + u16::from(n),
        Key::Space => 0x20,
        Key::Enter => 0x0d,
        Key::Backspace => 0x08,
        Key::Delete => 0x2e,
        Key::Insert => 0x2d,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_match_what_win32_names_them() {
        assert_eq!(virtual_key(Key::Space), Some(0x20));
        assert_eq!(virtual_key(Key::Letter(b'A')), Some(0x41));
        assert_eq!(virtual_key(Key::Digit(b'0')), Some(0x30));
        assert_eq!(virtual_key(Key::Function(1)), Some(0x70));
        assert_eq!(virtual_key(Key::Function(24)), Some(0x87));
    }
}
