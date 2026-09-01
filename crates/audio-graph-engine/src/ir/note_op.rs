//! The note half of a program.
//!
//! Notes used to have no runtime existence at all. A plugin carried a
//! `NoteRoute` — a source name, a gate lane and a key mask — that the compiler
//! folded out of the whole chain of nodes between the MIDI input and the
//! plugin, and the adapter turned back into events. Nothing flowed anywhere.
//!
//! That works only as long as every note node is a filter on one stream from
//! one source. It cannot express merging two streams, a node that *makes*
//! notes, or a control change turned into a signal, because there is no place
//! for the result to be. So notes get buffers, the way audio has buffers, and
//! the nodes between become ops that read one and write another.
//!
//! These run once per sub-block, before the audio ops of the same sub-block,
//! so a gate's decision is as current as any other parameter's.

/// An index into the note buffer pool.
pub type NoteBuf = u16;

/// Ceiling on the note buffer pool, so `activate` can size it once and never
/// grow. A filter whose gate and mask are both empty aliases its input rather
/// than taking a buffer of its own, so a long chain of open gates costs one.
pub const MAX_NOTE_BUFS: usize = 16;

/// How many events one note buffer holds for one sub-block.
///
/// Neither format can be told "I consumed fewer than you gave me", so there is
/// no back pressure to apply and nothing to carry over — an overflow is a drop,
/// counted and shown, never a `Vec` growing on the audio thread. A dense
/// controller lane is what makes this number matter; it is sized generously
/// rather than tightly because the memory is trivial next to the audio pool.
pub const NOTE_BUF_CAPACITY: usize = 256;

/// One step of the note half of a program.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteOp {
    /// Fill a buffer with what the DAW sent on `bus`, for this sub-block.
    Input { out: NoteBuf, bus: u16 },
    /// Copy `a` into `out`, dropping what this node refuses.
    ///
    /// `gate` names the lane carrying the open/shut decision, sampled per
    /// sub-block the way a mix gain is; below 0.5 the stream is shut. A shut
    /// gate holds note-ons back and lets everything else through, so a note
    /// already sounding still gets its note-off — blocking everything would
    /// leave a hung note behind whatever threw the gate.
    ///
    /// `mute` is a key mask: bit `k` set drops key `k`, note-on *and*
    /// note-off. Dropping both is what makes it safe, and it is the opposite
    /// case from a shut gate — the note-on never went, so nothing is waiting
    /// for a release. Events with no key of their own always pass: a control
    /// change carries the whole channel, and swallowing it because a key
    /// switch sits upstream would take the pedal with the keys.
    Filter {
        a: NoteBuf,
        out: NoteBuf,
        gate: Option<u16>,
        mute: u128,
    },
}
