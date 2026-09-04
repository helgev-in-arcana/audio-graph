//! Realtime audio engine execution runtime.
//!
//! Executes compiled [`Program`] instructions on the audio thread.
//!
//! The rules this file exists to keep: no allocation, no locking, and no `Drop`
//! of anything the main thread gave us. Every buffer is sized once in
//! [`Engine::new`] against the compiler's ceilings, so adopting a new program
//! is a pointer swap and a short loop, never a resize.
//!
//! It also holds the state that must *survive* a swap — LFO phases, delay ring
//! contents, latch registers, the current note expression values.
//! Recompiling happens on every drag of every control, and an oscillator that
//! restarted each time would make the editor unusable for exactly the thing an
//! LFO is for.
//!
//! Outside of its own tests, nothing here mentions `graph` or a node kind, and
//! that is the thread boundary rather than an accident: what reaches this side
//! is a [`Program`] and nothing else. A `use crate::graph::…` appearing above
//! the `#[cfg(test)]` line is the signal that something has leaked across.

use plugin_host::{Event, NoteEvent};

use crate::handoff::Handoff;
use crate::ir::{
    AudioOp, Buf, Chunking, Detect, Follow, MAX_AUDIO_DELAY_LINES, MAX_BUFFER_CHANNELS,
    MAX_BUFFERS, MAX_CHANNELS, MAX_COMPENSATION, MAX_COMPENSATORS, MAX_DELAY_LINES, MAX_DELAY_TAPS,
    MAX_LATCHES, MAX_LFOS, MAX_NOTE_BUFS, MAX_NOTE_EMITS, MAX_REGISTERS, MathOp, NOTE_BUF_CAPACITY,
    NoteOp, Op, Operand, Program, RateSpec, Stage, Waveform,
};
use crate::nodes::db_to_linear;
use crate::notes::{Ended, NoteLedger};
use subhost_adapter::{AudioChunk, AudioInstances, MIN_QUANTUM};

/// Maximum number of `DelayRead` taps supported in a single program.
///
/// More than one read may share a line — that is a multi-tap delay, and it falls
/// out of splitting a delay into a write and a read for free — so this is not
/// `MAX_AUDIO_DELAY_LINES`. The engine keeps one number per tap: where its read
/// pointer was at the end of the last chunk, so the next one can ramp rather
/// than jump.
pub const MAX_AUDIO_TAPS: usize = 16;

/// Sentinel indicating that no ring buffer currently holds this delay line.
const NOT_PRESENT: usize = usize::MAX;

/// Context for evaluating one parameter sub-block.
#[derive(Debug, Clone, Copy)]
pub struct BlockContext {
    pub sample_rate: f64,
    pub tempo_bpm: f64,
    /// Number of audio frames processed during this evaluation. Phases advance
    /// by this much afterwards, which is what makes the sub-block rate a
    /// property of the caller rather than of the engine.
    pub frames: u32,
    /// Where this sub-block starts inside the DAW's block.
    pub offset: u32,
    /// Frames in the whole DAW block, which is how the audio buffers are
    /// packed. Only [`Op::Follow`] needs it — it is the one parameter op that
    /// reads one. See [`Window`].
    pub block: u32,
    /// Which sub-block this is, counting from the start of the DAW's block.
    ///
    /// The same number as the lane grid's row. Carried rather than divided out
    /// of `offset`, because the last sub-block of a block is short whenever the
    /// block is not a multiple of the quantum, and because each stage walks the
    /// sub-blocks from the start again so a counter would not do either.
    pub row: u32,
}

/// Context for evaluating one whole block of audio.
///
/// The lanes are the same buffer the parameter side fills: one row of values
/// per sub-block boundary. The audio half reads only its own range of lane
/// numbers out of it — delay times, and the like — and passes the rest through
/// untouched. It does not know what a parameter is.
#[derive(Debug, Clone, Copy)]
pub struct AudioContext<'a> {
    pub frames: u32,
    /// Sub-block chunk size in frames. Chunk boundaries are computed from it
    /// the same way the wrapper's slot schedule does, so chunk `i` and lane row
    /// `i` cover the same samples.
    pub quantum: u32,
    pub sample_rate: f64,
    pub lanes: &'a [f64],
    /// Number of lanes per sub-block row.
    pub lanes_per_row: usize,
}

impl AudioContext<'_> {
    fn lane(&self, row: usize, lane: u16) -> Option<f64> {
        self.lanes
            .get(row * self.lanes_per_row + lane as usize)
            .copied()
    }
}

/// How many events one DAW block may carry into the graph.
///
/// Sized here rather than from the block length because it is the DAW's
/// stream, not ours: a dense controller lane is what makes the number matter.
const MAX_BLOCK_EVENTS: usize = 1024;

/// Where one chunk of a block sits inside the buffer pool.
///
/// Channels are packed at the DAW's block length, never at the chunk's, so
/// where a sample lives does not depend on how big the call that wrote it was.
/// A buffer filled in one go has to be readable a sub-block at a time, and the
/// other way round, the moment two parts of a program run at different
/// granularities.
///
/// The sub-plugin boundary is the one place that still wants the chunk's own
/// packing, because that is what every plugin format's buffer layout means.
/// See [`AudioOp::Plugin`] in `run_chunk`.
#[derive(Clone, Copy)]
struct Window {
    /// Frames in the DAW's block: the distance between one channel and the
    /// next inside a buffer.
    block: usize,
    /// Where this chunk starts inside the block.
    start: usize,
    /// Frames this chunk covers.
    frames: usize,
}

/// Two disjoint `&mut` into one pool, for an op that copies between buffers.
///
/// The compiler never gives a filter the same buffer for both ends — a filter
/// that drops anything allocates its own output — so the panic is a bug in the
/// compiler, not a case a program can reach.
fn index_two<T>(pool: &mut [T], a: usize, b: usize) -> (&T, &mut T) {
    assert!(a != b, "a note filter reads and writes the same buffer");
    if a < b {
        let (front, back) = pool.split_at_mut(b);
        (&front[a], &mut back[0])
    } else {
        let (front, back) = pool.split_at_mut(a);
        (&back[0], &mut front[b])
    }
}

/// Whether an event survives a filter's masks — see [`NoteOp::Filter`].
///
/// Each mask judges only the events that have the thing it names. An event
/// with no key is not swallowed by a key mask, an event with no channel is not
/// swallowed by a channel mask, and only a control change is asked about
/// controller numbers.
fn passes(event: &Event, keys: u128, channels: u16, controllers: u128) -> bool {
    let Event::Note(note) = event else {
        return true;
    };
    if let Some(key) = note.key()
        && (0..128).contains(&key)
        && keys & (1u128 << key) != 0
    {
        return false;
    }
    if let Some(channel) = note.channel()
        && (0..16).contains(&channel)
        && channels & (1u16 << channel) == 0
    {
        return false;
    }
    if let NoteEvent::Cc { cc, .. } = note
        && controllers & (1u128 << (cc & 0x7f)) == 0
    {
        return false;
    }
    true
}

/// One note buffer: the events it holds, and what they amount to.
///
/// Kept together rather than as one array per reading. Every reader — a key
/// switch, a gate, a velocity follow, a controller latch — asks about one
/// buffer at a time, so what it wants is one of these rather than the same
/// index into five different tables.
///
/// The tables are per buffer rather than per engine because a stream is
/// something the graph routes. One table for the whole program, fed straight
/// from the DAW, would make a key switch fire on keys a filter upstream of it
/// had already taken out.
#[derive(Debug)]
struct NoteBuf {
    /// One whole DAW block of events, appended a sub-block at a time as the
    /// parameter half walks the rows, plus the last sub-block of the block
    /// before it. See [`NoteState`] and [`Engine::note_marks`].
    events: Vec<Event>,
    /// Which keys are down, one bit each.
    held: u128,
    /// Which keys were struck in the sub-block this buffer last carried, so an
    /// op sees each note-on exactly once.
    struck: u128,
    /// How many notes are down.
    count: u32,
    /// Velocity of the most recent note-on, and the key it was on, normalized.
    /// Both held between notes.
    velocity: f64,
    key: f64,
}

impl NoteBuf {
    fn new() -> NoteBuf {
        NoteBuf {
            events: Vec::with_capacity(NOTE_BUF_CAPACITY),
            held: 0,
            struck: 0,
            count: 0,
            velocity: 0.0,
            // The absence of a note is not the bottom of the keyboard, for the
            // same reason a pan sits in the middle.
            key: 0.5,
        }
    }

    /// Forget what is being played, without touching what is in the buffer.
    fn silence(&mut self) {
        self.held = 0;
        self.struck = 0;
        self.count = 0;
        self.velocity = 0.0;
        self.key = 0.5;
    }
}

/// What the note half fills in as it runs.
///
/// Held apart from the stream it reads ([`Engine::translated`]) rather than
/// beside it, and that is the whole point of the split: the note pass wants
/// the stream by shared reference and this by exclusive one, and a method
/// taking `&mut self` on a type that owned both could be given neither without
/// a dance.
///
/// The note ops run once for a sub-block, appending to what the buffers
/// already hold, and the two readers differ only in where they look: a
/// parameter op reads everything the buffer holds, which is the stream up to
/// the boundary it just crossed — the value in force at that instant, which is
/// what a parameter signal's sub-block resolution means. The audio half reads
/// the rows of its own chunk, found through [`Engine::note_marks`]. Replaying
/// them per reader instead, into buffers cleared before each replay, would
/// copy every event twice per note op, and force the generating ops to keep a
/// private "did it move" state per replay or the first replay would eat the
/// edge the second has to send.
#[derive(Debug)]
struct NoteState {
    /// One per note buffer, allocated in [`Engine::new`] and only ever cleared
    /// and refilled after that. A program swap happens on the audio thread, so
    /// nothing here may be sized from the program.
    bufs: Vec<NoteBuf>,
    /// Last value each controller-generating op sent, or NaN before its first.
    /// Forgotten on a program swap; see [`NoteOp::Emit`].
    emitted: Vec<f64>,
    /// Events dropped because a buffer was full, since the last reset.
    ///
    /// Counted rather than silently swallowed: an overflow is a real fault and
    /// the number is the only way anyone would find out.
    dropped: u64,
}

impl NoteState {
    fn new() -> NoteState {
        NoteState {
            bufs: (0..MAX_NOTE_BUFS).map(|_| NoteBuf::new()).collect(),
            emitted: vec![f64::NAN; MAX_NOTE_EMITS],
            dropped: 0,
        }
    }

    /// Appends unless the buffer is full. Dropping an event is bad; growing a
    /// `Vec` on the audio thread is worse.
    fn push(buf: &mut Vec<Event>, dropped: &mut u64, event: Event) {
        if buf.len() < buf.capacity() {
            buf.push(event);
        } else {
            *dropped += 1;
        }
    }

    /// One sub-block's worth of the note half, appended to what the buffers
    /// already hold. `base` is where each of them stood beforehand.
    ///
    /// On [`NoteState`] rather than on the engine, so the stream it reads can
    /// be handed in by shared reference from the same call. An engine method
    /// would borrow the whole engine, `translated` included, and the stream
    /// would have to be moved out and put back around every call.
    #[allow(clippy::too_many_arguments)]
    fn run_notes_step(
        &mut self,
        program: &Program,
        stage: Stage,
        events: &[Event],
        start: u32,
        frames: u32,
        lanes: &[f64],
        base: &[usize; MAX_NOTE_BUFS],
    ) {
        for op in &program.note_ops[stage.notes.range()] {
            match *op {
                NoteOp::Input { out, bus } => {
                    // One note bus so far. A second would be a second DAW note
                    // input, which the wrapper does not offer yet.
                    if bus != 0 {
                        continue;
                    }
                    // The stream is sorted, so the chunk is a range rather
                    // than a filter — and on the common block where every
                    // event falls in the first chunk, no per-event work at
                    // all.
                    let from = events.partition_point(|e| e.sample_offset() < start);
                    let to = events.partition_point(|e| e.sample_offset() < start + frames);
                    for &event in &events[from..to.max(from)] {
                        NoteState::push(
                            &mut self.bufs[out as usize].events,
                            &mut self.dropped,
                            event,
                        );
                    }
                    self.follow_notes(out, base[out as usize]);
                }
                NoteOp::Emit {
                    a,
                    out,
                    lane,
                    state,
                    channel,
                    cc,
                } => {
                    let value = lanes.get(lane as usize).copied().unwrap_or(0.0);
                    let value = value.clamp(0.0, 1.0);
                    let last = &mut self.emitted[state as usize];
                    // NaN on the left of a comparison is never equal, which is
                    // what makes the first sub-block after a swap send.
                    let moved = *last != value;
                    *last = value;

                    let event = Event::Note(NoteEvent::Cc {
                        port: 0,
                        channel: i16::from(channel),
                        cc,
                        value,
                        // The lane's value became true at the start of this
                        // sub-block, and writing it before the stream keeps
                        // the buffer sorted.
                        sample_offset: start,
                    });
                    match a {
                        Some(a) => {
                            let from = base[a as usize];
                            let (source, dest) =
                                index_two(&mut self.bufs, a as usize, out as usize);
                            let (source, dest) = (&source.events, &mut dest.events);
                            if moved {
                                NoteState::push(dest, &mut self.dropped, event);
                            }
                            for &passed in &source[from.min(source.len())..] {
                                NoteState::push(dest, &mut self.dropped, passed);
                            }
                        }
                        None => {
                            let dropped = &mut self.dropped;
                            if moved {
                                NoteState::push(
                                    &mut self.bufs[out as usize].events,
                                    dropped,
                                    event,
                                );
                            }
                        }
                    }
                }
                NoteOp::Filter {
                    a,
                    out,
                    gate,
                    mute,
                    channels,
                    controllers,
                } => {
                    // Below 0.5 the gate is shut. A gate whose lane is missing
                    // is a program the engine should not have been handed;
                    // shutting the stream is the quiet failure rather than the
                    // loud one.
                    let shut = gate
                        .is_some_and(|lane| !lanes.get(lane as usize).is_some_and(|&v| v >= 0.5));
                    let from = base[a as usize];
                    let (source, dest) = index_two(&mut self.bufs, a as usize, out as usize);
                    let (source, dest) = (&source.events, &mut dest.events);
                    for &event in &source[from.min(source.len())..] {
                        if shut && matches!(event, Event::Note(NoteEvent::NoteOn { .. })) {
                            continue;
                        }
                        if !passes(&event, mute, channels, controllers) {
                            continue;
                        }
                        NoteState::push(dest, &mut self.dropped, event);
                    }
                    self.follow_notes(out, base[out as usize]);
                }
            }
        }
    }

    /// Fold a buffer's notes into the tables the key and follow ops read.
    ///
    /// Called once per buffer per sub-block, over the events that sub-block
    /// appended and no others: the tables are a running total, and folding an
    /// event into them twice would leave a key held after it was let go.
    fn follow_notes(&mut self, buf: u16, from: usize) {
        let Some(buf) = self.bufs.get_mut(buf as usize) else {
            return;
        };
        let events = &buf.events[from.min(buf.events.len())..];
        let mut struck = 0u128;
        let mut held = buf.held;
        let mut count = buf.count;
        let mut velocity = buf.velocity;
        let mut key_track = buf.key;
        for event in events {
            match *event {
                Event::Note(NoteEvent::NoteOn {
                    key, velocity: v, ..
                }) => {
                    velocity = v;
                    key_track = f64::from(key).clamp(0.0, 127.0) / 127.0;
                    count = count.saturating_add(1);
                    if let Some(bit) = key_bit(key) {
                        held |= bit;
                        struck |= bit;
                    }
                }
                // NoteEnd is the plugin saying a voice finished, which is not
                // the player letting go; only a note-off lifts a key.
                Event::Note(NoteEvent::NoteOff { key, .. }) => {
                    count = count.saturating_sub(1);
                    if let Some(bit) = key_bit(key) {
                        held &= !bit;
                    }
                }
                _ => {}
            }
        }
        buf.struck = struck;
        buf.held = held;
        buf.count = count;
        buf.velocity = velocity;
        buf.key = key_track;
    }
}

