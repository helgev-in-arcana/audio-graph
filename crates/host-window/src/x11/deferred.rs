//! X11 side of the deferred queue.
//!
//! There is no window and no message here. Win32 needs one because the thread's
//! message queue is the only place the DAW's pump will look; on X11 the pump is
//! this crate's own, so a queue registers itself with the connection and is run
//! from [`crate::poll`]. The guarantee the caller is buying — that work posted
//! from a draw callback does not run inside it — is unchanged.

use std::rc::Rc;

use super::conn::{Conn, conn};
use crate::deferred::{Deferred, Inner};

pub(crate) struct DeferredHandle {
    conn: Rc<Conn>,
    id: u64,
}

pub(crate) fn new_deferred() -> Result<Deferred, String> {
    let conn = conn()?;
    let inner = Inner::new();
    let id = conn.register_queue(&inner);
    Ok(Deferred::from_parts(inner, DeferredHandle { conn, id }))
}

pub(crate) fn wake_deferred(handle: &DeferredHandle) {
    handle.conn.wake();
}

pub(crate) fn destroy_deferred(handle: &DeferredHandle) {
    handle.conn.unregister_queue(handle.id);
}
