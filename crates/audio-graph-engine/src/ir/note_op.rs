//! The note half of a program.
//!
//! Notes flow along buffers, the way audio does, and the nodes between them
//! are ops that read one and write another.
//!
//! Folding the chain away instead — compiling every node between a MIDI input
//! and a plugin down to a source name, a gate lane and a key mask that the
//! adapter turns back into events — costs nothing at runtime, and works only
//! as long as every note node is a filter on one stream from one source. It
//! cannot express merging two streams, a node that *makes* notes, or a control
//! change turned into a signal, because there is no place for the result to
//! be.
//!
//! These run once per row, before the audio ops of the same row,
//! so a gate's decision is as current as any other parameter's.

/// An index into the note buffer pool.
pub type NoteBuf = u16;

/// Ceiling on the note buffer pool, so `activate` can size it once and never
/// grow. A filter whose gate and mask are both empty aliases its input rather
/// than taking a buffer of its own, so a long chain of open gates costs one.
pub const MAX_NOTE_BUFS: usize = 16;

/// How many events one note buffer holds.
///
/// One whole DAW block, plus the last row of the block before it, which
/// is carried over so a parameter op reading at the first boundary of a block
/// has the stream that was in force there.
///
/// Neither format can be told "I consumed fewer than you gave me", so there is
/// no back pressure to apply — an overflow is a drop, counted and shown, never
/// a `Vec` growing on the audio thread. A dense controller lane is what makes
/// this number matter; it is sized generously rather than tightly because the
/// memory is trivial next to the audio pool.
pub const NOTE_BUF_CAPACITY: usize = 256;

/// The shortest gap between two controllers a [`NoteOp::Emit`] generates, in
/// samples: about a third of a millisecond at 48 kHz.
///
/// A floor of its own rather than the parameter resolution. A controller is a
/// note-stream event, and the stream has room for [`NOTE_BUF_CAPACITY`] of
/// them a block: at a resolution of one sample a single moving controller
/// would fill a 512-sample block's buffer halfway through, and what did not
/// fit — the note-offs included — would be dropped, leaving notes hanging.
/// Sixteen is the finest the controllers were ever sent at, and denser than
/// MIDI itself carries them by a factor of three.
pub const CC_INTERVAL: u32 = 16;

/// Every MIDI channel. What a node that has no opinion about channels says.
pub const ALL_CHANNELS: u16 = u16::MAX;

/// Every controller number.
pub const ALL_CONTROLLERS: u128 = u128::MAX;

/// Ceiling on the ops that remember the last value they sent.
pub const MAX_NOTE_EMITS: usize = 16;

/// How many [`NoteOp::Delay`]s one program may have. A ceiling because each
/// holds a queue sized once, in [`Engine::new`][crate::Engine::new].
pub const MAX_NOTE_DELAYS: usize = 16;

/// How many events one [`NoteOp::Delay`] can hold in flight: a buffer's worth.
pub const NOTE_DELAY_CAPACITY: usize = NOTE_BUF_CAPACITY;

/// How many streams one [`NoteOp::Merge`] joins. A `Mix`'s ceiling, for a
/// `Mix`'s reason: past it the node is a wall of sockets.
pub const MAX_MERGE_INPUTS: usize = 8;

/// Buffer numbers move with compilation; a stream is its producer and upstream meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoteStream {
    pub node: super::NodeId,
    pub port: u8,
    pub source: Option<NoteBuf>,
    pub kind: NoteStreamKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteStreamKind {
    Empty,
    Input(u16),
    Filter {
        gated: bool,
        mute: u128,
        channels: u16,
        controllers: u128,
    },
    Emit {
        channel: u8,
        cc: u8,
    },
    /// Several streams joined; which ones is the node's wiring, and `count`
    /// says how many were wired.
    Merge {
        count: u8,
    },
    Delay,
}