pub struct Engine {
    program: Option<Box<Program>>,
    registers: Vec<f64>,
    /// LFO phase, 0..1, per state index of the current program.
    phases: Vec<f64>,
    /// Sample-and-hold values, per state index.
    holds: Vec<f64>,
    /// Scratch for carrying phases across a program swap. Sized once so the
    /// swap itself allocates nothing.
    carry: Vec<(u32, f64, f64)>,
    /// Which node each phase belongs to, mirrored from the program so the swap
    /// can match old state to new without touching the old program again.
    phase_nodes: Vec<u32>,
    /// One ring per parameter delay line, each `MAX_DELAY_TAPS` sub-blocks long.
    ///
    /// A `Vec<Vec<f64>>` rather than one flat buffer specifically so that a
    /// program swap can reorder the lines by swapping the outer entries, which
    /// moves pointers instead of 32 kB of samples.
    rings: Vec<Vec<f64>>,
    /// Current write head position per parameter delay line.
    ring_heads: Vec<usize>,
    /// Which `DelayWrite` node each ring belongs to. Same role as
    /// `phase_nodes`.
    ring_nodes: Vec<u32>,
    /// Scratch for reordering `rings` on a swap, so the swap allocates
    /// nothing.
    ring_order: Vec<usize>,
    /// The audio buffer pool, one `MAX_CHANNELS * max_frames` region per buffer
    /// index. Sized by [`prepare`][Engine::prepare], which is the only place in
    /// this type that allocates.
    pool: Vec<f32>,
    /// One sub-plugin call's channels, packed at the chunk's length rather
    /// than the block's. Used only when a chunk is shorter than the block it
    /// sits in; see [`Window`] and the `Plugin` arm of `run_chunk`.
    chunk_in: Vec<f32>,
    chunk_out: Vec<f32>,
    /// What the note half fills in. See [`NoteState`].
    notes: NoteState,
    /// Where each note buffer stood before each sub-block was appended to it.
    ///
    /// One row per sub-block the schedule can produce, sized in `prepare`. It
    /// is what lets the audio half find its chunk's events in a buffer that
    /// holds the whole block: a chunk covers a contiguous run of rows, so its
    /// events are `note_marks[first] .. note_marks[end]`.
    note_marks: Vec<[u32; MAX_NOTE_BUFS]>,
    /// How many sub-blocks of the current block the parameter half has run.
    /// Rows past this have stale marks and the buffer's end is the answer.
    note_rows: usize,
    /// The DAW's stream for the block being processed, with every note given
    /// the graph's own id.
    ///
    /// Translated once, before either half runs, because both of them read it
    /// and they must agree about which note is which. Doing it inside the note
    /// pass would hand out a fresh id for the same note-on in every stage that
    /// walks over it.
    translated: Vec<Event>,
    /// Who is who, and who still owes an ending. See [`crate::notes`].
    ledger: NoteLedger,
    /// Frames one buffer's channel holds. Zero until `prepare`.
    stride: usize,
    /// Channel width of each of the wrapper's own input buses, main first. Set
    /// by `prepare`, because it is fixed for as long as the DAW keeps us
    /// activated.
    daw_inputs: Vec<u16>,
    /// One ring per audio delay line, as long as the node asked for.
    ///
    /// Allocated on the main thread and carried in on the program, because this
    /// thread may not allocate and only that side knows both the graph's
    /// `max_time` and the sample rate. Split per line for the same reason the
    /// param rings are: a program swap reorders them by moving pointers.
    audio_rings: Vec<Vec<f32>>,
    /// Samples per channel in each of those, mirrored so the ops do not have to
    /// reach into the program for it.
    audio_ring_len: Vec<usize>,
    audio_ring_heads: Vec<usize>,
    audio_ring_nodes: Vec<u32>,
    audio_ring_order: Vec<usize>,
    /// Where each tap's read pointer stood at the end of the last chunk, in
    /// samples. NaN means "no previous", which is what a fresh program leaves
    /// behind and what makes the first chunk after a swap jump rather than sweep
    /// from wherever the old patch happened to be.
    tap_distance: Vec<f64>,
    /// Rings for latency compensation, one per compensated path.
    compensators: Vec<f32>,
    compensator_heads: Vec<usize>,
    /// One value per latch, or NaN for a latch nothing has set yet.
    latches: Vec<f64>,
    /// Which node each latch belongs to, so a program swap can carry it over.
    latch_nodes: Vec<u32>,
    /// Scratch for that swap, sized once so the swap itself allocates
    /// nothing.
    latch_carry: Vec<(u32, f64)>,
    rng: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// Move a delay line's contents into a differently sized ring.
///
/// The samples that matter are the most recent ones, so the copy walks
/// backwards from the old head and lands them at the end of the new ring.
/// Anything that no longer fits is the oldest of it, which is the part a
/// shorter line was never going to read again anyway.
///
/// `head` comes in pointing into the old ring and goes out pointing into the
/// new one.
fn copy_ring(from: &[f32], from_len: usize, to: &mut [f32], to_len: usize, head: &mut usize) {
    if from_len == 0 || to_len == 0 {
        *head = 0;
        return;
    }
    let keep = from_len.min(to_len);
    for ch in 0..MAX_CHANNELS {
        let (src, dst) = (ch * from_len, ch * to_len);
        for i in 0..keep {
            // `keep` samples ending at the old head, laid down ending at the
            // new one — which is `keep`, since the new ring starts empty.
            let at = (*head + from_len - keep + i) % from_len;
            if src + at < from.len() && dst + i < to.len() {
                to[dst + i] = from[src + at];
            }
        }
    }
    *head = keep % to_len;
}

/// Four-point cubic Hermite, the interpolator a modulated delay asks for.
///
/// Linear interpolation loses audible high end while the delay time is moving,
/// and an all-pass interpolator misbehaves under exactly the modulation this
/// exists to support. `x` is the fractional
/// offset between `y1` and `y2`.
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, x: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * x + c2) * x + c1) * x + y1
}

