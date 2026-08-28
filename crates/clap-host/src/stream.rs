//! In-memory stream adaptors for CLAP `clap_istream` and `clap_ostream`.
//!
//! Converts between opaque binary state buffers and CLAP's stream interfaces.

use std::ffi::c_void;

use clap_sys::stream::{clap_istream, clap_ostream};

/// A stream the plugin writes its state into.
pub(crate) struct OutStream {
    raw: clap_ostream,
    data: Vec<u8>,
}

impl OutStream {
    pub(crate) fn new() -> OutStream {
        OutStream {
            raw: clap_ostream {
                ctx: std::ptr::null_mut(),
                write: Some(write),
            },
            data: Vec::new(),
        }
    }

    /// Re-points `ctx` before handing the pointer over, so the struct is free
    /// to live on the caller's stack.
    pub(crate) fn as_raw(&mut self) -> *const clap_ostream {
        self.raw.ctx = (&raw mut *self).cast::<c_void>();
        &raw const self.raw
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

unsafe extern "C" fn write(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let ctx = unsafe { (*stream).ctx };
    if ctx.is_null() {
        return -1;
    }
    let out = unsafe { &mut *ctx.cast::<OutStream>() };
    let Ok(size) = usize::try_from(size) else {
        return -1;
    };
    let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), size) };
    out.data.extend_from_slice(bytes);
    // The whole request is always taken: this stream is a `Vec`, so a short
    // write would only make plugins loop for no reason.
    size as i64
}

/// A stream the plugin reads its state out of.
pub(crate) struct InStream<'a> {
    raw: clap_istream,
    data: &'a [u8],
    at: usize,
}

impl<'a> InStream<'a> {
    pub(crate) fn new(data: &'a [u8]) -> InStream<'a> {
        InStream {
            raw: clap_istream {
                ctx: std::ptr::null_mut(),
                read: Some(read),
            },
            data,
            at: 0,
        }
    }

    pub(crate) fn as_raw(&mut self) -> *const clap_istream {
        self.raw.ctx = (&raw mut *self).cast::<c_void>();
        &raw const self.raw
    }
}

unsafe extern "C" fn read(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let ctx = unsafe { (*stream).ctx };
    if ctx.is_null() {
        return -1;
    }
    let input = unsafe { &mut *ctx.cast::<InStream>() };
    let Ok(size) = usize::try_from(size) else {
        return -1;
    };
    // Zero is end of stream, which is how a plugin's read loop terminates.
    let take = size.min(input.data.len() - input.at);
    if take > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(
                input.data[input.at..].as_ptr(),
                buffer.cast::<u8>(),
                take,
            );
        }
        input.at += take;
    }
    take as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_what_is_read_back() {
        let mut out = OutStream::new();
        let raw = out.as_raw();
        let payload = b"sub-plugin state";
        let written =
            unsafe { (*raw).write.unwrap()(raw, payload.as_ptr().cast(), payload.len() as u64) };
        assert_eq!(written, payload.len() as i64);
        let blob = out.into_bytes();
        assert_eq!(blob, payload);

        let mut input = InStream::new(&blob);
        let raw = input.as_raw();
        let mut buf = [0u8; 64];
        let read_back =
            unsafe { (*raw).read.unwrap()(raw, buf.as_mut_ptr().cast(), buf.len() as u64) };
        assert_eq!(read_back, payload.len() as i64);
        assert_eq!(&buf[..payload.len()], payload);

        // A second read has nothing left, which is how a plugin's loop ends.
        let done = unsafe { (*raw).read.unwrap()(raw, buf.as_mut_ptr().cast(), buf.len() as u64) };
        assert_eq!(done, 0);
    }

    #[test]
    fn a_short_read_advances_by_what_it_took() {
        let blob = vec![1u8, 2, 3, 4, 5];
        let mut input = InStream::new(&blob);
        let raw = input.as_raw();
        let mut buf = [0u8; 2];
        for expected in [[1, 2], [3, 4]] {
            let n = unsafe { (*raw).read.unwrap()(raw, buf.as_mut_ptr().cast(), 2) };
            assert_eq!(n, 2);
            assert_eq!(buf, expected);
        }
        let n = unsafe { (*raw).read.unwrap()(raw, buf.as_mut_ptr().cast(), 2) };
        assert_eq!(n, 1, "only one byte was left");
    }
}
