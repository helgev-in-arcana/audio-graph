//! The clock a key-switched tremolo cuts by, and what one tremolo keeps
//! between rows.
//!
//! The MIDI half and the audio half each own a [`TremoloLine`], but they run
//! the same [`Clock`] over the same events, one sample at a time. That is what
//! puts a note's cut and the audio's cut on the same sample: two clocks that
//! agreed only on the arithmetic, one stepping edge to edge and the other
//! sample by sample, would disagree about which sample a boundary rounds to.

use plugin_host::{Event, NoteEvent};

use crate::ir::{MAX_TREMOLO_ROWS, TREMOLO_NOTES, TremoloSpec};
use crate::notes::NoteLedger;

use super::lines::Slot;
use super::notes::NoteState;

/// Slack for a position that lands on a boundary give or take the last bit.
/// Far below a sample: at 300 bpm and 192 kHz one sample is 2.6e-5 beats.
const EPSILON: f64 = 1e-9;

/// What the clock crossed at a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Edge {
    /// A new step began: whatever was cut sounds again.
    Step,
    /// The sounding part of a step ended.
    Cut,
}

/// Where a tremolo is: which key started it and how far it has gone since.
///
/// The position is counted in samples from a point whose position in beats
/// is known, and multiplied out rather than summed. Summing a beats-per-sample
/// step would drift, and a drift of one sample at a boundary is exactly what
/// the two halves must not disagree about.
#[derive(Debug, Clone, Copy)]
pub(super) struct Clock {
    /// The row whose key started the running tremolo.
    row: Option<u8>,
    /// Rows whose key is down, one bit each, for a tremolo that runs while
    /// its key is held.
    held: u8,
    /// Beats gone at `samples == 0`, and the tempo the samples since are
    /// counted at.
    base: f64,
    samples: u64,
    tempo: f64,
    /// The step and the part of it the previous sample was in, for telling
    /// when one is crossed.
    step: u64,
    sounding: bool,
}

impl Clock {
    const STOPPED: Clock = Clock {
        row: None,
        held: 0,
        base: 0.0,
        samples: 0,
        tempo: 0.0,
        step: 0,
        sounding: true,
    };

    /// Whether the sample the clock is on sounds: always, unless a tremolo is
    /// running and it is in the cut part of a step.
    pub(super) fn sounding(&self) -> bool {
        self.row.is_none() || self.sounding
    }

    /// Carry the position over a change of tempo, so the samples after it are
    /// counted at the new one. Called at the start of every run, which is as
    /// often as the host can change it.
    pub(super) fn follow(&mut self, tempo: f64, sample_rate: f64) {
        if tempo != self.tempo {
            self.base = self.beats(sample_rate);
            self.samples = 0;
            self.tempo = tempo;
        }
    }

    fn beats(&self, sample_rate: f64) -> f64 {
        self.base + self.samples as f64 * self.tempo / (60.0 * sample_rate.max(1.0))
    }

    /// Which step the clock is in and whether it is in the sounding part.
    fn place(&self, spec: &TremoloSpec, row: u8, sample_rate: f64) -> (u64, bool) {
        let length = spec.steps[usize::from(row).min(MAX_TREMOLO_ROWS - 1)];
        if length <= 0.0 {
            return (0, true);
        }
        let steps = self.beats(sample_rate) / length;
        let step = (steps + EPSILON).floor().max(0.0);
        let into = (steps - step).max(0.0);
        (step as u64, into < spec.share - EPSILON)
    }

    /// Look at the sample the clock is on, and say what was crossed getting
    /// there.
    pub(super) fn edge(&mut self, spec: &TremoloSpec, sample_rate: f64) -> Option<Edge> {
        let row = self.row?;
        let (step, sounding) = self.place(spec, row, sample_rate);
        let edge = if step != self.step && sounding {
            Some(Edge::Step)
        } else if self.sounding && !sounding {
            // Also a new step whose sounding part is shorter than a sample:
            // there is nothing to strike, only something to cut.
            Some(Edge::Cut)
        } else {
            None
        };
        self.step = step;
        self.sounding = sounding;
        edge
    }

    pub(super) fn tick(&mut self) {
        self.samples += 1;
    }