/// Move each ring to the index the new program gave its node, contents intact.
///
/// Work out the permutation first, then apply it by swapping the outer `Vec`
/// entries — no allocation, and no copying of ring contents, which for an audio
/// line is 96 000 samples a channel.
fn reorder<T: Copy>(
    rings: &mut [Vec<T>],
    heads: &mut [usize],
    nodes: &mut [u32],
    order: &mut [usize],
    want: &[u32],
    zero: T,
) {
    let lines = want.len().min(rings.len());
    for (i, slot) in order[..lines].iter_mut().enumerate() {
        *slot = nodes
            .iter()
            .position(|&n| n == want[i])
            .unwrap_or(NOT_PRESENT);
    }

    // Move the surviving rings into place first. Clearing as we went would wipe
    // a ring that is still sitting in a slot some later line wants.
    for i in 0..lines {
        let from = order[i];
        // `from` is never below `i`: slots below `i` already hold the rings of
        // earlier lines, whose nodes are all different from this one's.
        if from == NOT_PRESENT || from == i {
            continue;
        }
        rings.swap(i, from);
        heads.swap(i, from);
        // Whatever was at `i` now sits at `from`; a line still pointing at `i`
        // has to follow it there.
        for slot in order[i + 1..lines].iter_mut() {
            if *slot == i {
                *slot = from;
            }
        }
        order[i] = i;
    }
    // Whatever is left in a new line's slot belonged to a line that is gone.
    for i in 0..lines {
        if order[i] == NOT_PRESENT {
            rings[i].fill(zero);
            heads[i] = 0;
        }
        nodes[i] = want[i];
    }
    for node in nodes[lines..].iter_mut() {
        *node = u32::MAX;
    }
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            program: None,
            registers: vec![0.0; MAX_REGISTERS],
            phases: vec![0.0; MAX_LFOS],
            holds: vec![0.0; MAX_LFOS],
            carry: vec![(0, 0.0, 0.0); MAX_LFOS],
            phase_nodes: vec![u32::MAX; MAX_LFOS],
            rings: (0..MAX_DELAY_LINES)
                .map(|_| vec![0.0; MAX_DELAY_TAPS])
                .collect(),
            ring_heads: vec![0; MAX_DELAY_LINES],
            ring_nodes: vec![u32::MAX; MAX_DELAY_LINES],
            ring_order: vec![0; MAX_DELAY_LINES],
            // Empty until a program with a delay line in it arrives, and then
            // only as long as that line asked for.
            audio_rings: (0..MAX_AUDIO_DELAY_LINES).map(|_| Vec::new()).collect(),
            audio_ring_len: vec![0; MAX_AUDIO_DELAY_LINES],
            audio_ring_heads: vec![0; MAX_AUDIO_DELAY_LINES],
            audio_ring_nodes: vec![u32::MAX; MAX_AUDIO_DELAY_LINES],
            audio_ring_order: vec![0; MAX_AUDIO_DELAY_LINES],
            tap_distance: vec![f64::NAN; MAX_AUDIO_TAPS],
            pool: Vec::new(),
            daw_inputs: Vec::new(),
            stride: 0,
            // Sized here rather than in `prepare`, because the note pool
            // does not depend on the block size and a graph with no audio in
            // it still evaluates note ops.
            translated: Vec::with_capacity(MAX_BLOCK_EVENTS),
            ledger: NoteLedger::new(),
            notes: NoteState::new(),
            chunk_in: Vec::new(),
            chunk_out: Vec::new(),
            note_marks: Vec::new(),
            note_rows: 0,
            compensators: Vec::new(),
            compensator_heads: vec![0; MAX_COMPENSATORS],
            latches: vec![f64::NAN; MAX_LATCHES],
            latch_nodes: vec![u32::MAX; MAX_LATCHES],
            latch_carry: vec![(u32::MAX, f64::NAN); MAX_LATCHES],
            // Any odd seed; the sequence only has to be uncorrelated, not
            // unpredictable.
            rng: 0x2545_F491,
        }
    }

    /// Whether the graph currently drives `lane` — see
    /// [`Program::drives_lane`].
    pub fn drives_lane(&self, lane: usize) -> bool {
        self.program.as_ref().is_some_and(|p| p.drives_lane(lane))
    }

    pub fn has_program(&self) -> bool {
        self.program.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// Picks up a newly compiled program if one is waiting in the handoff channel.
    ///
    /// Returns `true` if a new program was adopted. Realtime-safe: does not allocate or lock.
    pub fn adopt(&mut self, handoff: &Handoff<Program>) -> bool {
        // Remember which node each running phase belongs to *before* the swap;
        // afterwards the old program is gone.
        let live = self.phase_nodes.len().min(self.phases.len());
        for i in 0..live {
            self.carry[i] = (self.phase_nodes[i], self.phases[i], self.holds[i]);
        }
        let carried = self
            .program
            .as_ref()
            .map_or(0, |p| p.lfo_nodes.len().min(MAX_LFOS));

        if !handoff.take(&mut self.program) {
            return false;
        }
        let next = self.program.as_ref().expect("take reported a swap");

        for (i, &node) in next.lfo_nodes.iter().take(MAX_LFOS).enumerate() {
            // Linear over at most MAX_LFOS entries.
            match self.carry[..carried].iter().find(|&&(id, _, _)| id == node) {
                Some(&(_, phase, hold)) => {
                    self.phases[i] = phase;
                    self.holds[i] = hold;
                }
                None => {
                    self.phases[i] = 0.0;
                    self.holds[i] = 0.0;
                }
            }
            self.phase_nodes[i] = node;
        }
        for i in next.lfo_nodes.len()..MAX_LFOS {
            self.phase_nodes[i] = u32::MAX;
        }

        // Latches keep their values across the swap so user switch settings persist.
        let latched = self
            .latch_carry
            .len()
            .min(self.latch_nodes.len())
            .min(self.latches.len());
        for i in 0..latched {
            self.latch_carry[i] = (self.latch_nodes[i], self.latches[i]);
        }
        for (i, &node) in next.latch_nodes.iter().take(MAX_LATCHES).enumerate() {
            self.latches[i] = self.latch_carry[..latched]
                .iter()
                .find(|&&(id, _)| id == node)
                .map_or(f64::NAN, |&(_, value)| value);
            self.latch_nodes[i] = node;
        }
        for i in next.latch_nodes.len()..MAX_LATCHES {
            self.latch_nodes[i] = u32::MAX;
            self.latches[i] = f64::NAN;
        }

        // Delay line ring buffers retain their contents across program swaps.
        reorder(
            &mut self.rings,
            &mut self.ring_heads,
            &mut self.ring_nodes,
            &mut self.ring_order,
            &next.delay_nodes,
            0.0,
        );
        reorder(
            &mut self.audio_rings,
            &mut self.audio_ring_heads,
            &mut self.audio_ring_nodes,
            &mut self.audio_ring_order,
            &next.audio_delay_nodes,
            0.0,
        );
        // When ring lengths change, new buffers provided by the main thread are swapped in.
        let next = self.program.as_mut().expect("take reported a swap");
        for line in 0..next.audio_delay_nodes.len().min(MAX_AUDIO_DELAY_LINES) {
            let len = next.audio_ring_len.get(line).copied().unwrap_or(0);
            if next.audio_rings.get(line).is_some_and(|r| !r.is_empty()) {
                std::mem::swap(&mut self.audio_rings[line], &mut next.audio_rings[line]);
                // Carry over what will still fit, most recent samples last.
                let from = &next.audio_rings[line];
                copy_ring(
                    from,
                    self.audio_ring_len[line],
                    &mut self.audio_rings[line],
                    len,
                    &mut self.audio_ring_heads[line],
                );
                self.audio_ring_len[line] = len;
            } else if self.audio_ring_len[line] != len {
                self.audio_ring_len[line] = 0;
            }
        }
        for line in next.audio_delay_nodes.len()..MAX_AUDIO_DELAY_LINES {
            self.audio_ring_len[line] = 0;
        }
        self.tap_distance.iter_mut().for_each(|d| *d = f64::NAN);
        true
    }

    /// Give back whatever program is loaded, so the main thread can free it.
    ///
    /// Called when the plugin is torn down, from the main thread.
    pub fn release(&mut self) -> Option<Box<Program>> {
        self.program.take()
    }

    /// Clears held notes and resets internal phase/delay head counters on host transport jump.
    ///
    /// This intentionally does NOT clear key switch latches, because latches should survive
    /// transport jumps (e.g. seeking in the DAW timeline).
    pub fn reset(&mut self) {
        self.ledger.clear();
        self.translated.clear();
        self.notes.bufs.iter_mut().for_each(NoteBuf::silence);
        self.phases.iter_mut().for_each(|p| *p = 0.0);
        self.rings.iter_mut().for_each(|r| r.fill(0.0));
        self.ring_heads.iter_mut().for_each(|h| *h = 0);
        self.pool.fill(0.0);
        self.compensators.fill(0.0);
        self.compensator_heads.iter_mut().for_each(|h| *h = 0);
        // Audio delay rings are sized and allocated by the main thread via Program::size_rings.
        self.audio_ring_heads.iter_mut().for_each(|h| *h = 0);
    }

    /// Allocates and sizes audio buffers for worst-case limits. Called from the main thread on activation.
    pub fn prepare(&mut self, max_frames: u32, daw_inputs: &[u16]) {
        self.stride = max_frames as usize;
        self.daw_inputs.clear();
        self.daw_inputs.extend_from_slice(daw_inputs);
        self.pool.clear();
        self.pool
            .resize(MAX_BUFFERS * MAX_BUFFER_CHANNELS * self.stride, 0.0);
        for scratch in [&mut self.chunk_in, &mut self.chunk_out] {
            scratch.clear();
            scratch.resize(MAX_BUFFER_CHANNELS * self.stride, 0.0);
        }
        for buf in &mut self.notes.bufs {
            buf.events.clear();
        }
        self.notes.emitted.iter_mut().for_each(|v| *v = f64::NAN);
        // One row per sub-block the schedule can cut the block into, at the
        // finest quantum it offers, plus one so a chunk ending on the last row
        // still has a row to ask about.
        self.note_marks.clear();
        self.note_marks
            .resize(self.stride / MIN_QUANTUM as usize + 2, [0; MAX_NOTE_BUFS]);
        self.note_rows = 0;
        self.notes.dropped = 0;
        self.compensators.clear();
        self.compensators
            .resize(MAX_COMPENSATORS * MAX_CHANNELS * MAX_COMPENSATION, 0.0);
        self.compensator_heads.iter_mut().for_each(|h| *h = 0);
        // Audio delay rings are sized and allocated by the main thread via Program::size_rings.
        self.audio_ring_heads.iter_mut().for_each(|h| *h = 0);
    }

    /// Take in the DAW's note stream for one block.
    ///
    /// Called once, before the parameter half runs. Every note-on gets an id of
    /// the graph's own here, and every note-off is matched back to the note it
    /// ends — by address, because neither format promises the note-off will
    /// carry an id at all.
    ///
    /// This is also where the note buffers are emptied — all but the last
    /// sub-block of the block that just ended.
    ///
    /// That tail is what the first sub-block of this block reads. A parameter
    /// op reads the stream in force at the boundary it has just crossed, and
    /// at the first boundary of a block that stream belongs to the block
    /// before: dropping it would make every controller snap back to its
    /// starting value once per DAW block. The audio half never sees it,
    /// because its rows start at `note_marks[0]`, which is recorded after the
    /// carry-over is already in place.
    pub fn begin_block(&mut self, events: &[Event]) {
        for (buf, pool) in self.notes.bufs.iter_mut().enumerate() {
            // Everything before the last row starts is spent: the parameter
            // half has read past it and the plugins have been handed it.
            let spent = match self.note_rows.checked_sub(1) {
                Some(last) => self
                    .note_marks
                    .get(last)
                    .and_then(|marks| marks.get(buf))
                    .map_or(0, |&at| (at as usize).min(pool.events.len())),
                // No row ran, so there is no boundary to carry.
                None => pool.events.len(),
            };
            pool.events.drain(..spent);
        }
        self.note_rows = 0;
        self.translated.clear();
        for &event in events {
            if self.translated.len() == self.translated.capacity() {
                self.notes.dropped += 1;
                continue;
            }
            self.translated.push(match event {
                Event::Note(note) => Event::Note(self.ledger.translate(note)),
                other => other,
            });
        }
    }

    /// Settle the block and collect the notes the graph has finished with.
    ///
    /// `from_plugins` is what the sub-plugins emitted; a `NoteEnd` in it is one
    /// of them saying it is done with a note. `ended` comes back holding the
    /// notes to report to the DAW, addressed the way the DAW will recognise.
    pub fn end_block(&mut self, from_plugins: &[Event], ended: &mut Vec<Ended>) {
        for event in from_plugins {
            if let Event::Note(NoteEvent::NoteEnd {
                note_id: Some(id), ..
            }) = event
            {
                self.ledger.finished(*id);
            }
        }
        self.ledger.end_block(ended);
    }

    /// How many notes have been forced out of the ledger to make room.
    pub fn notes_stolen(&self) -> u64 {
        self.ledger.stolen()
    }

    /// Returns the latency in samples reported by the compiled program.
    pub fn latency(&self) -> u32 {
        self.program.as_ref().map_or(0, |p| p.latency)
    }

    /// Returns whether the current program contains audio operations.
    pub fn has_audio(&self) -> bool {
        self.program
            .as_ref()
            .is_some_and(|p| !p.audio_ops.is_empty())
    }

    /// How many stages the active program is cut into.
    ///
    /// A caller that runs audio walks these itself, alternating
    /// [`run_stage`][Engine::run_stage] over the sub-blocks with
    /// [`run_audio_stage`][Engine::run_audio_stage], because a stage's
    /// parameters may be read off the audio the stage before it made.
    pub fn stages(&self) -> usize {
        self.program.as_ref().map_or(0, |p| p.stages.len())
    }

    /// The finest granularity anything in the active program runs at.
    ///
    /// A summary: the program is cut into stages and only the one holding a
    /// delay line's two ends runs a sub-block at a time. See [`Stage`].
    pub fn chunking(&self) -> Chunking {
        let looped = self.program.as_ref().is_some_and(|program| {
            program
                .stages
                .iter()
                .any(|stage| stage.chunking == Chunking::SubBlock)
        });
        if looped {
            Chunking::SubBlock
        } else {
            Chunking::WholeBlock
        }
    }

    /// Executes the audio pipeline for a block provided by the audio host.
    ///
    /// Evaluates operations at whole-block or sub-block chunking depending on whether
    /// audio feedback delay loops are present.
    ///
    /// Runs after [`begin_block`][Engine::begin_block] and one
    /// [`run`][Engine::run] per sub-block, in that order. The note buffers are
    /// filled by those calls and read here; calling this without them hands
    /// the sub-plugins the previous block's events, or none at all.
    ///
    /// Every stage in one call. See [`Engine::run`] for what that costs and
    /// when it costs nothing.
    pub fn run_audio(
        &mut self,
        ctx: &AudioContext<'_>,
        daw_in: &[f32],
        daw_out: &mut [f32],
        nodes: &mut dyn AudioInstances,
    ) {
        self.clear_output(daw_out);
        for stage in 0..self.stages() {
            self.run_audio_stage(stage, ctx, daw_in, daw_out, nodes);
        }
    }

    /// The block is the program's to fill: a channel no `Output` op reaches is
    /// silence, not whatever the caller's buffer already held. Called once
    /// before the stages, because each of them writes only its own part.
    pub fn clear_output(&self, daw_out: &mut [f32]) {
        daw_out.fill(0.0);
    }

    /// One stage's audio ops, over the whole block.
    ///
    /// A stage covers the block before the next one starts, so a stage that
    /// steps a sub-block at a time hands the one after it a finished buffer.
    /// Only the stage holding a delay line's two ends steps; the rest of the
    /// program is called once, however many loops are drawn elsewhere in the
    /// patch.
    pub fn run_audio_stage(
        &mut self,
        stage: usize,
        ctx: &AudioContext<'_>,
        daw_in: &[f32],
        daw_out: &mut [f32],
        nodes: &mut dyn AudioInstances,
    ) {
        let total = ctx.frames as usize;
        if self.stride == 0 || total > self.stride {
            return;
        }
        let Some(program) = self.program.take() else {
            return;
        };
        if let Some(&stage) = program.stages.get(stage) {
            let step = match stage.chunking {
                Chunking::WholeBlock => total.max(1),
                // The last chunk is short whenever the block is not a multiple
                // of the quantum.
                Chunking::SubBlock => (ctx.quantum as usize).max(1),
            };
            let mut start = 0usize;
            let mut row = 0usize;
            while start < total {
                let len = step.min(total - start);
                self.run_chunk(
                    &program, stage, ctx, nodes, daw_in, daw_out, start, len, row,
                );
                start += len;
                row += 1;
            }
        }

        self.program = Some(program);
    }

    /// Where the rows `first..end` sit in note buffer `buf`.
    ///
    /// The buffer holds the whole block, so this is how the audio half asks
    /// for its own chunk's events without the note half having to run again.
    /// A row past what the parameter half has filled reads to the end, which
    /// is what makes the last chunk right whether or not the block divides
    /// evenly by the quantum.
    fn note_slice(&self, buf: u16, first: usize, end: usize) -> std::ops::Range<usize> {
        let len = self
            .notes
            .bufs
            .get(buf as usize)
            .map_or(0, |buf| buf.events.len());
        let mark = |row: usize| {
            self.note_marks
                .get(row)
                .and_then(|marks| marks.get(buf as usize))
                .map_or(len, |&at| (at as usize).min(len))
        };
        // Not zero for row 0: what sits before it is the carry-over from the
        // previous block, which the parameter half reads and the plugins have
        // already been handed.
        let from = mark(first);
        let to = if end >= self.note_rows {
            len
        } else {
            mark(end)
        };
        from..to.max(from)
    }

    /// How many events have been dropped for want of buffer space.
    pub fn notes_dropped(&self) -> u64 {
        self.notes.dropped
    }

    /// One chunk of `run_audio`: every op, over `len` frames starting at
    /// `start` inside the DAW's block.
    #[allow(clippy::too_many_arguments)]
    fn run_chunk(
        &mut self,
        program: &Program,
        stage: Stage,
        ctx: &AudioContext<'_>,
        nodes: &mut dyn AudioInstances,
        daw_in: &[f32],
        daw_out: &mut [f32],
        start: usize,
        frames: usize,
        row: usize,
    ) {
        let block = ctx.frames as usize;
        let mut tap = 0usize;
        // Which rows of the note buffers this chunk covers. The buffers were
        // filled by the parameter half and hold the whole block; a chunk is a
        // contiguous run of rows, so its events are a contiguous slice.
        let first_row = row;
        let end_row = row + frames.div_ceil((ctx.quantum as usize).max(1));
        let win = Window {
            block,
            start,
            frames,
        };

        for op in &program.audio_ops[stage.audio.range()] {
            match op {
                AudioOp::Silence { out } => self.fill(*out, win, 0.0),
                AudioOp::Input { out, bus } => {
                    let width = program.buffers[*out as usize] as usize;
                    // daw_in holds interleaved planar buses.
                    let bus = *bus as usize;
                    let Some(&have) = self.daw_inputs.get(bus) else {
                        self.fill(*out, win, 0.0);
                        continue;
                    };
                    // Both sides are packed at the block's length, so the two
                    // walk in step and only the bus base differs.
                    let base: usize = self.daw_inputs[..bus]
                        .iter()
                        .map(|&c| c as usize * block)
                        .sum();
                    for ch in 0..width.min(MAX_CHANNELS) {
                        let to = self.at(*out, ch, win);
                        if ch >= have as usize {
                            self.pool[to..to + frames].fill(0.0);
                            continue;
                        }
                        let from = base + ch * block + start;
                        for i in 0..frames {
                            self.pool[to + i] = daw_in.get(from + i).copied().unwrap_or(0.0);
                        }
                    }
                }
                AudioOp::Output { a, bus } => {
                    if *bus != 0 {
                        continue;
                    }
                    let width = program.buffers[*a as usize] as usize;
                    for ch in 0..width.min(MAX_CHANNELS) {
                        let from = self.at(*a, ch, win);
                        let to = ch * block + start;
                        if to + frames <= daw_out.len() {
                            daw_out[to..to + frames]
                                .copy_from_slice(&self.pool[from..from + frames]);
                        }
                    }
                }
                AudioOp::Gather { out, buses } => {
                    // Assembles the plugin input buffer across connected buses.
                    // Width conversions are performed here during assembly.
                    let mut at = 0usize;
                    for &(from, want) in buses {
                        let have = program.buffers[from as usize];
                        for ch in 0..want {
                            let to = self.at(*out, at + ch as usize, win);
                            if have == 1 && want > 1 {
                                // Mono into a wider bus: the same signal on
                                // every channel, which is what a host does.
                                let src = self.at(from, 0, win);
                                self.pool.copy_within(src..src + frames, to);
                            } else if want == 1 && have > 1 {
                                // Wider into mono: averaged, the inverse of the
                                // branch above, so a round trip keeps its
                                // level. Taking the left channel alone would
                                // ignore half the signal.
                                let first = self.at(from, 0, win);
                                self.pool.copy_within(first..first + frames, to);
                                for other in 1..have {
                                    let src = self.at(from, other as usize, win);
                                    for i in 0..frames {
                                        self.pool[to + i] += self.pool[src + i];
                                    }
                                }
                                let scale = 1.0 / have as f32;
                                for i in 0..frames {
                                    self.pool[to + i] *= scale;
                                }
                            } else if ch < have {
                                let src = self.at(from, ch as usize, win);
                                self.pool.copy_within(src..src + frames, to);
                            } else {
                                self.pool[to..to + frames].fill(0.0);
                            }
                        }
                        at += want as usize;
                    }
                }
                AudioOp::Split {
                    from,
                    out,
                    channel,
                    width,
                } => {
                    // One bus out of a plugin's output region. No conversion:
                    // both sides are the width the plugin negotiated.
                    for ch in 0..*width as usize {
                        let src = self.at(*from, *channel as usize + ch, win);
                        let dst = self.at(*out, ch, win);
                        self.pool.copy_within(src..src + frames, dst);
                    }
                }
                AudioOp::Plugin {
                    instance,
                    input,
                    input_buses,
                    output,
                    output_buses,
                    notes,
                } => {
                    // Worked out before the pool is split, because that borrow
                    // covers the rest of the arm.
                    let heard = notes.map(|buf| self.note_slice(buf, first_row, end_row));
                    // The compiler guarantees these differ, so the two regions
                    // cannot overlap and `split_at_mut` is enough to prove it.
                    let span = MAX_BUFFER_CHANNELS * self.stride;
                    let (lo, hi) = if input < output {
                        (*input as usize, *output as usize)
                    } else {
                        (*output as usize, *input as usize)
                    };
                    let (front, back) = self.pool.split_at_mut(hi * span);
                    let low = &mut front[lo * span..lo * span + span];
                    let high = &mut back[..span];
                    let (source, dest) = if input < output {
                        (&low[..], high)
                    } else {
                        (&high[..], low)
                    };
                    let in_width: u16 = input_buses.iter().sum();
                    let out_width: u16 = output_buses.iter().sum();
                    // Only what the plugin will actually read is handed over.
                    // The buffer behind it is as wide as any buffer in the
                    // pool; the region it owns is sized for its active buses.
                    let packed_in = in_width as usize * frames;
                    let packed_out = out_width as usize * frames;
                    // A plugin is handed its channels packed at the length of
                    // the call, which is what every format's buffer layout
                    // means. The pool packs at the block's length instead, so
                    // that a buffer written whole can be read a sub-block at a
                    // time. The two agree whenever the chunk *is* the block —
                    // the common case, handed over where it lies — and a
                    // shorter chunk is gathered into a scratch and scattered
                    // back. Only a program with a feedback loop in it pays
                    // that, and it is already paying for the extra calls.
                    let short = frames != block;
                    if short {
                        for ch in 0..in_width as usize {
                            let from = ch * block + start;
                            let to = ch * frames;
                            self.chunk_in[to..to + frames]
                                .copy_from_slice(&source[from..from + frames]);
                        }
                    }
                    // An unwired notes port hears nothing, which is not the
                    // same as hearing an empty buffer only because this chunk
                    // was quiet.
                    let events: &[Event] = match (notes, heard) {
                        (Some(buf), Some(range)) => &self.notes.bufs[*buf as usize].events[range],
                        _ => &[],
                    };
                    // Counted here, where the note is actually handed over, and
                    // not where a wire branches: a branch a gate later swallows
                    // would never be counted back down, and the note would
                    // never be reported ended.
                    for event in events {
                        if let Event::Note(NoteEvent::NoteOn {
                            note_id: Some(id), ..
                        }) = event
                        {
                            self.ledger.delivered(*id);
                        }
                    }
                    let (heard_in, heard_out): (&[f32], &mut [f32]) = if short {
                        (
                            &self.chunk_in[..packed_in],
                            &mut self.chunk_out[..packed_out],
                        )
                    } else {
                        (&source[..packed_in], &mut dest[..packed_out])
                    };
                    nodes.process(
                        *instance,
                        events,
                        heard_in,
                        heard_out,
                        AudioChunk {
                            input_channels: in_width,
                            output_channels: out_width,
                            aux_inputs: plugin_host::AuxBuses::new(
                                input_buses.get(1..).unwrap_or(&[]),
                            ),
                            aux_outputs: plugin_host::AuxBuses::new(
                                output_buses.get(1..).unwrap_or(&[]),
                            ),
                            frames: frames as u32,
                            offset: start as u32,
                        },
                    );
                    if short {
                        for ch in 0..out_width as usize {
                            let from = ch * frames;
                            let to = ch * block + start;
                            dest[to..to + frames]
                                .copy_from_slice(&self.chunk_out[from..from + frames]);
                        }
                    }
                }
                AudioOp::Mix { out, inputs } => {
                    if inputs.is_empty() {
                        self.fill(*out, win, 0.0);
                        continue;
                    }
                    let width = program.buffers[*out as usize] as usize;
                    for (n, input) in inputs.iter().enumerate() {
                        let gain = input
                            .lane
                            .and_then(|lane| ctx.lane(row, lane))
                            .map(|db| db_to_linear(db) as f32)
                            .unwrap_or(input.gain as f32);
                        for ch in 0..width.min(MAX_CHANNELS) {
                            let from = self.at(input.buf, ch, win);
                            let to = self.at(*out, ch, win);
                            if from == to && gain == 1.0 {
                                // Already in place and unchanged: unity gain on destination buffer.
                                continue;
                            }
                            for i in 0..frames {
                                let value = self.pool[from + i] * gain;
                                if n == 0 {
                                    self.pool[to + i] = value;
                                } else {
                                    self.pool[to + i] += value;
                                }
                            }
                        }
                    }
                }
                AudioOp::Compensate { buf, slot, samples } => {
                    let width = program.buffers[*buf as usize] as usize;
                    self.compensate(*buf, *slot as usize, *samples as usize, width, win);
                }
                AudioOp::DelayRead {
                    out,
                    line,
                    lane,
                    time,
                    max_time,
                } => {
                    let index = tap;
                    tap += 1;
                    let seconds = lane
                        .and_then(|lane| ctx.lane(row, lane))
                        .unwrap_or(*time)
                        .max(0.0);
                    let width = program.buffers[*out as usize] as usize;
                    self.delay_read(*line as usize, index, *out, width, win, {
                        // Minimum floor in samples, plus the two samples the
                        // interpolator needs ahead of the read pointer.
                        let floor = frames as f64 + 2.0;
                        let ceiling = (max_time * ctx.sample_rate)
                            .min(self.audio_ring_len[*line as usize].saturating_sub(4) as f64)
                            .max(floor);
                        (seconds * ctx.sample_rate).clamp(floor, ceiling)
                    });
                }
                AudioOp::DelayWrite { line, a } => {
                    let width = program.buffers[*a as usize] as usize;
                    self.delay_write(*line as usize, *a, width, win);
                }
                AudioOp::DelaySilence { line } => {
                    self.delay_silence(*line as usize, frames);
                }
            }
        }
    }

    /// Where this chunk of one channel of one buffer starts in the pool.
    ///
    /// Each buffer owns a region sized for the longest block, with the channels
    /// packed at the block's length. See [`Window`].
    fn at(&self, buf: Buf, channel: usize, win: Window) -> usize {
        buf as usize * MAX_BUFFER_CHANNELS * self.stride + channel * win.block + win.start
    }

    fn fill(&mut self, buf: Buf, win: Window, value: f32) {
        for ch in 0..MAX_CHANNELS {
            let start = self.at(buf, ch, win);
            self.pool[start..start + win.frames].fill(value);
        }
    }

    /// Reads samples from an audio delay line into `buf` with cubic Hermite interpolation.
    ///
    /// Smooths the read pointer across chunks to prevent clicks during delay modulation.
    fn delay_read(
        &mut self,
        line: usize,
        tap: usize,
        buf: Buf,
        width: usize,
        win: Window,
        distance: f64,
    ) {
        let frames = win.frames;
        let ring_len = self.audio_ring_len.get(line).copied().unwrap_or(0);
        if ring_len == 0 || self.audio_rings[line].len() < MAX_CHANNELS * ring_len {
            self.fill(buf, win, 0.0);
            return;
        }
        // NaN on the first chunk after a swap, and on the very first block.
        let from = match self.tap_distance.get(tap).copied() {
            Some(previous) if previous.is_finite() => previous,
            _ => distance,
        };
        let head = self.audio_ring_heads[line];
        for ch in 0..width.min(MAX_CHANNELS) {
            let ring = ch * ring_len;
            let to = self.at(buf, ch, win);
            for i in 0..frames {
                // The sweep lands exactly on `distance` at the last sample.
                let t = (i + 1) as f64 / frames as f64;
                let d = from + (distance - from) * t;
                let position = (head + i) as f64 - d;
                let whole = position.floor();
                let fraction = position - whole;
                let at = whole as i64;
                let y = |offset: i64| -> f32 {
                    let index = (at + offset).rem_euclid(ring_len as i64) as usize;
                    self.audio_rings[line][ring + index]
                };
                self.pool[to + i] = hermite(y(-1), y(0), y(1), y(2), fraction as f32);
            }
        }
        if let Some(slot) = self.tap_distance.get_mut(tap) {
            *slot = distance;
        }
    }

    /// Append this chunk of `buf` to `line`.
    ///
    /// Every read in the chunk has already run — the compiler holds the writes
    /// back for exactly that reason — so the head this advances is the one the
    /// reads saw, and a delay of one chunk reads the chunk before it rather
    /// than itself.
    fn delay_write(&mut self, line: usize, buf: Buf, width: usize, win: Window) {
        let frames = win.frames;
        let ring_len = self.audio_ring_len.get(line).copied().unwrap_or(0);
        if ring_len == 0 || self.audio_rings[line].len() < MAX_CHANNELS * ring_len {
            return;
        }
        let head = self.audio_ring_heads[line];
        for ch in 0..MAX_CHANNELS {
            let ring = ch * ring_len;
            // A channel the source does not have still has to be written, or
            // the line would keep replaying whatever a wider patch left there.
            let from = self.at(buf, ch, win);
            for i in 0..frames {
                let at = (head + i) % ring_len;
                self.audio_rings[line][ring + at] = if ch < width.min(MAX_CHANNELS) {
                    self.pool[from + i]
                } else {
                    0.0
                };
            }
        }
        self.audio_ring_heads[line] = (head + frames) % ring_len;
    }

    /// Advance `line`'s write head over this chunk without a source.
    ///
    /// The head moves at the rate a connected write would move it, so the line
    /// drains over its delay time rather than holding still.
    fn delay_silence(&mut self, line: usize, frames: usize) {
        let ring_len = self.audio_ring_len.get(line).copied().unwrap_or(0);
        if ring_len == 0 || self.audio_rings[line].len() < MAX_CHANNELS * ring_len {
            return;
        }
        let head = self.audio_ring_heads[line];
        for ch in 0..MAX_CHANNELS {
            let ring = ch * ring_len;
            for i in 0..frames {
                self.audio_rings[line][ring + (head + i) % ring_len] = 0.0;
            }
        }
        self.audio_ring_heads[line] = (head + frames) % ring_len;
    }

    /// Delays a buffer in place by a fixed sample count for latency compensation.
    fn compensate(&mut self, buf: Buf, slot: usize, samples: usize, width: usize, win: Window) {
        if slot >= MAX_COMPENSATORS || samples == 0 || samples >= MAX_COMPENSATION {
            return;
        }
        let frames = win.frames;
        let mut head = self.compensator_heads[slot];
        for ch in 0..width.min(MAX_CHANNELS) {
            // Every channel walks the same distance, so each starts from the
            // same head and only the last one leaves it moved.
            head = self.compensator_heads[slot];
            let ring = slot * MAX_CHANNELS * MAX_COMPENSATION + ch * MAX_COMPENSATION;
            let signal = self.at(buf, ch, win);
            for i in 0..frames {
                let read = (head + MAX_COMPENSATION - samples) % MAX_COMPENSATION;
                let delayed = self.compensators[ring + read];
                self.compensators[ring + head] = self.pool[signal + i];
                self.pool[signal + i] = delayed;
                head = (head + 1) % MAX_COMPENSATION;
            }
        }
        self.compensator_heads[slot] = head;
    }

    /// Every stage's parameter and note ops for one sub-block.
    ///
    /// Overwrites slot table values for lanes driven by the graph.
    ///
    /// Paired with [`run_audio`][Engine::run_audio] this puts every parameter
    /// of the block before any of its audio, which is one block behind the
    /// order the stages describe. What that costs depends on the graph:
    ///
    /// * with nothing reading a parameter off audio there is only one stage
    ///   and the two orders are the same walk;
    /// * with an [`Op::Follow`] whose value never comes back round to audio —
    ///   a meter, or anything else the editor reads and the block does not —
    ///   the audio is identical and the lane is a block old, which is what a
    ///   meter is anyway;
    /// * with one that *does* reach audio again, through a sub-plugin's
    ///   parameter or a generated controller, the block is rendered against
    ///   the level of the block before it.
    ///
    /// The buffer read is the pool as the last block left it, so that last
    /// case is a block of latency rather than nonsense. A caller that wants
    /// none of it walks the stages itself; see [`Engine::stages`].
    pub fn run(&mut self, ctx: &BlockContext, slots: &mut [f64]) {
        for stage in 0..self.stages() {
            self.run_stage(stage, ctx, slots);
        }
    }

    /// One stage's parameter and note ops for one sub-block.
    ///
    /// Called once per sub-block, in order, before that stage's audio ops.
    /// What a parameter op reads out of a note buffer is everything the buffer
    /// holds, which is the stream up to the boundary this sub-block starts on.
    pub fn run_stage(&mut self, stage: usize, ctx: &BlockContext, slots: &mut [f64]) {
        // Moved out and put back rather than borrowed.
        let Some(program) = self.program.take() else {
            return;
        };
        let Some(&stage) = program.stages.get(stage) else {
            self.program = Some(program);
            return;
        };
        if program.is_empty() {
            self.program = Some(program);
            return;
        }

        let dt = if ctx.sample_rate > 0.0 {
            f64::from(ctx.frames) / ctx.sample_rate
        } else {
            0.0
        };

        // Parameter delay distance converted to sub-block taps.
        let taps_per_second = if ctx.frames > 0 && ctx.sample_rate > 0.0 {
            ctx.sample_rate / f64::from(ctx.frames)
        } else {
            0.0
        };

        for op in &program.ops[stage.params.range()] {
            match *op {
                Op::DelayRead {
                    out,
                    line,
                    time,
                    time_reg,
                } => {
                    // A wired time control overrides the static node setting.
                    let time = match time_reg {
                        Some(reg) => self.registers[reg as usize].max(0.0),
                        None => time,
                    };
                    let index = line as usize;
                    self.registers[out as usize] = if index < self.rings.len() {
                        let taps = (time * taps_per_second)
                            .round()
                            .clamp(1.0, (MAX_DELAY_TAPS - 1) as f64)
                            as usize;
                        let head = self.ring_heads[index];
                        let at = (head + MAX_DELAY_TAPS - taps) % MAX_DELAY_TAPS;
                        self.rings[index][at]
                    } else {
                        0.0
                    };
                }
                Op::DelayWrite { line, a } => {
                    let index = line as usize;
                    if index < self.rings.len() {
                        let head = self.ring_heads[index];
                        self.rings[index][head] = self.registers[a as usize];
                        self.ring_heads[index] = (head + 1) % MAX_DELAY_TAPS;
                    }
                }
                Op::Const { out, value } => self.registers[out as usize] = value,
                Op::Slot { out, slot } => {
                    self.registers[out as usize] = slots.get(slot as usize).copied().unwrap_or(0.0)
                }
                Op::Lfo {
                    out,
                    state,
                    waveform,
                    rate,
                    offset_phase,
                    depth,
                    centre,
                } => {
                    let i = state as usize;
                    let phase = (self.phases[i] + offset_phase).rem_euclid(1.0);
                    let shape = match waveform {
                        Waveform::Sine => (phase * std::f64::consts::TAU).sin(),
                        Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
                        Waveform::Saw => 2.0 * phase - 1.0,
                        Waveform::Square => {
                            if phase < 0.5 {
                                -1.0
                            } else {
                                1.0
                            }
                        }
                        Waveform::Random => self.holds[i],
                    };
                    self.registers[out as usize] = centre + depth * shape;

                    let hz = match rate {
                        RateSpec::Hz(hz) => hz,
                        RateSpec::CyclesPerBeat(cpb) => cpb * ctx.tempo_bpm / 60.0,
                    };
                    let advanced = self.phases[i] + hz * dt;
                    if advanced >= 1.0 && waveform == Waveform::Random {
                        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        self.holds[i] = f64::from(self.rng >> 8) / f64::from(1u32 << 23) - 1.0;
                    }
                    self.phases[i] = advanced.rem_euclid(1.0);
                }
                Op::Select {
                    out,
                    control,
                    threshold,
                    low,
                    high,
                } => {
                    let pick = if self.registers[control as usize] >= threshold {
                        high
                    } else {
                        low
                    };
                    self.registers[out as usize] = match pick {
                        Operand::Reg(reg) => self.registers[reg as usize],
                        Operand::Value(value) => value,
                    };
                }
                Op::KeyHeld { out, buf, key } => {
                    self.registers[out as usize] = f64::from(self.held(buf, key));
                }
                Op::Follow {
                    out,
                    buf,
                    state,
                    detect,
                    attack,
                    release,
                } => {
                    let win = Window {
                        block: ctx.block as usize,
                        start: ctx.offset as usize,
                        frames: ctx.frames as usize,
                    };
                    let width = program
                        .buffers
                        .get(buf as usize)
                        .map_or(0, |&w| (w as usize).min(MAX_CHANNELS));
                    let level = self.loudness(buf, width, win, detect);
                    // One pole per sub-block, which is as often as a parameter
                    // is allowed to move. `dt` is this sub-block's length, so
                    // the times mean the same thing at any quantum and any
                    // block size. A time of zero is a coefficient of zero,
                    // which is following exactly.
                    let held = self.latches.get(state as usize).copied().unwrap_or(0.0);
                    let held = if held.is_finite() { held } else { 0.0 };
                    let time = if level > held { attack } else { release };
                    let value = if time > 0.0 && dt > 0.0 {
                        let coeff = (-dt / time).exp();
                        held + (level - held) * (1.0 - coeff)
                    } else {
                        level
                    };
                    if let Some(latch) = self.latches.get_mut(state as usize) {
                        *latch = value;
                    }
                    self.registers[out as usize] = value;
                }
                Op::NoteFollow {
                    out,
                    buf,
                    state,
                    what,
                } => {
                    let index = buf as usize;
                    self.registers[out as usize] = match what {
                        Follow::Velocity => {
                            self.notes.bufs.get(index).map_or(0.0, |buf| buf.velocity)
                        }
                        Follow::KeyTrack => self.notes.bufs.get(index).map_or(0.5, |buf| buf.key),
                        Follow::Gate => f64::from(u8::from(
                            self.notes.bufs.get(index).is_some_and(|buf| buf.count > 0),
                        )),
                    };
                    // The latch is not read back — the tables above already
                    // survive a program swap — but keeping the value in it
                    // means the editor can show what the node is reading.
                    if let Some(latch) = self.latches.get_mut(state as usize) {
                        *latch = self.registers[out as usize];
                    }
                }
                Op::KeyStep {
                    state,
                    buf,
                    key,
                    count,
                } => {
                    if self.struck(buf, key)
                        && count > 0
                        && let Some(latch) = self.latches.get_mut(state as usize)
                    {
                        let at = if latch.is_nan() { 0.0 } else { *latch };
                        *latch = (at + 1.0).rem_euclid(f64::from(count));
                    }
                }
                Op::KeyLatch {
                    state,
                    buf,
                    key,
                    value,
                } => {
                    if self.struck(buf, key)
                        && let Some(latch) = self.latches.get_mut(state as usize)
                    {
                        *latch = value;
                    }
                }
                Op::LatchIs {
                    out,
                    state,
                    value,
                    initial,
                } => {
                    let at = self
                        .latches
                        .get(state as usize)
                        .copied()
                        .unwrap_or(f64::NAN);
                    let at = if at.is_nan() { initial } else { at };
                    self.registers[out as usize] = f64::from(at == value);
                }
                Op::NoteCc {
                    out,
                    buf,
                    state,
                    channel,
                    cc,
                    initial,
                } => {
                    // The last matching event wins: within one sub-block a
                    // controller may move several times, and what the boundary
                    // carries is where it ended up.
                    let latest = self
                        .notes
                        .bufs
                        .get(buf as usize)
                        .into_iter()
                        .flat_map(|buf| buf.events.iter())
                        .rev()
                        .find_map(|event| match *event {
                            Event::Note(NoteEvent::Cc {
                                channel: on,
                                cc: number,
                                value,
                                ..
                            }) if number == cc && (channel < 0 || channel == on) => Some(value),
                            _ => None,
                        });
                    let held = &mut self.latches[state as usize];
                    if let Some(value) = latest {
                        *held = value;
                    }
                    self.registers[out as usize] = if held.is_nan() { initial } else { *held };
                }
                Op::Latch {
                    out,
                    state,
                    initial,
                } => {
                    let value = self
                        .latches
                        .get(state as usize)
                        .copied()
                        .unwrap_or(f64::NAN);
                    self.registers[out as usize] = if value.is_nan() { initial } else { value };
                }
                Op::Math { out, a, b, op } => {
                    let a = self.registers[a as usize];
                    let b = match b {
                        Operand::Reg(reg) => self.registers[reg as usize],
                        Operand::Value(value) => value,
                    };
                    self.registers[out as usize] = match op {
                        MathOp::Add => a + b,
                        MathOp::Subtract => a - b,
                        MathOp::Multiply => a * b,
                        MathOp::Min => a.min(b),
                        MathOp::Max => a.max(b),
                        // Clamping the exponent to at least 0.01 prevents yielding Infinity,
                        // which can crash third-party plugins if fed to their parameters.
                        MathOp::Curve => a.clamp(0.0, 1.0).powf(b.clamp(0.01, 100.0)),
                    };
                }
                Op::Range {
                    out,
                    a,
                    in_lo,
                    in_span,
                    out_lo,
                    out_span,
                    clamp,
                } => {
                    let value = self.registers[a as usize];
                    let t = if in_span == 0.0 {
                        0.0
                    } else {
                        (value - in_lo) / in_span
                    };
                    let t = if clamp { t.clamp(0.0, 1.0) } else { t };
                    self.registers[out as usize] = out_lo + t * out_span;
                }
            }
        }

        for &(lane, reg) in &program.outputs {
            if let Some(target) = slots.get_mut(lane as usize) {
                // Host automation and parameter slots are normalized to 0..1, while audio lanes
                // carry physical units (decibels, seconds) without clamping.
                let value = self.registers[reg as usize];
                *target = if !value.is_finite() {
                    0.0
                } else if lane < program.audio_lane_base {
                    value.clamp(0.0, 1.0)
                } else {
                    value
                };
            }
        }

        // Last, so that a reader in *this* sub-block saw the previous one's
        // stream. A parameter signal has sub-block resolution, so the value it
        // wants is the one in effect at the boundary it just crossed, not one
        // from the middle of the sub-block about to start.
        //
        // The buffers are appended to rather than refilled, so where they
        // stand now is both what this row's ops must skip and where the audio
        // half will later find this row's events.
        let row = ctx.row as usize;
        let mut base = [0usize; MAX_NOTE_BUFS];
        for (slot, buf) in base.iter_mut().zip(self.notes.bufs.iter()) {
            *slot = buf.events.len();
        }
        // Only for the buffers this stage fills. A later stage passing over
        // the same rows would otherwise overwrite every mark with the length
        // the buffer finished at, and the audio half would read the whole
        // block as one row.
        if let Some(marks) = self.note_marks.get_mut(row) {
            for (buf, mark) in marks.iter_mut().enumerate() {
                if stage.note_bufs & (1 << buf) != 0 {
                    *mark = base[buf] as u32;
                }
            }
        }
        // Two disjoint fields, which is the whole reason the note half's
        // state is a type of its own: the pass wants the block's stream by
        // shared reference and everything it fills by exclusive one.
        self.notes.run_notes_step(
            &program,
            stage,
            &self.translated,
            ctx.offset,
            ctx.frames,
            slots,
            &base,
        );
        self.note_rows = self.note_rows.max(row + 1);

        self.program = Some(program);
    }

    /// How loud one window of an audio buffer is, across its channels.
    ///
    /// Linear amplitude, not decibels: a parameter lane is a plain number and
    /// the graph has arithmetic nodes for anyone who wants the log of it.
    fn loudness(&self, buf: Buf, width: usize, win: Window, detect: Detect) -> f64 {
        if width == 0 || win.frames == 0 {
            return 0.0;
        }
        let mut peak = 0.0f32;
        let mut sum = 0.0f64;
        for ch in 0..width {
            let at = self.at(buf, ch, win);
            for &sample in &self.pool[at..at + win.frames] {
                match detect {
                    Detect::Peak => peak = peak.max(sample.abs()),
                    Detect::Rms => sum += f64::from(sample) * f64::from(sample),
                }
            }
        }
        match detect {
            Detect::Peak => f64::from(peak),
            // Across every channel at once rather than per channel and
            // averaged: what is wanted is how loud the signal is, and a stereo
            // pair carrying the same thing twice is not twice as loud.
            Detect::Rms => (sum / (width * win.frames) as f64).sqrt(),
        }
    }

    /// Whether `key` is down on `buf`. Out of range is never down.
    fn held(&self, buf: u16, key: u8) -> bool {
        let table = self.notes.bufs.get(buf as usize).map_or(0, |buf| buf.held);
        key_bit(i16::from(key)).is_some_and(|bit| table & bit != 0)
    }

    /// Whether `key` was struck in the sub-block `buf` last carried.
    fn struck(&self, buf: u16, key: u8) -> bool {
        let table = self
            .notes
            .bufs
            .get(buf as usize)
            .map_or(0, |buf| buf.struck);
        key_bit(i16::from(key)).is_some_and(|bit| table & bit != 0)
    }
}

