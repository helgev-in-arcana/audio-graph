//! C-string helpers.
//!
//! CLAP is a C API: everything the plugin says arrives as a NUL-terminated
//! `char*` of unknown provenance, and everything the host says has to be one.

use std::ffi::{CStr, c_char};

/// Read a plugin-owned `char*` into an owned `String`.
///
/// Null and invalid UTF-8 both become an empty/lossy string rather than an
/// error: a plugin with a mangled name is still a plugin the user can load, and
/// refusing to scan it helps nobody.
///
/// # Safety
/// `ptr` must be null or point at a NUL-terminated string that stays valid for
/// the duration of the call.
pub unsafe fn from_cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Read a fixed-size `char[N]` field, which CLAP uses for names and paths.
///
/// The array is NUL-terminated *within* its bounds by the format, but a plugin
/// that fills it completely leaves no terminator, so the length is capped
/// explicitly rather than trusted.
pub fn from_char_array(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
