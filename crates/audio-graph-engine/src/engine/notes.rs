//! The note half: what each note buffer holds and what it amounts to.

use super::*;

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
pub(super) struct NoteBuf {
    /// One whole DAW block of events, appended a sub-block at a time as the
    /// parameter half walks the rows, plus the last sub-block of the block
    /// before it. See [`NoteState`] and [`Engine::note_marks`].
    pub(super) events: Vec<Event>,
    /// Which keys are down, one bit each.
    pub(super) held: u128,
    /// Which keys were struck in the sub-block this buffer last carried, so an
    /// op sees each note-on exactly once.
    pub(super) struck: u128,
    /// How many notes are down.
    pub(super) count: u32,
    /// Velocity of the most recent note-on, and the key it was on, normalized.
    /// Both held between notes.
    pub(super) velocity: f64,
    pub(super) key: f64,
}

impl NoteBuf {
    pub(super) fn new() -> NoteBuf {
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
    pub(super) fn silence(&mut self) {
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
pub(super) struct NoteState {
    /// One per note buffer, allocated in [`Engine::new`] and only ever cleared
    /// and refilled after that. A program swap happens on the audio thread, so
    /// nothing here may be sized from the program.
    pub(super) bufs: Vec<NoteBuf>,
    pub(super) streams: [Option<NoteStream>; MAX_NOTE_BUFS],
    /// Last value each controller-generating op sent, or NaN before its first.
    /// Forgotten on a program swap; see [`NoteOp::Emit`].
    pub(super) emitted: Vec<f64>,
    /// Events dropped because a buffer was full, since the last reset.
    ///
    /// Counted rather than silently swallowed: an overflow is a real fault and
    /// the number is the only way anyone would find out.
    pub(super) dropped: u64,
}

impl NoteState {
    pub(super) fn new() -> NoteState {
        NoteState {
            bufs: (0..MAX_NOTE_BUFS).map(|_| NoteBuf::new()).collect(),
            streams: [None; MAX_NOTE_BUFS],
            emitted: vec![f64::NAN; MAX_NOTE_EMITS],
            dropped: 0,
        }
    }

    /// Upstream buffers precede their readers, so each match can validate its source's match.
    pub(super) fn adopt(&mut self, streams: &[NoteStream]) -> [Option<usize>; MAX_NOTE_BUFS] {
        let mut remap = [None; MAX_NOTE_BUFS];
        for (to, next) in streams.iter().enumerate() {
            remap[to] = self.streams.iter().position(|old| {
                let Some(old) = old else { return false };
                old.node == next.node
                    && old.port == next.port
                    && old.kind == next.kind
                    && match (old.source, next.source) {
                        (None, None) => true,
                        (Some(old), Some(next)) => {
                            remap[usize::from(next)] == Some(usize::from(old))
                        }
                        _ => false,
                    }
            });
        }
        let mut order: [usize; MAX_NOTE_BUFS] = std::array::from_fn(|i| i);
        for (to, from) in remap.iter().enumerate() {
            if let Some(from) = from {
                let at = order.iter().position(|slot| slot == from).unwrap();
                self.bufs.swap(to, at);
                order.swap(to, at);
            }
        }
        for (index, buf) in self.bufs.iter_mut().enumerate() {
            if remap[index].is_none() {
                buf.events.clear();
                buf.silence();
            }
            self.streams[index] = streams.get(index).copied();
        }
        self.emitted.fill(f64::NAN);
        remap
    }

    /// Appends unless the buffer is full. Dropping an event is bad; growing a
    /// `Vec` on the audio thread is worse.
    pub(super) fn push(buf: &mut Vec<Event>, dropped: &mut u64, event: Event) {
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
    pub(super) fn run_notes_step(
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
    pub(super) fn follow_notes(&mut self, buf: u16, from: usize) {
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

/// One key's bit in the held/struck tables, or `None` for a key outside the
/// MIDI range — which a malformed event can carry and a bit shift cannot.
pub(super) fn key_bit(key: i16) -> Option<u128> {
    (0..128).contains(&key).then(|| 1u128 << key)
}