/// One key's bit in the held/struck tables, or `None` for a key outside the
/// MIDI range — which a malformed event can carry and a bit shift cannot.
fn key_bit(key: i16) -> Option<u128> {
    (0..128).contains(&key).then(|| 1u128 << key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::graph::{Graph, NodeId};
    use crate::ir::MathOp;
    use crate::nodes::{
        AudioIn, AudioOut, CcIn, Constant, DelayRead, DelayWrite, EnvelopeFollower, Gate, KeyParam,
        KeyParamMode, KeySplit, KeySwitch, KeySwitchMode, Lfo, Math, Mix, NodeKind, NoteFilter,
        NoteFollow, NoteGate, NoteMute, ParamPort, ParamToCc, Plugin, PluginPorts, Rate, SlotIn,
        Switch, linear_to_db,
    };
    use crate::port::PortType;

    const SLOTS: usize = 32;

    /// Helper creating a parameter sink plugin node.
    fn param_sink(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    params: vec![ParamPort {
                        id: 0,
                        name: "p".into(),
                    }],
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    /// The lane [`param_sink`]'s parameter is driven through.
    const SINK: usize = SLOTS;

    /// A lane row: the slot table, and the sink's lane after it.
    fn lanes() -> Vec<f64> {
        vec![0.0; SLOTS + 1]
    }

    fn ctx(frames: u32) -> BlockContext {
        BlockContext {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            frames,
            offset: 0,
            row: 0,
            block: frames,
        }
    }

    /// The rate the audio tests run at. Real rather than convenient, because
    /// the delay rings are sized in seconds and a fake rate would make a
    /// sensible `max_time` come out as four samples.
    const RATE: f64 = 48_000.0;

    /// Samples, as the seconds a delay node wants.
    fn seconds(samples: f64) -> f64 {
        samples / RATE
    }

    /// A context for a test with no automation in it.
    fn audio_ctx(frames: u32) -> AudioContext<'static> {
        AudioContext {
            frames,
            quantum: 32,
            sample_rate: RATE,
            lanes: &[],
            lanes_per_row: 0,
        }
    }

    fn load(engine: &mut Engine, graph: &Graph) {
        let handoff = Handoff::new();
        let mut program = compile(graph, SLOTS).unwrap();
        // What the wrapper's `publish_graph` does, and for the same reason: the
        // rings are allocated on this side and ride over with the program.
        program.size_rings(RATE, &[]);
        handoff.send(Box::new(program));
        assert!(engine.adopt(&handoff));
    }

    #[test]
    fn a_lane_the_graph_does_not_drive_keeps_the_daws_value() {
        let mut graph = Graph::new();
        let c = graph.add(NodeKind::Constant(Constant { value: 0.25 }), [0.0, 0.0]);
        let out = param_sink(&mut graph);
        graph.connect(c, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.9; SLOTS + 1];
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.25);
        assert_eq!(slots[1], 0.9, "an undriven slot is left alone");
    }

    #[test]
    fn the_daws_automation_can_be_read_shaped_and_written_back() {
        let mut graph = Graph::new();
        let input = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
        let half = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(input, 0, half, 0);
        graph.connect(half, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[3] = 0.8;
        engine.run(&ctx(32), &mut slots);
        assert!((slots[SINK] - 0.4).abs() < 1e-12);
    }

    /// The parameter half's switch: one value below the threshold, another at
    /// it and above.
    #[test]
    fn a_switch_picks_by_threshold() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::Switch(Switch {
                values: vec![0.2, 0.9],
                thresholds: vec![0.6],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(control, 0, switch, 0);
        graph.connect(switch, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 0.59;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.2);

        slots[1] = 0.6;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.9, "the threshold itself is on");
    }

    /// More than two rungs: the last threshold the control has passed is the
    /// one that wins, and below all of them the first value is what is read.
    #[test]
    fn a_switch_climbs_a_ladder_of_thresholds() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::Switch(Switch {
                values: vec![0.1, 0.4, 0.7],
                thresholds: vec![0.3, 0.8],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(control, 0, switch, 0);
        graph.connect(switch, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        for (control, expected) in [(0.0, 0.1), (0.29, 0.1), (0.3, 0.4), (0.79, 0.4), (0.8, 0.7)] {
            slots[1] = control;
            engine.run(&ctx(32), &mut slots);
            assert_eq!(slots[SINK], expected, "at {control}");
        }
    }

    /// Either side of a switch can be a signal rather than a number, which is
    /// what makes it a router as well as a chooser.
    #[test]
    fn a_switch_can_pick_between_two_signals() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let a = graph.add(NodeKind::SlotIn(SlotIn { slot: 2 }), [0.0, 0.0]);
        let b = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::Switch(Switch {
                values: vec![0.0, 1.0],
                thresholds: vec![0.5],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(control, 0, switch, 0);
        graph.connect(a, 0, switch, 1);
        graph.connect(b, 0, switch, 2);
        graph.connect(switch, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[2] = 0.25;
        slots[3] = 0.75;
        slots[1] = 0.0;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.25);

        slots[2] = 0.25;
        slots[3] = 0.75;
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.75);
    }

    #[test]
    fn an_lfo_sweeps_and_comes_back() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        let mut seen: Vec<f64> = Vec::new();
        // One second at 48 kHz in 32-sample sub-blocks: a whole cycle.
        for _ in 0..1500 {
            engine.run(&ctx(32), &mut slots);
            seen.push(slots[SINK]);
        }
        let lowest = seen.iter().cloned().fold(f64::INFINITY, f64::min);
        let highest = seen.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(lowest < 0.02, "a saw should reach the bottom, got {lowest}");
        assert!(highest > 0.98, "a saw should reach the top, got {highest}");
        assert!(seen.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn tempo_sync_follows_the_host() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                // One cycle per beat: at 120 bpm that is 2 Hz.
                rate: Rate::Beats(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        // Quarter of a beat at 120 bpm = 0.125 s = 6000 samples.
        let mut slots = lanes();
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 6000,
                offset: 0,
                row: 0,
                block: 6000,
            },
            &mut slots,
        );
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 1,
                offset: 0,
                row: 0,
                block: 1,
            },
            &mut slots,
        );
        assert!(
            (slots[SINK] - 0.25).abs() < 1e-3,
            "expected a quarter cycle, got {}",
            slots[SINK]
        );
    }

    /// Helper creating a parameter feedback loop test graph.
    fn feedback_graph(time: f64) -> (Graph, NodeId) {
        let mut graph = Graph::new();
        let seed = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::Param,
                max_time: 1.0,
                time,
            }),
            [0.0, 0.0],
        );
        // The loop: (input + what came back) * 0.5, written back to the line.
        let mixed = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 0.0],
        );
        let decayed = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 0.5,
            }),
            [0.0, 0.0],
        );
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::Param,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);

        graph.connect(seed, 0, mixed, 0);
        graph.connect(read, 0, mixed, 1);
        graph.connect(mixed, 0, decayed, 0);
        graph.connect(decayed, 0, write, 0);
        graph.connect(decayed, 0, out, 0);
        (graph, write)
    }

    #[test]
    fn a_delay_line_carries_a_value_round_the_loop() {
        // One sub-block of delay, so each run reads exactly what the previous
        // one wrote.
        let (graph, _) = feedback_graph(32.0 / 48_000.0);
        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        // (1 + 0) * 0.5
        assert!(
            (slots[SINK] - 0.5).abs() < 1e-9,
            "first pass: {}",
            slots[SINK]
        );

        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        // (1 + 0.5) * 0.5 — the 0.5 came back round.
        assert!(
            (slots[SINK] - 0.75).abs() < 1e-9,
            "second pass: {}",
            slots[SINK]
        );
    }

    /// Verifies that recompiling does not clear parameter delay line state.
    #[test]
    fn recompiling_does_not_empty_a_delay_line() {
        let (mut graph, _) = feedback_graph(32.0 / 48_000.0);
        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert!((slots[SINK] - 0.5).abs() < 1e-9);

        // An unrelated node appears, as it does on any edit.
        graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        load(&mut engine, &graph);

        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert!(
            (slots[SINK] - 0.75).abs() < 1e-9,
            "the line was emptied by the swap: {}",
            slots[SINK]
        );
    }

    /// Verifies that parameter delay times below one sub-block are clamped to the minimum floor.
    #[test]
    fn a_delay_shorter_than_a_sub_block_is_held_at_one() {
        let (graph, _) = feedback_graph(0.0);
        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert!(
            (slots[SINK] - 0.75).abs() < 1e-9,
            "a zero time should behave as one sub-block, not as zero: {}",
            slots[SINK]
        );
    }

    /// The DAW's buffer size is not the sub-block size, and the loop is defined
    /// in sub-blocks. Two runs of 32 must land where one run of 64 does not.
    #[test]
    fn the_loop_is_measured_in_sub_blocks_not_daw_blocks() {
        let (graph, _) = feedback_graph(2.0 * 32.0 / 48_000.0);
        let run = |frames: u32, passes: usize| {
            let mut engine = Engine::new();
            load(&mut engine, &graph);
            let mut slots = lanes();
            for _ in 0..passes {
                slots[1] = 1.0;
                engine.run(&ctx(frames), &mut slots);
            }
            slots[SINK]
        };
        // Same sub-block size, same answer, however the DAW hands us the block.
        assert!((run(32, 4) - run(32, 4)).abs() < 1e-12);
        // Two sub-blocks of delay: nothing has come back yet after two passes.
        assert!((run(32, 2) - 0.5).abs() < 1e-9);
        // By the third, it has.
        assert!(run(32, 3) > 0.5);
    }

    /// An instrument: one stereo output, and a notes port.
    fn note_plugin(graph: &mut Graph, instance: usize) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    /// Records the note stream each instance was handed, so the note half can
    /// be read off a run.
    #[derive(Default)]
    struct Heard(std::collections::BTreeMap<u32, Vec<Event>>);

    impl AudioInstances for Heard {
        fn process(
            &mut self,
            instance: u32,
            notes: &[Event],
            _input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
        ) {
            self.0.entry(instance).or_default().extend_from_slice(notes);
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
        }
    }

    /// The note id an event carries, for reading a recorded stream back.
    fn named(event: &Event) -> Option<i32> {
        match event {
            Event::Note(note) => note.note_id(),
            Event::Param(_) => None,
        }
    }

    fn note_on(key: i16, at: u32) -> Event {
        Event::Note(NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: at,
        })
    }

    fn note_off(key: i16, at: u32) -> Event {
        Event::Note(NoteEvent::NoteOff {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity: 0.0,
            sample_offset: at,
        })
    }

    /// Runs `graph` over one block with `events` on the DAW's note input, and
    /// returns what each instance heard.
    fn hear(graph: &Graph, events: &[Event], lanes: &[f64]) -> Heard {
        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, graph);
        let mut heard = Heard::default();
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut row = vec![0.0; width];
        row[..lanes.len().min(width)].copy_from_slice(&lanes[..lanes.len().min(width)]);
        // The wrapper's order: take the stream in, run the param half over the
        // row the note filters read, then the audio half.
        engine.begin_block(events);
        engine.run(&ctx(8), &mut row);
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 32,
                sample_rate: RATE,
                lanes: &row,
                lanes_per_row: width,
            },
            &[0.0; 2 * 8],
            &mut [0.0; 2 * 8],
            &mut heard,
        );
        heard
    }

    /// A synth wired to the MIDI input hears the DAW; the one next to it does
    /// not. This is the whole reason notes are routed rather than broadcast.
    #[test]
    fn only_the_wired_instrument_hears_the_daw() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let wired = note_plugin(&mut graph, 0);
        let idle = note_plugin(&mut graph, 1);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, wired, 0);
        graph.connect(wired, 0, mix, 0);
        graph.connect(idle, 0, mix, 2);
        graph.connect(mix, 0, out, 0);

        let heard = hear(&graph, &[note_on(60, 0)], &[]);
        assert_eq!(heard.0[&0].len(), 1);
        assert!(
            heard.0.get(&1).is_none_or(Vec::is_empty),
            "an unwired notes port means silence, not everything"
        );
    }

    /// A shut gate holds the note-ons back and lets the releases through, so a
    /// note that was sounding when it closed still gets its note-off. Blocking
    /// everything would leave a hung note behind whatever threw the gate.
    #[test]
    fn a_shut_note_gate_still_delivers_the_releases() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::NoteGate(NoteGate {
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, gate, 0);
        graph.connect(control, 0, gate, 1);
        graph.connect(gate, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let events = [note_on(60, 0), note_off(55, 1)];

        // Slot 0 is the gate's control, and it is read straight out of the
        // lane row the DAW's automation rides in.
        let open = hear(&graph, &events, &[1.0]);
        assert_eq!(open.0[&0].len(), 2, "open, everything passes");

        let shut = hear(&graph, &events, &[0.0]);
        let seen = &shut.0[&0];
        assert_eq!(seen.len(), 1, "only the release got through");
        assert!(matches!(
            seen[0],
            Event::Note(NoteEvent::NoteOff { key: 55, .. })
        ));
    }

    /// A key mute drops both halves of the keys it names — the note-on went
    /// too, so nothing is left waiting for a release — and leaves everything
    /// else, including events that have no key at all.
    #[test]
    fn a_key_mute_takes_both_halves_and_leaves_the_controllers() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let mute = graph.add(NodeKind::NoteMute(NoteMute { keys: vec![24] }), [0.0, 0.0]);
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, mute, 0);
        graph.connect(mute, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let sustain = Event::Note(NoteEvent::Cc {
            port: 0,
            channel: 0,
            cc: 64,
            value: 1.0,
            sample_offset: 2,
        });
        let heard = hear(
            &graph,
            &[note_on(24, 0), note_off(24, 1), sustain, note_on(60, 3)],
            &[],
        );
        let seen = &heard.0[&0];
        assert_eq!(seen.len(), 2, "both halves of key 24 are gone: {seen:?}");
        assert!(matches!(seen[0], Event::Note(NoteEvent::Cc { .. })));
        assert!(matches!(
            seen[1],
            Event::Note(NoteEvent::NoteOn { key: 60, .. })
        ));
    }

    /// A key split hands each band to its own instrument, and hands the pedal
    /// to both of them. Dividing the keys is the whole job; an event that has
    /// no key belongs to no band and so belongs to all of them.
    #[test]
    fn a_key_split_divides_the_keys_and_shares_what_has_none() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let split = graph.add(
            NodeKind::KeySplit(KeySplit { splits: vec![60] }),
            [0.0, 0.0],
        );
        let upper = note_plugin(&mut graph, 0);
        let lower = note_plugin(&mut graph, 1);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, split, 0);
        graph.connect(split, 0, upper, 0);
        graph.connect(split, 1, lower, 0);
        graph.connect(upper, 0, mix, 0);
        graph.connect(lower, 0, mix, 2);
        graph.connect(mix, 0, out, 0);

        let sustain = Event::Note(NoteEvent::Cc {
            port: 0,
            channel: 0,
            cc: 64,
            value: 1.0,
            sample_offset: 3,
        });
        // 60 is the split, and it belongs to the band it names — the lower one.
        let heard = hear(
            &graph,
            &[note_on(72, 0), note_on(60, 1), note_on(48, 2), sustain],
            &[],
        );

        let keys = |instance: u32| -> Vec<i16> {
            heard.0[&instance]
                .iter()
                .filter_map(|event| match event {
                    Event::Note(NoteEvent::NoteOn { key, .. }) => Some(*key),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(keys(0), vec![72], "the upper band takes 61 and above");
        assert_eq!(keys(1), vec![60, 48], "the lower band takes 60 and below");
        for instance in [0, 1] {
            assert!(
                heard.0[&instance]
                    .iter()
                    .any(|event| matches!(event, Event::Note(NoteEvent::Cc { cc: 64, .. }))),
                "the pedal reaches instance {instance}"
            );
        }
    }

    /// A MIDI filter narrows the stream by channel and by controller number,
    /// and judges each event only on what it actually has: a note has a
    /// channel but no controller number, so a CC list must not swallow it.
    #[test]
    fn a_midi_filter_narrows_by_channel_and_controller() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let filter = graph.add(
            NodeKind::NoteFilter(NoteFilter {
                channels: vec![0],
                channel_mode: crate::nodes::FilterMode::Keep,
                controllers: vec![64],
                controller_mode: crate::nodes::FilterMode::Keep,
            }),
            [0.0, 0.0],
        );
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, filter, 0);
        graph.connect(filter, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let cc = |number: u8, channel: i16, at: u32| {
            Event::Note(NoteEvent::Cc {
                port: 0,
                channel,
                cc: number,
                value: 1.0,
                sample_offset: at,
            })
        };
        let mut on_other_channel = note_on(60, 4);
        if let Event::Note(NoteEvent::NoteOn { channel, .. }) = &mut on_other_channel {
            *channel = 1;
        }

        let heard = hear(
            &graph,
            &[
                note_on(60, 0),   // channel 0: passes
                on_other_channel, // channel 1: dropped
                cc(64, 0, 1),     // the sustain pedal: passes
                cc(1, 0, 2),      // the mod wheel: dropped
                cc(64, 1, 3),     // right controller, wrong channel
            ],
            &[],
        );
        let seen = &heard.0[&0];
        assert_eq!(seen.len(), 2, "expected the note and the pedal: {seen:?}");
        assert!(matches!(
            seen[0],
            Event::Note(NoteEvent::NoteOn {
                key: 60,
                channel: 0,
                ..
            })
        ));
        assert!(matches!(seen[1], Event::Note(NoteEvent::Cc { cc: 64, .. })));
    }

    /// A parameter driving a controller: the value reaches the plugin as CC,
    /// and joins the stream that was already flowing rather than replacing it.
    #[test]
    fn a_parameter_reaches_the_plugin_as_a_controller() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let pedal = graph.add(
            NodeKind::ParamToCc(ParamToCc { channel: 0, cc: 64 }),
            [0.0, 0.0],
        );
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(control, 0, pedal, 0);
        graph.connect(notes, 0, pedal, 1);
        graph.connect(pedal, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let heard = hear(&graph, &[note_on(60, 2)], &[1.0]);
        let seen = &heard.0[&0];
        assert_eq!(seen.len(), 2, "the pedal and the note: {seen:?}");
        assert!(
            matches!(
                seen[0],
                Event::Note(NoteEvent::Cc { cc: 64, value, sample_offset: 0, .. }) if value == 1.0
            ),
            "the controller comes first, at the sub-block start, so the \
             buffer stays sorted: {seen:?}"
        );
        assert!(matches!(
            seen[1],
            Event::Note(NoteEvent::NoteOn { key: 60, .. })
        ));
    }

    /// An unchanged controller is not an event. Re-sending it every sub-block
    /// would fill a plugin's parameter queue with nothing and retrigger the
    /// smoothing on plugins that ramp towards each incoming point.
    #[test]
    fn a_controller_is_sent_once_until_it_moves() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let pedal = graph.add(
            NodeKind::ParamToCc(ParamToCc { channel: 0, cc: 64 }),
            [0.0, 0.0],
        );
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(control, 0, pedal, 0);
        graph.connect(pedal, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut heard = Heard::default();
        let block = |engine: &mut Engine, value: f64, heard: &mut Heard| {
            let mut row = vec![0.0; width];
            row[0] = value;
            engine.begin_block(&[]);
            engine.run(&ctx(8), &mut row);
            engine.run_audio(
                &AudioContext {
                    frames: 8,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &row,
                    lanes_per_row: width,
                },
                &[0.0; 2 * 8],
                &mut [0.0; 2 * 8],
                heard,
            );
        };

        block(&mut engine, 1.0, &mut heard);
        assert_eq!(heard.0[&0].len(), 1, "the first value is always news");
        block(&mut engine, 1.0, &mut heard);
        assert_eq!(heard.0[&0].len(), 1, "and holding it is not");
        block(&mut engine, 0.0, &mut heard);
        assert_eq!(heard.0[&0].len(), 2, "letting go is");
    }

    /// A generator follows its parameter at sub-block resolution even when the
    /// plugins are called once for the whole block.
    ///
    /// Those are separate questions and must not be conflated: reading one
    /// row's lane values and applying them to the whole chunk would make a
    /// `Param → CC` send one value per block however fast the parameter moved.
    /// The sub-plugin call rate is a cost decision; the resolution of what it
    /// is told is not.
    #[test]
    fn a_generator_follows_its_lane_inside_a_whole_block_chunk() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let pedal = graph.add(
            NodeKind::ParamToCc(ParamToCc { channel: 0, cc: 64 }),
            [0.0, 0.0],
        );
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(control, 0, pedal, 0);
        graph.connect(pedal, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[]);
        load(&mut engine, &graph);
        assert_eq!(
            engine.chunking(),
            Chunking::WholeBlock,
            "no feedback loop, so the plugin is called once for the block"
        );

        // Four sub-blocks of 16, with the control taking a different value in
        // each. One whole-block chunk covers all four.
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let values = [0.0, 0.25, 0.25, 1.0];
        let mut rows = vec![0.0; width * values.len()];
        for (index, &value) in values.iter().enumerate() {
            let row = &mut rows[index * width..(index + 1) * width];
            row[0] = value;
            engine.run(
                &BlockContext {
                    sample_rate: RATE,
                    tempo_bpm: 120.0,
                    frames: 16,
                    offset: index as u32 * 16,
                    row: index as u32,
                    block: 64,
                },
                row,
            );
        }

        let mut heard = Heard::default();
        engine.run_audio(
            &AudioContext {
                frames: 64,
                quantum: 16,
                sample_rate: RATE,
                lanes: &rows,
                lanes_per_row: width,
            },
            &[0.0; 2 * 64],
            &mut [0.0; 2 * 64],
            &mut heard,
        );

        let seen = &heard.0[&0];
        let sent: Vec<(u32, f64)> = seen
            .iter()
            .filter_map(|event| match *event {
                Event::Note(NoteEvent::Cc {
                    value,
                    sample_offset,
                    ..
                }) => Some((sample_offset, value)),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![(0, 0.0), (16, 0.25), (48, 1.0)],
            "one event per move, timed at the sub-block it happened in, and \
             nothing for the sub-block where it held: {seen:?}"
        );
    }

    /// A sub-plugin is handed the graph's own note id, not the DAW's — and
    /// gets one even when the DAW supplied none, which is the normal case for
    /// anything that arrived as raw MIDI.
    #[test]
    fn a_sub_plugin_is_handed_the_graphs_own_note_id() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        // Two overlapping notes on one key, neither carrying an id.
        // Substituting the key number for a missing one would give both the
        // same id and make them one note.
        let heard = hear(&graph, &[note_on(60, 0), note_on(60, 1)], &[]);
        let ids: Vec<Option<i32>> = heard.0[&0].iter().map(named).collect();
        assert!(
            ids.iter().all(Option::is_some),
            "every note is named: {ids:?}"
        );
        assert_ne!(ids[0], ids[1], "and the two are told apart");
    }

    /// The note-off names the note the note-on opened, however little the DAW
    /// said about it.
    #[test]
    fn a_note_off_reaches_the_plugin_naming_the_note_it_ends() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let heard = hear(&graph, &[note_on(60, 0), note_off(60, 1)], &[]);
        let seen = &heard.0[&0];
        assert_eq!(seen.len(), 2);
        assert_eq!(named(&seen[0]), named(&seen[1]));
    }

    /// Every branch gated shut: the note reaches nobody, and the DAW is still
    /// holding a voice for it. Telling it so at the end of the block is the
    /// honest answer, and the only one that lets the voice go.
    #[test]
    fn a_note_that_reaches_no_plugin_is_reported_ended() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::NoteGate(NoteGate {
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, gate, 0);
        graph.connect(control, 0, gate, 1);
        graph.connect(gate, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);

        // Shut, so the note-on is swallowed before it reaches the synth.
        let mut heard = Heard::default();
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut row = vec![0.0; width];
        engine.begin_block(&[note_on(60, 0)]);
        engine.run(&ctx(8), &mut row);
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 32,
                sample_rate: RATE,
                lanes: &row,
                lanes_per_row: width,
            },
            &[0.0; 2 * 8],
            &mut [0.0; 2 * 8],
            &mut heard,
        );
        assert!(
            heard.0.get(&0).is_none_or(Vec::is_empty),
            "the gate held it back"
        );

        let mut ended = Vec::with_capacity(8);
        engine.end_block(&[], &mut ended);
        assert_eq!(ended.len(), 1, "and the DAW is told the note is over");
        assert_eq!(ended[0].key, 60);
    }

    /// A note that did reach a plugin waits for the plugin to say it is done.
    #[test]
    fn a_delivered_note_is_reported_only_when_the_plugin_ends_it() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);

        let mut heard = Heard::default();
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let row = vec![0.0; width];
        engine.begin_block(&[note_on(60, 0)]);
        engine.run(&ctx(8), &mut vec![0.0; width]);
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 32,
                sample_rate: RATE,
                lanes: &row,
                lanes_per_row: width,
            },
            &[0.0; 2 * 8],
            &mut [0.0; 2 * 8],
            &mut heard,
        );
        let id = named(&heard.0[&0][0]).expect("the synth was told a name");

        let mut ended = Vec::with_capacity(8);
        engine.end_block(&[], &mut ended);
        assert!(ended.is_empty(), "the synth is still playing it");

        // What a CLAP sub-plugin sends when its voice finishes.
        engine.end_block(
            &[Event::Note(NoteEvent::NoteEnd {
                note_id: Some(id),
                port: 0,
                channel: 0,
                key: 60,
                sample_offset: 0,
            })],
            &mut ended,
        );
        assert_eq!(ended.len(), 1, "and now it is not");
    }

    /// Each sub-block gets its own events, once. Handing every chunk the whole
    /// block would replay every note once per chunk.
    #[test]
    fn a_note_lands_in_one_sub_block_only() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);
        let mut heard = Heard::default();
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        // A quantum of 4 cuts the block in two whatever the graph says, so the
        // same note must not appear in both halves.
        let mut row = vec![0.0; width * 2];
        engine.begin_block(&[note_on(60, 0), note_on(61, 5)]);
        for sub in 0..2 {
            engine.run(
                &BlockContext {
                    sample_rate: RATE,
                    tempo_bpm: 120.0,
                    frames: 4,
                    offset: sub as u32 * 4,
                    row: sub as u32,
                    block: 8,
                },
                &mut row[sub * width..(sub + 1) * width],
            );
        }
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 4,
                sample_rate: RATE,
                lanes: &row,
                lanes_per_row: width,
            },
            &[0.0; 2 * 8],
            &mut [0.0; 2 * 8],
            &mut heard,
        );
        assert_eq!(heard.0[&0].len(), 2, "each note once: {:?}", heard.0[&0]);
    }

    /// The last sub-block of a block is still readable at the start of the
    /// next one, and is not played twice.
    ///
    /// The buffers carry that tail across the block boundary so a parameter op
    /// reading at the first boundary of a block has the stream that was in
    /// force there — without it every controller would snap back to its
    /// starting value once per DAW block. The plugins were handed those events
    /// last block and must not be handed them again, which is the half of the
    /// arrangement nothing else would notice.
    #[test]
    fn the_boundary_a_block_starts_on_belongs_to_the_block_before() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = note_plugin(&mut graph, 0);
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut heard = Heard::default();
        let block = |engine: &mut Engine, events: &[Event], heard: &mut Heard| {
            let mut row = vec![0.0; width];
            engine.begin_block(events);
            engine.run(&ctx(8), &mut row);
            engine.run_audio(
                &AudioContext {
                    frames: 8,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &row,
                    lanes_per_row: width,
                },
                &[0.0; 2 * 8],
                &mut [0.0; 2 * 8],
                heard,
            );
        };

        block(&mut engine, &[note_on(60, 0)], &mut heard);
        assert_eq!(heard.0[&0].len(), 1, "the synth is played the note");
        block(&mut engine, &[], &mut heard);
        assert_eq!(
            heard.0[&0].len(),
            1,
            "and not played it again: {:?}",
            heard.0[&0]
        );
    }

    /// A `CC In` reading the DAW's mod wheel into a parameter lane, one
    /// sub-block behind the events — which is what a parameter signal's
    /// resolution means, not a delay anybody chose.
    #[test]
    fn a_controller_becomes_a_parameter_at_the_next_boundary() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let wheel = graph.add(NodeKind::CcIn(CcIn::default()), [0.0, 0.0]);
        let sink = param_sink(&mut graph);
        graph.connect(notes, 0, wheel, 0);
        graph.connect(wheel, 0, sink, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[]);
        load(&mut engine, &graph);

        let wheel_at = |value: f64, at: u32| {
            Event::Note(NoteEvent::Cc {
                port: 0,
                channel: 0,
                cc: 1,
                value,
                sample_offset: at,
            })
        };
        // Two sub-blocks of 8 frames each, with the wheel moving inside the
        // first one.
        let events = [wheel_at(1.0, 3)];
        let mut rows = vec![0.0; (SLOTS + 1) * 2];
        let width = SLOTS + 1;

        engine.begin_block(&events);
        for index in 0..2u32 {
            let (head, tail) = rows.split_at_mut(index as usize * width);
            let row = if index == 0 {
                &mut head[..]
            } else {
                &mut tail[..width]
            };
            engine.run(
                &BlockContext {
                    sample_rate: RATE,
                    tempo_bpm: 120.0,
                    frames: 8,
                    offset: index * 8,
                    row: index,
                    block: 16,
                },
                row,
            );
        }

        assert_eq!(
            rows[SINK], 0.0,
            "the first sub-block carries the value in effect at its start"
        );
        assert_eq!(
            rows[width + SINK],
            1.0,
            "the second carries where the wheel ended up in the first"
        );
    }

    /// The stream a `CC In` reads is the routed one, so a filter upstream of it
    /// changes what it sees. That is the whole reason it takes a note input
    /// instead of reading whatever the DAW sent.
    #[test]
    fn a_filter_upstream_of_a_cc_in_changes_what_it_reads() {
        let build = |keep: u8| {
            let mut graph = Graph::new();
            let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
            let filter = graph.add(
                NodeKind::NoteFilter(NoteFilter {
                    controllers: vec![keep],
                    controller_mode: crate::nodes::FilterMode::Keep,
                    ..NoteFilter::default()
                }),
                [0.0, 0.0],
            );
            let wheel = graph.add(NodeKind::CcIn(CcIn::default()), [0.0, 0.0]);
            let sink = param_sink(&mut graph);
            graph.connect(notes, 0, filter, 0);
            graph.connect(filter, 0, wheel, 0);
            graph.connect(wheel, 0, sink, 0);
            graph
        };

        let events = [Event::Note(NoteEvent::Cc {
            port: 0,
            channel: 0,
            cc: 1,
            value: 1.0,
            sample_offset: 0,
        })];
        let read = |graph: &Graph| {
            let mut engine = Engine::new();
            engine.prepare(64, &[]);
            load(&mut engine, graph);
            let mut row = vec![0.0; SLOTS + 1];
            engine.begin_block(&events);
            // Twice: the first fills the buffers, the second reads them.
            for index in 0..2u32 {
                engine.run(
                    &BlockContext {
                        sample_rate: RATE,
                        tempo_bpm: 120.0,
                        frames: 8,
                        offset: index * 8,
                        row: index,
                        block: 16,
                    },
                    &mut row,
                );
            }
            row[SINK]
        };

        assert_eq!(read(&build(1)), 1.0, "CC1 is kept, so the wheel is read");
        assert_eq!(
            read(&build(64)),
            0.0,
            "keeping only CC64 takes the wheel out before it gets here"
        );
    }

    /// A controller keeps its position between messages. A block with no CC in
    /// it must hold the last value, not snap back to the starting one.
    #[test]
    fn a_controller_holds_its_position() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let wheel = graph.add(NodeKind::CcIn(CcIn::default()), [0.0, 0.0]);
        let sink = param_sink(&mut graph);
        graph.connect(notes, 0, wheel, 0);
        graph.connect(wheel, 0, sink, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[]);
        load(&mut engine, &graph);

        let moved = [Event::Note(NoteEvent::Cc {
            port: 0,
            channel: 0,
            cc: 1,
            value: 0.75,
            sample_offset: 0,
        })];
        let mut row = vec![0.0; SLOTS + 1];
        let block = |engine: &mut Engine, events: &[Event], row: &mut Vec<f64>| {
            engine.begin_block(events);
            engine.run(
                &BlockContext {
                    sample_rate: RATE,
                    tempo_bpm: 120.0,
                    frames: 8,
                    offset: 0,
                    row: 0,
                    block: 8,
                },
                row,
            );
        };

        block(&mut engine, &moved, &mut row);
        block(&mut engine, &[], &mut row);
        assert_eq!(row[SINK], 0.75, "the wheel moved and the value took");
        block(&mut engine, &[], &mut row);
        assert_eq!(row[SINK], 0.75, "and stayed where it was left");
    }

    /// A stand-in for the sub-plugins, so the engine's routing can be tested
    /// without one. Each instance adds its own number to every sample, which
    /// makes the order it ran in readable off the output.
    struct Adders;

    impl AudioInstances for Adders {
        fn process(
            &mut self,
            instance: u32,
            _notes: &[Event],
            input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
        ) {
            for ch in 0..chunk.output_channels {
                let range = chunk.channel(ch);
                for (o, i) in output[range.clone()].iter_mut().zip(input[range].iter()) {
                    *o = *i + (instance + 1) as f32;
                }
            }
        }
    }

    fn audio_plugin(graph: &mut Graph, instance: usize, latency: u32) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    latency,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    #[test]
    fn audio_runs_through_two_plugins_in_order() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let first = audio_plugin(&mut graph, 0, 0);
        let second = audio_plugin(&mut graph, 1, 0);
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, first, 0);
        graph.connect(first, 0, second, 0);
        graph.connect(second, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![10.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);

        // 10, then +1 from instance 0, then +2 from instance 1.
        assert!(
            daw_out.iter().all(|&s| (s - 13.0).abs() < 1e-6),
            "{daw_out:?}"
        );
    }

    /// Verifies latency compensation delay aligns parallel audio branches.
    #[test]
    fn a_compensated_branch_arrives_with_the_late_one() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        // Latency 4, but the stand-in does not actually delay: what is being
        // tested is that the *other* branch is delayed by the same 4.
        let slow = audio_plugin(&mut graph, 0, 4);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                // Empty is unity: what a mix did before it had gains.
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, slow, 0);
        graph.connect(slow, 0, mix, 0);
        graph.connect(input, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        // An impulse on the first sample of each channel.
        let mut daw_in = vec![0.0f32; 2 * 8];
        daw_in[0] = 1.0;
        daw_in[8] = 1.0;
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);

        // The wet branch is the stand-in: input + 1, so the impulse shows as
        // 2.0 at sample 0 and 1.0 everywhere else. The dry branch is held back
        // 4 samples, so its impulse lands at sample 4 and nowhere else.
        assert!((daw_out[0] - 2.0).abs() < 1e-6, "wet at 0: {}", daw_out[0]);
        assert!(
            (daw_out[4] - 2.0).abs() < 1e-6,
            "dry arrives at 4: {}",
            daw_out[4]
        );
        assert!(
            (daw_out[1] - 1.0).abs() < 1e-6,
            "quiet between: {}",
            daw_out[1]
        );
    }

    /// The patch a new instance starts with has to be the through-connection
    /// the wrapper used to make for itself, or every fresh instance is silent.
    #[test]
    fn the_default_patch_passes_audio_through() {
        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &Graph::default_patch());

        let daw_in: Vec<f32> = (0..2 * 8).map(|i| i as f32 * 0.1).collect();
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert_eq!(daw_out, daw_in);
    }

    /// An unconnected `AudioOut` emits no `Output` op, so `run_audio` clears
    /// the block itself.
    #[test]
    fn an_unconnected_output_clears_the_daw_buffer() {
        let mut graph = Graph::new();
        graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![0.0f32; 2 * 8];
        let mut daw_out = vec![7.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(daw_out.iter().all(|&s| s == 0.0), "{daw_out:?}");
    }

    /// A channel no op reaches is silence too.
    #[test]
    fn a_channel_no_op_writes_is_cleared() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 1,
            }),
            [0.0, 0.0],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 1,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, output, 0);
        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![1.0f32; 2 * 8];
        let mut daw_out = vec![7.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(daw_out[8..].iter().all(|&s| s == 0.0), "{daw_out:?}");
    }

    /// A line whose writer loses its input keeps its ring across the swap, so
    /// the write head has to go on advancing for the line to drain.
    #[test]
    fn a_delay_line_nothing_writes_drains_to_silence() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        // A tap well inside a block, so a handful of blocks fills the ring.
        let (write, read) = audio_delay(&mut graph, 100.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        // Load the line.
        let daw_in = vec![1.0f32; 2 * 128];
        let mut daw_out = vec![0.0f32; 2 * 128];
        for _ in 0..8 {
            engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
        }
        assert!(
            daw_out.iter().any(|s| s.abs() > 0.5),
            "the line is loaded: {:?}",
            &daw_out[..8]
        );

        // Pull the wire out of the write node and let the line run dry.
        graph.disconnect(write, 0);
        load(&mut engine, &graph);
        let silence = vec![0.0f32; 2 * 128];
        for _ in 0..32 {
            engine.run_audio(&audio_ctx(128), &silence, &mut daw_out, &mut Adders);
        }
        assert!(
            daw_out.iter().all(|s| s.abs() < 1e-6),
            "the ring should have drained: {:?}",
            &daw_out[..8]
        );
    }

    /// `prepare` is the only thing that allocates, so running without it — or
    /// with a longer block than promised — has to be a no-op rather than a
    /// panic or a read past the end.
    #[test]
    fn running_audio_unprepared_does_nothing() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, output, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let daw_in = vec![1.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(daw_out.iter().all(|&s| s == 0.0));

        engine.prepare(4, &[2]);
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(
            daw_out.iter().all(|&s| s == 0.0),
            "8 frames were promised 4"
        );
    }

    #[test]
    fn recompiling_does_not_restart_a_running_lfo() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        for _ in 0..200 {
            engine.run(&ctx(32), &mut slots);
        }
        let before = slots[SINK];
        assert!(before > 0.05, "the LFO should have moved by now");

        // Something unrelated changes — a new node appears — and the graph is
        // recompiled, as it is on every edit.
        graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        load(&mut engine, &graph);
        engine.run(&ctx(1), &mut slots);

        assert!(
            (slots[SINK] - before).abs() < 0.01,
            "the phase jumped across a recompile: {before} -> {}",
            slots[SINK]
        );
    }

    /// Plays notes into the graph's MIDI input the way the wrapper does.
    ///
    /// Notes reach a parameter op one sub-block after they arrive — the note
    /// half fills the buffers at the end of each sub-block's evaluation, so a
    /// reader sees the stream in effect at the boundary it just crossed. That
    /// is one sub-block of setup in every test that plays something, which
    /// says nothing about what the test is checking, so it lives here.
    #[derive(Default)]
    struct Keyboard {
        pending: Vec<Event>,
    }

    impl Keyboard {
        fn note(&mut self, event: &NoteEvent) {
            self.pending.push(Event::Note(*event));
        }

        /// Evaluate over the two sub-blocks it takes for what has been played
        /// to reach the parameter half.
        fn run(&mut self, engine: &mut Engine, frames: u32, slots: &mut [f64]) {
            engine.begin_block(&self.pending);
            for (row, offset) in [0, frames].into_iter().enumerate() {
                engine.run(
                    &BlockContext {
                        sample_rate: 48_000.0,
                        tempo_bpm: 120.0,
                        frames,
                        offset,
                        row: row as u32,
                        block: frames * 2,
                    },
                    slots,
                );
            }
            self.pending.clear();
        }
    }

    /// A key switch watches one key, whatever has been played since — which
    /// is exactly what a follow of the newest note cannot answer.
    #[test]
    fn a_held_key_switch_follows_its_own_key() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::KeySwitch(KeySwitch {
                keys: vec![24],
                mode: KeySwitchMode::Hold,
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let synth = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, switch, 0);
        graph.connect(switch, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .note_ops
            .iter()
            .find_map(|op| match op {
                crate::ir::NoteOp::Filter { gate, .. } => *gate,
                _ => None,
            })
            .expect("the key switch booked a gate lane") as usize;

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut lanes = vec![0.0; width];
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 0.0, "nothing is held yet");

        keys.note(&NoteEvent::NoteOn {
            note_id: Some(1),
            port: 0,
            channel: 0,
            key: 24,
            velocity: 1.0,
            sample_offset: 0,
        });
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 1.0, "the switch key is down");

        // A different key coming and going must not move it.
        keys.note(&NoteEvent::NoteOn {
            note_id: Some(2),
            port: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
            sample_offset: 0,
        });
        keys.note(&NoteEvent::NoteOff {
            note_id: Some(2),
            port: 0,
            channel: 0,
            key: 60,
            velocity: 0.0,
            sample_offset: 0,
        });
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 1.0, "another key came and went");

        keys.note(&NoteEvent::NoteOff {
            note_id: Some(1),
            port: 0,
            channel: 0,
            key: 24,
            velocity: 0.0,
            sample_offset: 0,
        });
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 0.0, "let go");
    }

    /// A toggling switch moves on each strike and stays where it was put —
    /// including across the recompile that every edit causes.
    #[test]
    fn a_toggling_key_switch_latches_and_survives_a_recompile() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::KeySwitch(KeySwitch {
                keys: vec![24, 25],
                mode: KeySwitchMode::Toggle,
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let synth = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, switch, 0);
        graph.connect(switch, 1, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();

        let lane = compile(&graph, SLOTS)
            .unwrap()
            .note_ops
            .iter()
            .find_map(|op| match op {
                crate::ir::NoteOp::Filter { gate, .. } => *gate,
                _ => None,
            })
            .expect("output b got a gate lane") as usize;

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut lanes = vec![0.0; width];
        let strike = NoteEvent::NoteOn {
            note_id: Some(1),
            port: 0,
            channel: 0,
            key: 24,
            velocity: 1.0,
            sample_offset: 0,
        };

        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 0.0, "b is shut until the switch is thrown");

        keys.note(&strike);
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 1.0, "thrown");
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 1.0, "and it stays thrown");

        // An unrelated edit, and the recompile it causes.
        graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 1.0, "a recompile must not move the switch");

        keys.note(&strike);
        keys.run(&mut engine, 8, &mut lanes);
        assert_eq!(lanes[lane], 0.0, "thrown back");
    }

    /// One key stepping a parameter through its values, and staying where it
    /// was put. With two values that is a plain toggle.
    #[test]
    fn a_key_parameter_toggles_between_two_values() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Toggle,
                keys: vec![24, 25],
                values: vec![0.2, 0.8],
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, key, 0);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();
        let mut slots = lanes();
        let strike = NoteEvent::NoteOn {
            note_id: Some(1),
            port: 0,
            channel: 0,
            key: 24,
            velocity: 1.0,
            sample_offset: 0,
        };

        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.2, "untouched, it reads its first value");

        keys.note(&strike);
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.8);
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.8, "one strike is one step, not one per run");

        keys.note(&strike);
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.2, "and round again");
    }

    /// A bank of keys, one value each: the last one struck wins, which is what
    /// a row of switches does.
    #[test]
    fn a_key_parameter_selects_by_the_last_key_struck() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25, 26],
                values: vec![0.25, 0.5, 1.0],
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, key, 0);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();
        let mut slots = lanes();
        let strike = |key: i16| NoteEvent::NoteOn {
            note_id: Some(key as i32),
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: 0,
        };

        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.25);

        keys.note(&strike(25));
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.5);

        keys.note(&strike(26));
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 1.0);

        // A key the bank does not name changes nothing.
        keys.note(&strike(60));
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 1.0);
    }

    /// Nothing wired to the notes port means no keys are read at all. An
    /// unwired node that quietly followed the keyboard anyway would be a node
    /// whose links say nothing about what it does.
    #[test]
    fn a_key_parameter_with_no_notes_wired_stays_put() {
        let mut graph = Graph::new();
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.25, 0.75],
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();
        let mut slots = lanes();

        keys.note(&NoteEvent::NoteOn {
            note_id: Some(1),
            port: 0,
            channel: 0,
            key: 25,
            velocity: 1.0,
            sample_offset: 0,
        });
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.25);
    }

    /// A value socket wins over the number on its row, the same way `Math`'s
    /// `b` gives way to its input — so a key switch can pick between two
    /// signals, not only two numbers.
    #[test]
    fn a_key_parameter_value_can_be_a_signal() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let signal = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.25, 0.75],
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, key, 0);
        // Socket 0 is the notes port, so value 2 is socket 2.
        graph.connect(signal, 0, key, 2);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();
        let mut slots = lanes();
        slots[3] = 0.6;
        keys.note(&NoteEvent::NoteOn {
            note_id: Some(1),
            port: 0,
            channel: 0,
            key: 25,
            velocity: 1.0,
            sample_offset: 0,
        });
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.6, "the wired socket wins over the number");
    }

    #[test]
    fn the_gate_follows_held_notes() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::NoteFollow(NoteFollow { what: Follow::Gate }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, gate, 0);
        graph.connect(gate, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut keys = Keyboard::default();
        let mut slots = lanes();

        let on = |key: i16| NoteEvent::NoteOn {
            note_id: Some(key as i32),
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: 0,
        };
        let off = |key: i16| NoteEvent::NoteOff {
            note_id: Some(key as i32),
            port: 0,
            channel: 0,
            key,
            velocity: 0.0,
            sample_offset: 0,
        };

        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.0);

        keys.note(&on(60));
        keys.note(&on(64));
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 1.0);

        // Releasing one of two held notes must not drop the gate.
        keys.note(&off(60));
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 1.0);

        keys.note(&off(64));
        keys.run(&mut engine, 32, &mut slots);
        assert_eq!(slots[SINK], 0.0);
    }

    #[test]
    fn a_degenerate_graph_cannot_hand_a_nan_to_the_sub_plugin() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        let div = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Curve,
                b: 0.0,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(a, 0, div, 0);
        graph.connect(div, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = vec![0.5; SLOTS + 1];
        engine.run(&ctx(32), &mut slots);
        assert!(slots[SINK].is_finite());
        assert!((0.0..=1.0).contains(&slots[SINK]));
    }

    #[test]
    fn an_engine_with_no_program_leaves_everything_alone() {
        let mut engine = Engine::new();
        let mut slots = vec![0.3; SLOTS];
        engine.run(&ctx(32), &mut slots);
        assert!(slots.iter().all(|&v| v == 0.3));
        assert!(!engine.has_program());
    }

    /// A parameter read off audio, which only a program cut into stages can
    /// express.
    ///
    /// And read without latency: the follower's stage runs after the stage
    /// that made the sound, and that stage covered the whole block, so the
    /// window read for a sub-block is that sub-block's own. The first row
    /// already carries the level of the first row's audio.
    #[test]
    fn a_parameter_is_read_off_audio_in_the_sub_block_it_belongs_to() {
        const BLOCK: u32 = 64;
        const QUANTUM: u32 = 32;
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;

        let level = |detect: Detect, attack: f64, quiet_first: bool| -> [f64; 2] {
            let mut graph = Graph::new();
            let input = stereo_in(&mut graph);
            let follower = graph.add(
                NodeKind::EnvelopeFollower(EnvelopeFollower {
                    detect,
                    attack,
                    release: 0.0,
                }),
                [0.0, 0.0],
            );
            let sink = param_sink(&mut graph);
            graph.connect(input, 0, follower, 0);
            graph.connect(follower, 0, sink, 0);

            let mut engine = Engine::new();
            engine.prepare(BLOCK, &[2]);
            load(&mut engine, &graph);

            // Half scale, on the second sub-block only when asked: that is
            // what tells a reading of *this* row from a reading of the last.
            let mut daw_in = vec![0.5f32; 2 * BLOCK as usize];
            if quiet_first {
                for ch in 0..2usize {
                    let at = ch * BLOCK as usize;
                    daw_in[at..at + QUANTUM as usize].fill(0.0);
                }
            }
            let mut daw_out = vec![0.0f32; 2 * BLOCK as usize];
            let mut lanes = vec![0.0; width * 2];

            engine.begin_block(&[]);
            for stage in 0..engine.stages() {
                for row in 0..2usize {
                    let context = BlockContext {
                        sample_rate: RATE,
                        tempo_bpm: 120.0,
                        frames: QUANTUM,
                        offset: row as u32 * QUANTUM,
                        row: row as u32,
                        block: BLOCK,
                    };
                    engine.run_stage(stage, &context, &mut lanes[row * width..(row + 1) * width]);
                }
                engine.run_audio_stage(
                    stage,
                    &AudioContext {
                        frames: BLOCK,
                        quantum: QUANTUM,
                        sample_rate: RATE,
                        lanes: &lanes,
                        lanes_per_row: width,
                    },
                    &daw_in,
                    &mut daw_out,
                    &mut Adders,
                );
            }
            [lanes[SINK], lanes[width + SINK]]
        };

        let steady = level(Detect::Peak, 0.0, false);
        assert!(
            (steady[0] - 0.5).abs() < 1e-6 && (steady[1] - 0.5).abs() < 1e-6,
            "the peak of a half-scale signal is a half: {steady:?}"
        );

        // The one that would fail if the reading were a sub-block behind.
        let late = level(Detect::Peak, 0.0, true);
        assert!(
            late[0] < 1e-6,
            "the first sub-block was silent, so its level is nothing: {late:?}"
        );
        assert!(
            (late[1] - 0.5).abs() < 1e-6,
            "and the second is read in the sub-block it belongs to: {late:?}"
        );

        // RMS of a constant is that constant; what separates them is a
        // transient, which the peak catches and the mean does not.
        let mean = level(Detect::Rms, 0.0, true);
        assert!(
            (mean[1] - 0.5).abs() < 1e-6,
            "the RMS of a steady half is a half: {mean:?}"
        );

        // An attack time holds the rise back, and never past the level.
        let slow = level(Detect::Peak, 0.050, false);
        assert!(
            slow[0] > 0.0 && slow[0] < 0.5 && slow[1] > slow[0] && slow[1] < 0.5,
            "an attack of 50 ms climbs towards the level over sub-blocks: {slow:?}"
        );
    }

    /// What the all-stages helpers cost, which is nothing until a level comes
    /// back round to audio.
    ///
    /// `run` + `run_audio` put every parameter of the block before any of its
    /// audio, which is the order every caller with no audio to interleave
    /// wants. The claim under test is where that parts company with the order
    /// the stages describe: not wherever a follower appears, but only where
    /// its value reaches audio again inside the same block.
    #[test]
    fn the_all_stages_helpers_differ_only_where_a_level_reaches_audio() {
        const BLOCK: u32 = 64;
        const QUANTUM: u32 = 32;
        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;

        // A ramp, so every block is louder than the one before it and a block
        // of latency cannot hide in a steady signal.
        let played = |staged: bool, ducked: bool| -> Vec<f32> {
            let mut graph = Graph::new();
            let input = stereo_in(&mut graph);
            let output = stereo_out(&mut graph);
            let follower = graph.add(
                NodeKind::EnvelopeFollower(EnvelopeFollower {
                    detect: Detect::Peak,
                    attack: 0.0,
                    release: 0.0,
                }),
                [0.0, 0.0],
            );
            graph.connect(input, 0, follower, 0);
            let mix = graph.add(
                NodeKind::Mix(Mix {
                    channels: 2,
                    inputs: 1,
                    gains: vec![0.0],
                }),
                [0.0, 0.0],
            );
            graph.connect(input, 0, mix, 0);
            graph.connect(mix, 0, output, 0);
            if ducked {
                // The level drives the gain, so it reaches audio.
                graph.connect(follower, 0, mix, 1);
            } else {
                // The level goes to a sub-plugin parameter on an instance the
                // program has no audio for: read by the editor, and by nothing
                // this block renders.
                let sink = param_sink(&mut graph);
                graph.connect(follower, 0, sink, 0);
            }

            let mut engine = Engine::new();
            engine.prepare(BLOCK, &[2]);
            load(&mut engine, &graph);

            let mut heard = Vec::new();
            let mut lanes = vec![0.0; width * 2];
            for block in 0..4u32 {
                let value = 0.1 * (block + 1) as f32;
                let daw_in = vec![value; 2 * BLOCK as usize];
                let mut daw_out = vec![0.0f32; 2 * BLOCK as usize];
                let context = |row: usize| BlockContext {
                    sample_rate: RATE,
                    tempo_bpm: 120.0,
                    frames: QUANTUM,
                    offset: row as u32 * QUANTUM,
                    row: row as u32,
                    block: BLOCK,
                };
                let audio = AudioContext {
                    frames: BLOCK,
                    quantum: QUANTUM,
                    sample_rate: RATE,
                    lanes: &[],
                    lanes_per_row: width,
                };
                engine.begin_block(&[]);
                if staged {
                    for stage in 0..engine.stages() {
                        for row in 0..2usize {
                            engine.run_stage(
                                stage,
                                &context(row),
                                &mut lanes[row * width..(row + 1) * width],
                            );
                        }
                        engine.run_audio_stage(
                            stage,
                            &AudioContext {
                                lanes: &lanes,
                                ..audio
                            },
                            &daw_in,
                            &mut daw_out,
                            &mut Adders,
                        );
                    }
                } else {
                    for row in 0..2usize {
                        engine.run(&context(row), &mut lanes[row * width..(row + 1) * width]);
                    }
                    engine.run_audio(
                        &AudioContext {
                            lanes: &lanes,
                            ..audio
                        },
                        &daw_in,
                        &mut daw_out,
                        &mut Adders,
                    );
                }
                // The gain that came out, not the sample: what lags is the
                // level read, and the signal it is applied to is this block's
                // either way, so the two are not shifted copies of each other.
                heard.push(daw_out[BLOCK as usize - 1] / value);
            }
            heard
        };

        // The level reaches nothing this block renders, so the two orders make
        // the same sound and only the lane is a block old.
        assert_eq!(
            played(true, false),
            played(false, false),
            "a level nothing plays costs nothing to read late"
        );

        // And where it does reach audio, the batched order renders each block
        // against the level of the one before it.
        let staged = played(true, true);
        let batched = played(false, true);
        assert_ne!(staged, batched, "a gain read late is a different sound");
        assert!(
            batched
                .iter()
                .skip(1)
                .zip(&staged)
                .all(|(late, then)| (late - then).abs() < 1e-6),
            "and late by exactly one block: {batched:?} against {staged:?}"
        );
    }

    /// A second delay somewhere else in the patch does not change what the
    /// first one does.
    ///
    /// Both loops run in the same stage, and a `DelayWrite` gives its source
    /// buffer up as soon as it is *compiled*, while what it writes runs at the
    /// end of the stage. Anything compiled in between can be handed that
    /// buffer and fill it with something else, and the line then carries the
    /// other loop's signal instead of its own.
    ///
    /// Three things have to line up for it: the write has to be the buffer's
    /// last reader — a tap on the
    /// delayed side rather than on the sum, which is an ordinary way to wire a
    /// delay; the other loop has to be compiled after it; and it has to hold a
    /// node that asks the pool for a buffer rather than accumulating into one
    /// it already has, which a mix does and a plugin does not.
    #[test]
    fn a_second_delay_does_not_reach_into_the_first() {
        // in ──> mix in 1 ──> write(0)       the sum goes only to the line
        //        read(0) ─┬─> mix in 2
        //                 └─> out            and the tap is on the delayed side
        let patch = |second: bool| -> Vec<f32> {
            let mut graph = Graph::new();
            let input = stereo_in(&mut graph);
            let output = stereo_out(&mut graph);
            let (write, read) = audio_delay(&mut graph, 32.0);
            let mix = graph.add(
                NodeKind::Mix(Mix {
                    channels: 2,
                    inputs: 2,
                    gains: vec![0.0, linear_to_db(0.5)],
                }),
                [0.0, 0.0],
            );
            graph.connect(input, 0, mix, 0);
            graph.connect(read, 0, mix, 2);
            graph.connect(mix, 0, write, 0);
            graph.connect(read, 0, output, 0);

            if second {
                // A whole second loop, wired to nothing that is heard. It
                // exists to be compiled after the first one's write and to ask
                // the pool for a buffer while doing it.
                let other = graph.add(
                    NodeKind::DelayWrite(DelayWrite {
                        line: 1,
                        ty: PortType::STEREO,
                    }),
                    [0.0, 0.0],
                );
                let tap = graph.add(
                    NodeKind::DelayRead(DelayRead {
                        line: 1,
                        ty: PortType::STEREO,
                        max_time: 0.05,
                        time: seconds(64.0),
                    }),
                    [0.0, 0.0],
                );
                let plugin = audio_plugin(&mut graph, 0, 0);
                let sum = graph.add(
                    NodeKind::Mix(Mix {
                        channels: 2,
                        inputs: 2,
                        gains: vec![0.0, linear_to_db(0.5)],
                    }),
                    [0.0, 0.0],
                );
                graph.connect(input, 0, sum, 0);
                graph.connect(tap, 0, plugin, 0);
                graph.connect(plugin, 0, sum, 2);
                graph.connect(sum, 0, other, 0);
            }

            let mut engine = Engine::new();
            engine.prepare(128, &[2]);
            load(&mut engine, &graph);
            let mut heard = Vec::new();
            for block in 0..3 {
                let daw_in = if block == 0 {
                    impulse(128, 0)
                } else {
                    vec![0.0; 2 * 128]
                };
                let mut daw_out = vec![0.0f32; 2 * 128];
                engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
                heard.extend_from_slice(&daw_out[..128]);
            }
            heard
        };

        let alone = patch(false);
        assert!(
            alone.iter().filter(|v| v.abs() > 0.05).count() >= 3,
            "the delay repeats on its own"
        );
        assert_eq!(alone, patch(true), "and repeats the same beside another");
    }

    fn stereo_in(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        )
    }

    fn stereo_out(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        )
    }

    /// The two halves of an audio delay line, on line 0, `samples` back.
    fn audio_delay(graph: &mut Graph, samples: f64) -> (NodeId, NodeId) {
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::STEREO,
            }),
            [0.0, 0.0],
        );
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::STEREO,
                // Room for the sweeps, without asking for a megabyte of ring.
                max_time: 0.05,
                time: seconds(samples),
            }),
            [0.0, 0.0],
        );
        (write, read)
    }

    /// An impulse on both channels of a stereo block.
    fn impulse(frames: usize, at: usize) -> Vec<f32> {
        let mut daw_in = vec![0.0f32; 2 * frames];
        daw_in[at] = 1.0;
        daw_in[frames + at] = 1.0;
        daw_in
    }

    /// Verifies that an audio delay line outputs samples delayed by the expected duration.
    #[test]
    fn an_audio_delay_returns_what_it_was_given_a_delay_later() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 64.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        // One impulse in the first block, then silence.
        let mut heard = Vec::new();
        for block in 0..3 {
            let daw_in = if block == 0 {
                impulse(128, 8)
            } else {
                vec![0.0; 2 * 128]
            };
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
            heard.extend_from_slice(&daw_out[..128]);
        }

        let peak = heard
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap();
        assert_eq!(peak.0, 8 + 64, "the impulse comes back 64 samples later");
        assert!((peak.1 - 1.0).abs() < 1e-3, "and at its original height");
    }

    /// Verifies that delay times shorter than chunk length are clamped to minimum safe distance.
    #[test]
    fn a_delay_shorter_than_a_chunk_is_held_at_the_chunk() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 1.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        let mut heard = Vec::new();
        for block in 0..2 {
            let daw_in = if block == 0 {
                impulse(128, 0)
            } else {
                vec![0.0; 2 * 128]
            };
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
            heard.extend_from_slice(&daw_out[..128]);
        }
        let peak = heard
            .iter()
            .position(|v| v.abs() > 0.5)
            .expect("the impulse came back");
        // The quantum is 32, plus the two samples the interpolator needs ahead
        // of the read pointer. Asked for 1: a delay of 1 would have read this
        // chunk's own writes.
        assert_eq!(peak, 34);
    }

    /// Verifies consistent delay feedback behavior across different host block sizes.
    #[test]
    fn a_feedback_loop_sounds_the_same_at_any_block_size() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 64.0);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                // Empty is unity: what a mix did before it had gains.
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 2);
        graph.connect(mix, 0, output, 0);
        graph.connect(mix, 0, write, 0);

        let render = |block: usize| -> Vec<f32> {
            let mut engine = Engine::new();
            engine.prepare(512, &[2]);
            load(&mut engine, &graph);
            let mut heard = Vec::new();
            let mut at = 0;
            while at < 512 {
                let mut daw_in = vec![0.0f32; 2 * block];
                if at == 0 {
                    daw_in[0] = 1.0;
                    daw_in[block] = 1.0;
                }
                let mut daw_out = vec![0.0f32; 2 * block];
                engine.run_audio(&audio_ctx(block as u32), &daw_in, &mut daw_out, &mut Adders);
                heard.extend_from_slice(&daw_out[..block]);
                at += block;
            }
            heard
        };

        let big = render(512);
        let small = render(64);
        assert_eq!(big.len(), small.len());
        let worst = big
            .iter()
            .zip(&small)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "largest difference {worst}");
        assert!(
            big.iter().filter(|v| v.abs() > 0.5).count() >= 4,
            "the loop repeated"
        );
    }

    /// A plugin called for a sub-block is handed that sub-block, with its
    /// channels where the format says they are.
    ///
    /// The pool packs channels at the DAW block's length so that a buffer
    /// written whole can be read a piece at a time; a plugin wants them packed
    /// at the length of the call. The two agree only when the chunk is the
    /// whole block, which is every program without a feedback loop in it — so
    /// the gather and scatter that bridge them are reached exactly when
    /// nothing else in the suite looks, and a mistake there would put the
    /// right samples on the wrong channel.
    #[test]
    fn a_plugin_called_for_a_sub_block_is_handed_that_sub_block() {
        /// Records every input region it is given, channel by channel.
        #[derive(Default)]
        struct Records {
            heard: Vec<Vec<f32>>,
        }
        impl AudioInstances for Records {
            fn process(
                &mut self,
                _instance: u32,
                _notes: &[Event],
                input: &[f32],
                output: &mut [f32],
                chunk: AudioChunk,
            ) {
                self.heard.resize(chunk.input_channels as usize, Vec::new());
                for ch in 0..chunk.input_channels {
                    self.heard[ch as usize].extend_from_slice(&input[chunk.channel(ch)]);
                }
                for ch in 0..chunk.output_channels {
                    output[chunk.channel(ch)].fill(0.0);
                }
            }
        }

        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let plugin = audio_plugin(&mut graph, 0, 0);
        // The plugin sits *inside* the loop, which is the only way it runs at
        // sub-block granularity. The delay is longer than the block, so within
        // the one block played nothing has come back round yet and what
        // reaches the plugin is the DAW's own input — anything summed into it
        // would hide a packing mistake behind arithmetic.
        let (write, read) = audio_delay(&mut graph, 4096.0);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 2);
        graph.connect(mix, 0, plugin, 0);
        graph.connect(plugin, 0, write, 0);
        graph.connect(plugin, 0, output, 0);

        const BLOCK: usize = 128;
        let mut engine = Engine::new();
        engine.prepare(BLOCK as u32, &[2]);
        load(&mut engine, &graph);
        assert_eq!(
            engine.chunking(),
            Chunking::SubBlock,
            "the loop is what makes the chunks short"
        );

        // Two channels a mixup could not confuse for one another.
        let mut daw_in = vec![0.0f32; 2 * BLOCK];
        for i in 0..BLOCK {
            daw_in[i] = i as f32;
            daw_in[BLOCK + i] = -(i as f32);
        }
        let mut records = Records::default();
        engine.run_audio(
            &audio_ctx(BLOCK as u32),
            &daw_in,
            &mut vec![0.0; 2 * BLOCK],
            &mut records,
        );

        assert_eq!(records.heard.len(), 2, "a stereo plugin heard two channels");
        assert_eq!(
            records.heard[0],
            daw_in[..BLOCK],
            "the chunks join back into the block that was played"
        );
        assert_eq!(records.heard[1], daw_in[BLOCK..], "and so does the other");
    }

    /// How often a plugin is called is settled by the shape of the patch, not
    /// by a number somebody is turning.
    ///
    /// The plugin here feeds a delay line and nothing brings its output back
    /// round, so it is not in a loop and is called once for the block —
    /// whatever the delay time is set to. A plugin that *is* in a loop is
    /// covered by `a_plugin_called_for_a_sub_block_is_handed_that_sub_block`.
    #[test]
    fn moving_the_delay_time_does_not_change_how_often_a_plugin_runs() {
        struct Counting(usize);
        impl AudioInstances for Counting {
            fn process(
                &mut self,
                _instance: u32,
                _notes: &[Event],
                _input: &[f32],
                output: &mut [f32],
                chunk: AudioChunk,
            ) {
                self.0 += 1;
                for ch in 0..chunk.output_channels {
                    output[chunk.channel(ch)].fill(0.0);
                }
            }
        }

        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let plugin = audio_plugin(&mut graph, 0, 0);
        let (write, read) = audio_delay(&mut graph, 64.0);
        let time = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        graph.connect(input, 0, plugin, 0);
        graph.connect(plugin, 0, write, 0);
        graph.connect(read, 0, output, 0);
        graph.connect(time, 0, read, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::DelayRead { lane, .. } => *lane,
                _ => None,
            })
            .expect("the wired time control got a lane") as usize;

        let run = |seconds: f64| -> usize {
            let mut engine = Engine::new();
            engine.prepare(128, &[2]);
            load(&mut engine, &graph);
            let lanes_per_row = lane + 1;
            let lanes = vec![seconds; lanes_per_row * 4];
            let mut counting = Counting(0);
            engine.run_audio(
                &AudioContext {
                    frames: 128,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &lanes,
                    lanes_per_row,
                },
                &vec![0.0; 2 * 128],
                &mut vec![0.0; 2 * 128],
                &mut counting,
            );
            counting.0
        };
        assert_eq!(run(seconds(64.0)), run(seconds(400.0)));
        assert_eq!(
            run(seconds(64.0)),
            1,
            "one call for the block: the line carries nothing back to it"
        );
    }

    /// Verifies continuous tape-style pitch modulation during delay time sweeps without clicks.
    ///
    /// The signal written into the line is a ramp of one per sample, so what
    /// comes out is `t - d(t)` and the step between output samples is the
    /// playback speed. Holding the time still gives a step of exactly 1; a
    /// time shortening by a quarter of a sample per sample gives 1.25, which is
    /// the pitch moving up.
    #[test]
    fn sweeping_the_delay_time_moves_the_pitch_without_a_step() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 200.0);
        let time = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);
        graph.connect(time, 0, read, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::DelayRead { lane, .. } => *lane,
                _ => None,
            })
            .expect("the wired time control got a lane") as usize;
        let lanes_per_row = lane + 1;

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        let mut heard = Vec::new();
        let mut clock = 0.0f32;
        // Four blocks of ramp at a fixed 300 samples back to fill the line,
        // then four with the time sweeping from 300 to 268.
        for block in 0..8 {
            let mut daw_in = vec![0.0f32; 2 * 128];
            for i in 0..128 {
                daw_in[i] = clock;
                daw_in[128 + i] = clock;
                clock += 1.0;
            }
            let mut lanes = vec![0.0f64; lanes_per_row * 4];
            for row in 0..4 {
                let swept = (block - 4).max(0) as f64 * 4.0 + row as f64;
                lanes[row * lanes_per_row + lane] = seconds(300.0 - swept * 8.0);
            }
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(
                &AudioContext {
                    frames: 128,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &lanes,
                    lanes_per_row,
                },
                &daw_in,
                &mut daw_out,
                &mut Adders,
            );
            if block >= 4 {
                heard.extend_from_slice(&daw_out[..128]);
            }
        }

        // The first sub-block of the sweep is still coming up to speed: it
        // ramps from where the held pointer was, so it is the one chunk whose
        // step is 1. Everything after it is at the sweep's own rate.
        let steps: Vec<f32> = heard[32..].windows(2).map(|w| w[1] - w[0]).collect();
        // Eight samples of sweep every 32 of output: a quarter faster. Every
        // step, not just the average — one step out of line is what a click is.
        let worst = steps
            .iter()
            .map(|s| (s - 1.25).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.02, "largest departure from 1.25 was {worst}");
    }

    /// A mix of one input is a gain, which is the whole reason the gains live
    /// on `Mix` rather than on a node of their own.
    #[test]
    fn a_mix_of_one_is_a_gain() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let gain = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 1,
                gains: vec![linear_to_db(0.25)],
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, gain, 0);
        graph.connect(gain, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in: Vec<f32> = (0..2 * 8).map(|i| i as f32).collect();
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        let want: Vec<f32> = daw_in.iter().map(|v| v * 0.25).collect();
        assert_eq!(daw_out, want);
    }

    /// Each input has its own gain, and the sum is of the scaled ones. This is
    /// what turns a feedback loop's gain down below unity so it decays.
    #[test]
    fn each_mix_input_is_scaled_before_the_sum() {
        let mut graph = Graph::new();
        let a = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: vec![linear_to_db(0.5), linear_to_db(0.25)],
            }),
            [0.0, 0.0],
        );
        // The same source into both inputs: 0.5 + 0.25 of it should come out.
        graph.connect(a, 0, mix, 0);
        graph.connect(a, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![4.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(
            daw_out.iter().all(|&v| (v - 3.0).abs() < 1e-6),
            "{daw_out:?}"
        );
    }

    /// A gate is a `Mix` of one whose gain the parameter half switches, and
    /// this is the whole round trip: the control lands in a lane, the lane
    /// becomes a gain, the gain is unity or silence.
    #[test]
    fn a_gate_passes_or_silences_by_its_control() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::Gate(Gate {
                channels: 2,
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, gate, 0);
        graph.connect(control, 0, gate, 1);
        graph.connect(gate, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[2]);
        load(&mut engine, &graph);

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut render = |control: f64| -> Vec<f32> {
            let mut lanes = vec![0.0; width];
            lanes[0] = control;
            engine.run(&ctx(8), &mut lanes);
            let daw_in = vec![1.0f32; 2 * 8];
            let mut daw_out = vec![0.0f32; 2 * 8];
            engine.run_audio(
                &AudioContext {
                    frames: 8,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &lanes,
                    lanes_per_row: width,
                },
                &daw_in,
                &mut daw_out,
                &mut Adders,
            );
            daw_out
        };

        assert!(
            render(1.0).iter().all(|&v| (v - 1.0).abs() < 1e-6),
            "an open gate is unity gain"
        );
        assert!(
            render(0.0).iter().all(|&v| v.abs() < 1e-6),
            "a shut gate is silence"
        );
    }

    /// When a gain socket is driven by a parameter source, the parameter value
    /// is interpreted as decibels and converted to a linear multiplier for audio.
    #[test]
    fn a_driven_gain_socket_interprets_its_value_as_decibels() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let gain_ctl = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 1,
                gains: vec![0.0],
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        // Sockets alternate: input 1 is socket 0, gain 1 is socket 1.
        graph.connect(gain_ctl, 0, mix, 1);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Mix { inputs, .. } => inputs[0].lane,
                _ => None,
            })
            .expect("the wired gain control got a lane") as usize;
        let lanes_per_row = lane + 1;
        let lanes = vec![-6.0; lanes_per_row];

        let mut engine = Engine::new();
        engine.prepare(8, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![1.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 8,
                sample_rate: RATE,
                lanes: &lanes,
                lanes_per_row,
            },
            &daw_in,
            &mut daw_out,
            &mut Adders,
        );

        let want_linear = db_to_linear(-6.0) as f32;
        assert!(
            daw_out.iter().all(|&v| (v - want_linear).abs() < 1e-6),
            "expected {want_linear}, got {daw_out:?}"
        );
    }

    /// Verifies that resizing max delay time allocates a larger ring buffer while preserving existing samples.
    #[test]
    fn a_longer_max_time_gets_a_longer_ring_and_keeps_what_was_in_it() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 200.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut program = compile(&graph, SLOTS).unwrap();
        let sized = program.size_rings(RATE, &[]);
        // 0.05 s at 48 kHz, plus the interpolator's headroom.
        assert_eq!(program.audio_ring_len, vec![2404]);
        assert_eq!(program.audio_rings[0].len(), MAX_CHANNELS * 2404);

        // Publishing again with nothing changed hands over no ring at all.
        let mut again = compile(&graph, SLOTS).unwrap();
        let sized_again = again.size_rings(RATE, &sized);
        assert!(
            again.audio_rings[0].is_empty(),
            "an unchanged line is left alone"
        );
        assert_eq!(sized_again, sized);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        let handoff = Handoff::new();
        handoff.send(Box::new(program));
        assert!(engine.adopt(&handoff));

        let daw_in = impulse(128, 8);
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);

        // Now ask for four times the range. The ring has to be reallocated on
        // the main thread, and the impulse still in it has to survive.
        if let Some(NodeKind::DelayRead(DelayRead { max_time, .. })) =
            graph.node_mut(read).map(|n| &mut n.kind)
        {
            *max_time = 0.2;
        }
        let mut wider = compile(&graph, SLOTS).unwrap();
        wider.size_rings(RATE, &sized);
        assert_eq!(wider.audio_ring_len, vec![9604]);
        assert!(
            !wider.audio_rings[0].is_empty(),
            "a changed line gets a new ring"
        );
        let handoff = Handoff::new();
        handoff.send(Box::new(wider));
        assert!(engine.adopt(&handoff));

        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(
            &audio_ctx(128),
            &vec![0.0; 2 * 128],
            &mut daw_out,
            &mut Adders,
        );
        assert_eq!(
            daw_out[..128]
                .iter()
                .position(|v| v.abs() > 0.9)
                .expect("the impulse came through the reallocation"),
            8 + 200 - 128
        );
    }

    /// Verifies that recompiling a patch preserves audio delay buffer contents.
    #[test]
    fn a_recompile_leaves_the_line_full() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        // Longer than a block, so the impulse is still inside the line when
        // the swap happens.
        let (write, read) = audio_delay(&mut graph, 200.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        let daw_in = impulse(128, 8);
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);

        // An edit somewhere else entirely, between the write and the read.
        let constant = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        let slot = param_sink(&mut graph);
        graph.connect(constant, 0, slot, 0);
        load(&mut engine, &graph);

        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(
            &audio_ctx(128),
            &vec![0.0; 2 * 128],
            &mut daw_out,
            &mut Adders,
        );
        let peak = daw_out[..128]
            .iter()
            .fold(0.0f32, |best, v| best.max(v.abs()));
        assert!(peak > 0.9, "the impulse survived the swap, peak {peak}");
        assert_eq!(
            daw_out[..128]
                .iter()
                .position(|v| v.abs() > 0.9)
                .expect("and at the right moment"),
            8 + 200 - 128
        );
    }
}
