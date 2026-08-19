//! An `IBStream` backed by a byte vector.
//!
//! VST3 passes state through a stream rather than a buffer, so both saving and
//! loading need one of these. Memory-backed is the right shape here: the
//! wrapper embeds the sub-plugin's chunk inside its own state (§8.3), so the
//! bytes have to end up in our hands anyway.

use std::cell::RefCell;
use std::ffi::c_void;

use vst3::Steinberg::{
    IBStream, IBStreamTrait, IBStream_::IStreamSeekMode_, ISizeableStream, ISizeableStreamTrait,
    int32, int64, kInvalidArgument, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

/// Read/write cursor over an owned buffer.
pub struct MemoryStream {
    inner: RefCell<Inner>,
}

struct Inner {
    data: Vec<u8>,
    pos: usize,
}

impl MemoryStream {
    /// An empty stream to write into (saving state).
    pub fn empty() -> ComWrapper<MemoryStream> {
        Self::from_bytes(Vec::new())
    }

    /// A stream positioned at the start of `data` (loading state).
    pub fn from_bytes(data: Vec<u8>) -> ComWrapper<MemoryStream> {
        ComWrapper::new(MemoryStream {
            inner: RefCell::new(Inner { data, pos: 0 }),
        })
    }

    /// Copy out everything written so far.
    pub fn contents(&self) -> Vec<u8> {
        self.inner.borrow().data.clone()
    }

    /// Rewind, so the same stream can be handed to a second reader.
    ///
    /// Needed because a component's state is offered to both the processor and
    /// the controller (`setComponentState`), and the second reader would
    /// otherwise start at EOF.
    pub fn rewind(&self) {
        self.inner.borrow_mut().pos = 0;
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream, ISizeableStream);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_read: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let mut inner = self.inner.borrow_mut();
        let available = inner.data.len().saturating_sub(inner.pos);
        let n = available.min(num_bytes as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(inner.data[inner.pos..].as_ptr(), buffer as *mut u8, n);
        }
        inner.pos += n;
        if !num_bytes_read.is_null() {
            unsafe { *num_bytes_read = n as int32 };
        }
        // A short read is not an error; the plugin decides what it needed.
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_written: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let n = num_bytes as usize;
        let mut inner = self.inner.borrow_mut();
        let end = inner.pos + n;
        if end > inner.data.len() {
            inner.data.resize(end, 0);
        }
        let pos = inner.pos;
        unsafe {
            std::ptr::copy_nonoverlapping(buffer as *const u8, inner.data[pos..].as_mut_ptr(), n);
        }
        inner.pos = end;
        if !num_bytes_written.is_null() {
            unsafe { *num_bytes_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        use IStreamSeekMode_::{kIBSeekCur, kIBSeekEnd, kIBSeekSet};

        let mut inner = self.inner.borrow_mut();
        let len = inner.data.len() as i64;
        let base = match mode {
            m if m == kIBSeekSet as int32 => 0,
            m if m == kIBSeekCur as int32 => inner.pos as i64,
            m if m == kIBSeekEnd as int32 => len,
            _ => return kInvalidArgument,
        };
        let target = base.saturating_add(pos);
        if target < 0 {
            return kInvalidArgument;
        }
        // Seeking past the end is legal and does not extend the buffer; a
        // subsequent write is what grows it.
        inner.pos = target as usize;
        if !result.is_null() {
            unsafe { *result = target };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = self.inner.borrow().pos as int64 };
        kResultOk
    }
}

impl ISizeableStreamTrait for MemoryStream {
    unsafe fn getStreamSize(&self, size: *mut int64) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        unsafe { *size = self.inner.borrow().data.len() as int64 };
        kResultOk
    }

    unsafe fn setStreamSize(&self, size: int64) -> tresult {
        if size < 0 {
            return kInvalidArgument;
        }
        let mut inner = self.inner.borrow_mut();
        inner.data.resize(size as usize, 0);
        inner.pos = inner.pos.min(inner.data.len());
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::Steinberg::IBStream_::IStreamSeekMode_::{kIBSeekCur, kIBSeekSet};

    #[test]
    fn write_then_read_round_trips() {
        let stream = MemoryStream::empty();
        let mut src = *b"hello world";
        let mut written = 0;
        unsafe {
            stream.write(src.as_mut_ptr() as *mut c_void, src.len() as int32, &mut written);
        }
        assert_eq!(written, 11);
        assert_eq!(stream.contents(), b"hello world");

        stream.rewind();
        let mut dst = [0u8; 5];
        let mut read = 0;
        unsafe {
            stream.read(dst.as_mut_ptr() as *mut c_void, 5, &mut read);
        }
        assert_eq!(read, 5);
        assert_eq!(&dst, b"hello");
    }

    #[test]
    fn reading_past_the_end_yields_a_short_read_not_an_error() {
        let stream = MemoryStream::from_bytes(b"ab".to_vec());
        let mut dst = [0u8; 8];
        let mut read = 0;
        let res = unsafe { stream.read(dst.as_mut_ptr() as *mut c_void, 8, &mut read) };
        assert_eq!(res, kResultOk);
        assert_eq!(read, 2);
    }

    #[test]
    fn seek_modes_resolve_against_the_right_origin() {
        let stream = MemoryStream::from_bytes(vec![0; 10]);
        let mut out = 0i64;
        unsafe {
            stream.seek(4, kIBSeekSet as int32, &mut out);
            assert_eq!(out, 4);
            stream.seek(3, kIBSeekCur as int32, &mut out);
            assert_eq!(out, 7);
            assert_eq!(stream.seek(-1, kIBSeekSet as int32, &mut out), kInvalidArgument);
        }
    }

    #[test]
    fn writing_at_an_offset_grows_the_buffer() {
        let stream = MemoryStream::empty();
        let mut out = 0i64;
        let mut src = *b"xy";
        unsafe {
            stream.seek(3, kIBSeekSet as int32, &mut out);
            stream.write(src.as_mut_ptr() as *mut c_void, 2, std::ptr::null_mut());
        }
        assert_eq!(stream.contents(), vec![0, 0, 0, b'x', b'y']);
    }
}
