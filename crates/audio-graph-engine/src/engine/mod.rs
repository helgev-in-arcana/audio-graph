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
//! Outside of its tests (`tests.rs`), nothing in this module mentions `graph`
//! or a node kind, and that is the thread boundary rather than an accident:
//! what reaches this side is a [`Program`] and nothing else. A
//! `use crate::graph::…` in any other file here is the signal that something
//! has leaked across.

use plugin_host::{Event, NoteEvent};

use crate::handoff::Handoff;
use crate::ir::{
    AudioOp, Buf, Chunking, Detect, Follow, MAX_AUDIO_DELAY_LINES, MAX_BUFFER_CHANNELS,
    MAX_BUFFERS, MAX_CHANNELS, MAX_COMPENSATION, MAX_COMPENSATORS, MAX_DELAY_LINES, MAX_DELAY_TAPS,
    MAX_LATCHES, MAX_LFOS, MAX_NOTE_BUFS, MAX_NOTE_EMITS, MAX_REGISTERS, MathOp, NOTE_BUF_CAPACITY,
    NoteOp, NoteStream, Op, Operand, PreparedProgram, Program, RateSpec, Stage, Waveform,
};
use crate::nodes::db_to_linear;
use crate::notes::{Ended, NoteLedger};
use subhost_adapter::{AudioChunk, AudioInstances, MIN_QUANTUM, ScheduleView, SlotSchedule};

mod audio;
mod lines;
mod notes;
mod params;
#[cfg(test)]
mod tests;

use audio::Window;
use lines::{Move, copy_ring, reorder};
use notes::{NoteState, key_bit};

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

pub struct Engine {
    program: Option<Box<PreparedProgram>>,
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

    /// The adopted publication within its publisher, or zero before adoption.
    pub fn publication(&self) -> u64 {
        self.program
            .as_ref()
            .map_or(0, |program| program.publication)
    }

    /// Picks up a newly compiled program if one is waiting in the handoff channel.
    ///
    /// Returns `true` if a new program was adopted. Realtime-safe: does not allocate or lock.
    pub fn adopt(&mut self, publisher: &crate::ProgramPublisher) -> bool {
        self.adopt_handoff(publisher.handoff())
    }

