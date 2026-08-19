//! Conversions between VST3's fixed-size C string fields and Rust strings.

use vst3::Steinberg::{char16, char8};

/// Read a NUL-terminated (or field-filling) 8-bit string out of a fixed array.
///
/// VST3 does not promise a terminator when the text exactly fills the field,
/// so the length is bounded by the array either way.
pub fn from_char8(buf: &[char8]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Same for UTF-16 fields (`PClassInfoW`, and most `IEditController` strings).
pub fn from_char16(buf: &[char16]) -> String {
    let units: Vec<u16> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u16).collect();
    String::from_utf16_lossy(&units)
}

/// Read a `String128`-style buffer given only a pointer to its first element.
///
/// # Safety
/// `ptr` must point to at least `len` valid `char16`s.
pub unsafe fn from_char16_ptr(ptr: *const char16, len: usize) -> String {
    if ptr.is_null() {
        return String::new();
    }
    from_char16(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Write a Rust string into a fixed-size UTF-16 field, always NUL-terminating.
pub fn to_char16(src: &str, dst: &mut [char16]) {
    if dst.is_empty() {
        return;
    }
    let mut n = 0;
    for (unit, slot) in src.encode_utf16().zip(dst.iter_mut()) {
        *slot = unit as char16;
        n += 1;
    }
    if n < dst.len() {
        dst[n] = 0;
    } else {
        *dst.last_mut().unwrap() = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char8_stops_at_nul() {
        let mut buf = [0 as char8; 8];
        for (i, b) in b"hi".iter().enumerate() {
            buf[i] = *b as char8;
        }
        assert_eq!(from_char8(&buf), "hi");
    }

    #[test]
    fn char8_handles_unterminated_field() {
        let buf: Vec<char8> = b"abcd".iter().map(|&b| b as char8).collect();
        assert_eq!(from_char8(&buf), "abcd");
    }

    #[test]
    fn char16_round_trips() {
        let mut buf = [0 as char16; 16];
        to_char16("hello", &mut buf);
        assert_eq!(from_char16(&buf), "hello");
    }

    #[test]
    fn char16_truncates_and_terminates() {
        let mut buf = [1 as char16; 4];
        to_char16("abcdef", &mut buf);
        assert_eq!(buf[3], 0);
        assert_eq!(from_char16(&buf), "abc");
    }
}