    /// A key that steers the tremolo went down (`on`) or up. Returns whether
    /// a tremolo started here — which is a step beginning, and struck as one.
    pub(super) fn switch(&mut self, spec: &TremoloSpec, key: i16, on: bool) -> bool {
        let row = spec.row_of(key).map(|row| row as u8);
        if spec.latch {
            match row {
                // Struck again, it starts again: a key switch placed at the
                // start of every phrase, as a safeguard, puts each phrase on
                // its own grid rather than on the first one's.
                Some(row) if on => {
                    self.start(row);
                    true
                }
                None if on => {
                    self.row = None;
                    false
                }
                _ => false,
            }
        } else {
            let Some(row) = row else { return false };
            if on {
                self.held |= 1 << row;
                self.start(row);
                return true;
            }
            self.held &= !(1 << row);
            if self.row != Some(row) {
                return false;
            }
            // Letting go of the key in charge hands over to one still down,
            // from the moment of letting go — the other key's own moment
            // has passed, and its grid with it.
            match (0..MAX_TREMOLO_ROWS as u8).find(|r| self.held & (1 << r) != 0) {
                Some(other) => {
                    self.start(other);
                    true
                }
                None => {
                    self.row = None;
                    false
                }
            }
        }
    }

    /// Start `row`'s tremolo at the sample the clock is on.
    fn start(&mut self, row: u8) {
        self.row = Some(row);
        self.base = 0.0;
        self.samples = 0;
        self.step = 0;
        self.sounding = true;
    }

    /// Let go of every held key, as the notes are forgotten. A latched
    /// tremolo keeps its key — it is a setting the player made, not a note
    /// being played — and starts its grid again from here.
    fn forget_keys(&mut self) {
        if self.held != 0 {
            self.held = 0;
            self.row = None;
        }
        if let Some(row) = self.row {
            self.start(row);
        }
    }
}

/// One note a MIDI tremolo is cutting, and whether it is cut right now.
#[derive(Debug, Clone, Copy)]
struct Held {
    /// The note-on it arrived as, which is what each step strikes again.
    on: NoteEvent,
    cut: bool,
}

impl Held {
    fn id(&self) -> Option<i32> {
        match self.on {
            NoteEvent::NoteOn { note_id, .. } => note_id,
            _ => None,
        }
    }

    fn off(&self, at: u32) -> Event {
        let NoteEvent::NoteOn {
            note_id,
            port,
            channel,
            key,
            ..
        } = self.on
        else {
            unreachable!("only note-ons are held");
        };
        Event::Note(NoteEvent::NoteOff {
            note_id,
            port,
            channel,
            key,
            velocity: 0.0,
            sample_offset: at,
        })
    }
}

/// What one tremolo keeps from row to row, and across a recompile.
///
/// A note that is cut is one the graph is still holding for the player, so
/// the ledger counts it as delivered until it sounds again or ends — the way
/// it counts a note waiting in a delay. Otherwise a plugin finishing the
/// last cut voice before the next step would have the note reported ended to
/// the DAW while its key is still down.
#[derive(Debug)]
pub(super) struct TremoloLine {
    pub(super) clock: Clock,
    notes: Vec<Held>,
    /// The audio half's gain, where the last sample left it.
    pub(super) gain: f64,
    node: u32,
}

impl TremoloLine {
    pub(super) fn new() -> TremoloLine {
        TremoloLine {
            clock: Clock::STOPPED,
            notes: Vec::with_capacity(TREMOLO_NOTES),
            gain: 1.0,
            node: u32::MAX,
        }
    }

    /// Give back the cut notes this line was holding for the ledger, ahead of
    /// forgetting them.
    pub(super) fn release(&mut self, ledger: &mut NoteLedger) {
        for held in self.notes.drain(..) {
            if let (true, Some(id)) = (held.cut, held.id()) {
                ledger.finished(id);
            }
        }
    }

    /// Forget what is being played, and nothing else; the ledger is settled
    /// by the caller. See [`Clock::forget_keys`].
    pub(super) fn forget_notes(&mut self) {
        self.notes.clear();
        self.clock.forget_keys();
    }