    pub(crate) fn adopt_handoff(&mut self, handoff: &Handoff<PreparedProgram>) -> bool {
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
        self.ledger.adopt_destinations(
            &mut self
                .program
                .as_mut()
                .expect("take reported a swap")
                .fallback_destinations,
        );
        let next = self.program.as_ref().expect("take reported a swap");

        let remap = self.notes.adopt(&next.note_streams);
        for marks in self.note_marks.iter_mut().take(self.note_rows) {
            let previous = *marks;
            *marks = remap.map(|from| from.map_or(0, |index| previous[index]));
        }

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
        let (rings, heads) = (&mut self.rings, &mut self.ring_heads);
        reorder(
            &mut self.ring_nodes,
            &mut self.ring_order,
            &next.delay_nodes,
            |step| match step {
                Move::Swap(a, b) => {
                    rings.swap(a, b);
                    heads.swap(a, b);
                }
                Move::Clear(i) => {
                    rings[i].fill(0.0);
                    heads[i] = 0;
                }
            },
        );
        // An audio ring's length travels with it: the length decides whether
        // a ring handed over with the program replaces this one below.
        let (rings, heads, lens) = (
            &mut self.audio_rings,
            &mut self.audio_ring_heads,
            &mut self.audio_ring_len,
        );
        reorder(
            &mut self.audio_ring_nodes,
            &mut self.audio_ring_order,
            &next.audio_delay_nodes,
            |step| match step {
                Move::Swap(a, b) => {
                    rings.swap(a, b);
                    heads.swap(a, b);
                    lens.swap(a, b);
                }
                Move::Clear(i) => {
                    rings[i].fill(0.0);
                    heads[i] = 0;
                }
            },
        );
        // When ring lengths change, new buffers provided by the main thread are swapped in.
        let next = self
            .program
            .as_mut()
            .expect("take reported a swap")
            .program_mut();
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
    pub fn release(&mut self) -> Option<Box<PreparedProgram>> {
        self.program.take()
    }

    /// The playhead moved: forget what was sounding and what was in flight.
    ///
    /// Called by the DAW, on the audio thread. Key switch latches survive it,
    /// and that is the whole difference between this and
    /// [`reset_everything`][Engine::reset_everything]: a latch is a setting
    /// the user made with a key rather than a sound in progress, and seeking
    /// in a timeline is not a way of asking to undo it.
    ///
    /// The DAW is told nothing about the notes dropped here, because it is the
    /// one that jumped. See [`NoteLedger::clear`].
    pub fn reset(&mut self) {
        self.ledger.clear();
        self.forget_notes();
        self.forget_params();
        self.forget_audio();
    }

    /// Forget everything the graph remembers, and report the notes still alive
    /// into `ended`.
    ///
    /// What the editor's Reset asks for. Unlike a transport jump this takes
    /// the latches too: it exists for a patch whose stateful nodes have been
    /// left holding something that no longer matches what is on the canvas —
    /// a `Held Keys` counting a note-off that arrived while the wire was
    /// somewhere else — and a latch is exactly as able to be stranded that way
    /// as anything else here.
    ///
    /// Nothing on the wire caused this, so the DAW has no reason to think the
    /// notes it asked for are over. It is told, through `ended`.
    pub fn reset_everything(&mut self, ended: &mut Vec<Ended>) {
        self.ledger.end_all(ended);
        self.forget_notes();
        self.forget_params();
        // A latch nothing has set reads as NaN, which is what a fresh program
        // leaves behind; see `adopt`.
        self.latches.iter_mut().for_each(|v| *v = f64::NAN);
        self.forget_audio();
    }

    /// Forget what is being played, and nothing else.
    ///
    /// What an All Notes Off on the wire asks for, and no more than that: a
    /// DAW is free to send one on every stop, so taking the latches or the
    /// delay lines with it would undo the user's patch a few times an hour.
    pub fn reset_notes(&mut self, ended: &mut Vec<Ended>) {
        self.ledger.end_all(ended);
        self.forget_notes();
    }

    /// The note half of a reset: who is playing, and what has been said about
    /// it. The ledger is the caller's to settle, because how the DAW hears
    /// about it is the one thing the three entry points disagree on.
    fn forget_notes(&mut self) {
        self.translated.clear();
        for buf in &mut self.notes.bufs {
            buf.silence();
            buf.events.clear();
        }
        self.note_rows = 0;
        // NaN is "nothing sent yet", so every controller-generating op sends
        // its value again rather than holding back a number that matches what
        // a sub-plugin no longer has.
        self.notes.emitted.iter_mut().for_each(|v| *v = f64::NAN);
    }

    /// The parameter half: what a modulator has been carrying between blocks.
    /// Latches are not in it — see the two callers that differ over them.
    fn forget_params(&mut self) {
        self.phases.iter_mut().for_each(|p| *p = 0.0);
        self.holds.iter_mut().for_each(|h| *h = 0.0);
        self.rings.iter_mut().for_each(|r| r.fill(0.0));
        self.ring_heads.iter_mut().for_each(|h| *h = 0);
    }

    /// The audio half: every sample still on its way somewhere.
    ///
    /// The delay rings are emptied rather than only rewound. Moving a head
    /// back to zero without clearing what it points at leaves the old contents
    /// exactly where a read a fraction of a ring behind it will find them, so
    /// a reset would be heard as the tail carrying on.
    fn forget_audio(&mut self) {
        self.pool.fill(0.0);
        self.compensators.fill(0.0);
        self.compensator_heads.iter_mut().for_each(|h| *h = 0);
        // Emptied where they stand. An audio delay's ring is sized and
        // allocated on the main thread and rides in on the program, so there
        // is nothing to hand back and nothing to resize.
        self.audio_rings.iter_mut().for_each(|r| r.fill(0.0));
        self.audio_ring_heads.iter_mut().for_each(|h| *h = 0);
        // No previous position, so the first chunk after this jumps to where
        // its tap says rather than sweeping from where the old one left off.
        self.tap_distance.iter_mut().for_each(|d| *d = f64::NAN);
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
    pub fn end_block<'a>(
        &mut self,
        from_plugins: impl IntoIterator<Item = &'a Event>,
        ended: &mut Vec<Ended>,
    ) {
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

    /// The latency in samples of the program the engine is running.
    ///
    /// An audio-thread answer. A program reaches the engine through the
    /// handoff, and only [`Engine::adopt`] turns that, so a caller that has
    /// not run a block since the program was published is being told about
    /// the one before it — and before the first block, about no program at
    /// all. What the DAW has to be told at activate is the compiler's answer,
    /// not this one.
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

    /// Evaluates one complete host block, including schedule setup, note
    /// ingestion, parameter stages, and audio stages in dependency order.
    #[allow(clippy::too_many_arguments)]
    pub fn run_block(
        &mut self,
        schedule: &mut SlotSchedule,
        daw_slots: &[f64],
        events: &[Event],
        frames: u32,
        quantum: u32,
        sample_rate: f64,
        tempo_bpm: f64,
        daw_in: &[f32],
        daw_out: &mut [f32],
        nodes: &mut dyn AudioInstances,
    ) -> bool {
        if schedule.quantum() != quantum {
            schedule.set_quantum(quantum);
        }
        let Ok(blocks) = schedule.begin(frames) else {
            daw_out.fill(0.0);
            return false;
        };
        if !self.has_program() {
            daw_out.fill(0.0);
            schedule.fill(daw_slots);
            return false;
        }
        for index in 0..blocks {
            let values = schedule.block_mut(index);
            let slots = daw_slots.len().min(values.len());
            values[..slots].copy_from_slice(&daw_slots[..slots]);
            values[slots..].fill(0.0);
        }
        self.begin_block(events);
        self.clear_output(daw_out);
        for stage in 0..self.stages() {
            for index in 0..blocks {
                self.run_stage(
                    stage,
                    &BlockContext {
                        sample_rate,
                        tempo_bpm,
                        frames: schedule.frames_of(index),
                        offset: schedule.offset(index),
                        block: frames,
                        row: index as u32,
                    },
                    schedule.block_mut(index),
                );
            }
            let view = schedule.view();
            self.run_audio_stage(
                stage,
                &AudioContext {
                    frames,
                    quantum: view.quantum(),
                    sample_rate,
                    lanes: view.rows(),
                    lanes_per_row: view.lanes(),
                },
                daw_in,
                daw_out,
                nodes,
            );
        }
        true
    }

    /// The block is the program's to fill: a channel no `Output` op reaches is
    /// silence, not whatever the caller's buffer already held. Called once
    /// before the stages, because each of them writes only its own part.
    pub fn clear_output(&self, daw_out: &mut [f32]) {
        daw_out.fill(0.0);
    }
}