/// One step of the note half of a program.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteOp {
    /// Fill a buffer with what the DAW sent on `bus`, for this row.
    Input { out: NoteBuf, bus: u16 },
    /// Add a control change to a stream when the parameter on `lane` moves.
    ///
    /// `a` is the stream it joins, passed through first; `None` starts a fresh
    /// one. The generated event is timed at the start of the row, which
    /// is where the lane's value became true, and is written before the passed
    /// stream so the buffer stays sorted.
    ///
    /// `state` indexes the last value sent. Only a change is emitted — not to
    /// ration events, but because an unchanged controller is not an event.
    /// A program swap forgets it, so the next row re-sends the current
    /// value; a duplicate CC carrying the value the receiver already has is
    /// not something anyone can hear, and the alternative is carrying the
    /// state across recompiles for no gain.
    Emit {
        a: Option<NoteBuf>,
        out: NoteBuf,
        lane: u16,
        state: u16,
        channel: u8,
        cc: u8,
    },
    /// Copy `a` into `out`, dropping what this node refuses.
    ///
    /// `gate` names the lane carrying the open/shut decision, sampled per
    /// row the way a mix gain is; below 0.5 the stream is shut. A shut
    /// gate holds note-ons back and lets everything else through, so a note
    /// already sounding still gets its note-off — blocking everything would
    /// leave a hung note behind whatever threw the gate.
    ///
    /// `mute` is a key mask: bit `k` set drops key `k`, note-on *and*
    /// note-off. Dropping both is what makes it safe, and it is the opposite
    /// case from a shut gate — the note-on never went, so nothing is waiting
    /// for a release. Events with no key of their own are not affected: a
    /// control change carries the whole channel, and swallowing it because a
    /// key switch sits upstream would take the pedal with the keys.
    ///
    /// `channels` is a mask of the sixteen MIDI channels, and `controllers` a
    /// mask of the 128 controller numbers; set bits pass. They are here rather
    /// than on a filter of their own because every note node has to answer for
    /// them anyway — a key mute that quietly dropped channel 10 would be a
    /// worse surprise than one that says it passes everything.
    ///
    /// An event with no channel — raw bytes with no channel-voice status —
    /// passes any channel mask, on the same principle as a keyless event and a
    /// key mask.
    Filter {
        a: NoteBuf,
        out: NoteBuf,
        gate: Option<u16>,
        mute: u128,
        channels: u16,
        controllers: u128,
    },
    /// Join the first `count` of `inputs` into `out`, in time order.
    ///
    /// An event that another input has already put into this row's
    /// share of `out` is not put in again. That is what a note split into
    /// two branches and joined back looks like — the same note, id and all,
    /// arriving twice — and passing both would sound it twice and end it
    /// twice. Two different notes on one key are not duplicates: the graph
    /// gave them different ids, and both go through. Repeats within one input
    /// are that input's business and are kept.
    ///
    /// Stable across inputs: at one instant, the earlier input's events come
    /// first.
    Merge {
        inputs: [NoteBuf; MAX_MERGE_INPUTS],
        count: u8,
        out: NoteBuf,
    },
    /// Hand `a` on to `out` later: `time` seconds, or beats when `beats`
    /// is set, or what `lane` carries when the time is wired.
    ///
    /// Events wait in a queue that belongs to the node (`state`) and survives
    /// a program swap. Each is given the time it comes out as it goes in, and
    /// never earlier than the event queued before it: shortening the time
    /// while notes are in flight must not let a note-off overtake its note-on,
    /// which would leave the note sounding for good. A queue that is full
    /// drops what arrives, counted like any other overflow.
    ///
    /// A note-on waiting here is one the graph has not yet handed to any
    /// plugin, so the engine counts it as held by the delay until it comes
    /// out; see [`NoteLedger::delivered`][crate::notes::NoteLedger::delivered].
    Delay {
        a: NoteBuf,
        out: NoteBuf,
        state: u16,
        lane: Option<u16>,
        time: f64,
        beats: bool,
    },
}