    /// One row of the MIDI half: `events` in, cut and struck again into `out`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_notes(
        &mut self,
        spec: &TremoloSpec,
        mute: bool,
        events: &[Event],
        out: &mut Vec<Event>,
        dropped: &mut u64,
        (start, frames): (u32, u32),
        (sample_rate, tempo): (f64, f64),
        ledger: &mut NoteLedger,
    ) {
        self.clock.follow(tempo, sample_rate);
        let mut next = 0;
        for at in start..start + frames {
            if let Some(edge) = self.clock.edge(spec, sample_rate) {
                self.cross(edge, at, out, dropped, ledger);
            }
            while let Some(&event) = events.get(next)
                && event.sample_offset() <= at
            {
                next += 1;
                self.take(spec, mute, event, at, out, dropped, ledger);
            }
            self.clock.tick();
        }
        // A source is never meant to hand over an event past the row, but one
        // that did is passed on rather than lost.
        let end = (start + frames).saturating_sub(1);
        for &event in &events[next..] {
            self.take(spec, mute, event, end, out, dropped, ledger);
        }
    }

    fn cross(
        &mut self,
        edge: Edge,
        at: u32,
        out: &mut Vec<Event>,
        dropped: &mut u64,
        ledger: &mut NoteLedger,
    ) {
        for held in &mut self.notes {
            match edge {
                Edge::Step if held.cut => {
                    NoteState::push(out, dropped, Event::Note(held.on.at_offset(at)));
                    held.cut = false;
                    if let Some(id) = held.id() {
                        ledger.finished(id);
                    }
                }
                Edge::Cut if !held.cut => {
                    NoteState::push(out, dropped, held.off(at));
                    held.cut = true;
                    if let Some(id) = held.id() {
                        ledger.delivered(id);
                    }
                }
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn take(
        &mut self,
        spec: &TremoloSpec,
        mute: bool,
        event: Event,
        at: u32,
        out: &mut Vec<Event>,
        dropped: &mut u64,
        ledger: &mut NoteLedger,
    ) {
        let Event::Note(note) = event else {
            NoteState::push(out, dropped, event);
            return;
        };
        let (on, key, channel, id) = match note {
            NoteEvent::NoteOn {
                key,
                channel,
                note_id,
                ..
            } => (true, key, channel, note_id),
            NoteEvent::NoteOff {
                key,
                channel,
                note_id,
                ..
            } => (false, key, channel, note_id),
            _ => {
                NoteState::push(out, dropped, event);
                return;
            }
        };

        if spec.steers(key) {
            if self.clock.switch(spec, key, on) {
                self.cross(Edge::Step, at, out, dropped, ledger);
            }
            if !mute {
                NoteState::push(out, dropped, event);
            }
            return;
        }

        if on {
            if self.notes.len() == self.notes.capacity() {
                NoteState::push(out, dropped, event);
                return;
            }
            // Struck in the cut part of a step, it waits for the next one: a
            // note that sounded for what is left of a rest would be a note
            // the grid never asked for.
            let cut = !self.clock.sounding();
            self.notes.push(Held { on: note, cut });
            if cut {
                if let Some(id) = id {
                    ledger.delivered(id);
                }
            } else {
                NoteState::push(out, dropped, event);
            }
            return;
        }

        // The note this ends: by id where there is one, else the oldest on
        // its key, which is how the ledger matches a note-off too.
        let found = self.notes.iter().position(|held| match (id, held.id()) {
            (Some(id), Some(held)) => id == held,
            _ => {
                let NoteEvent::NoteOn {
                    key: k, channel: c, ..
                } = held.on
                else {
                    return false;
                };
                k == key && c == channel
            }
        });
        let Some(index) = found else {
            NoteState::push(out, dropped, event);
            return;
        };
        let held = self.notes.remove(index);
        if held.cut {
            // Already ended by the cut; the player letting go only means it
            // does not come back.
            if let Some(id) = held.id() {
                ledger.finished(id);
            }
        } else {
            NoteState::push(out, dropped, event);
        }
    }
}

impl Slot for TremoloLine {
    fn node(&self) -> u32 {
        self.node
    }
    fn set_node(&mut self, node: u32) {
        self.node = node;
    }
    fn clear(&mut self) {
        self.notes.clear();
        self.clock = Clock::STOPPED;
        self.gain = 1.0;
    }
}
