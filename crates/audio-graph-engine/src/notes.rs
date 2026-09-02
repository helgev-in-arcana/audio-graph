//! The ledger of live notes.
//!
//! Two jobs that turn out to be one structure.
//!
//! **Identity.** A note arriving from the DAW is given an id of the graph's
//! own, unique among the notes alive in this instance, and every sub-plugin is
//! handed that id. The DAW's own id is not usable for this: neither format
//! promises one at all — CLAP spells out that a host may send `NOTE_OFF` with
//! `note_id == -1` for a note whose `NOTE_ON` carried one, and VST3 documents
//! `noteId` as "if not available then -1" — so a note-off has to be matched by
//! voice address instead. The address is `(channel, key)`, which several live
//! notes can share: overlapping notes are ordinary, whatever a plugin declares
//! about them, so each address holds a chain in arrival order.
//!
//! **Lifetime.** CLAP plugins report `NOTE_END` when a voice finishes, and the
//! host is expected to pass that back so the DAW can release whatever it was
//! holding for the note. A note here can reach several sub-plugins or none, so
//! the ledger counts deliveries and reports the note ended when the last one
//! finishes — or immediately, if it reached nobody.
//!
//! Nothing here holds a *value*. It is a register of who is who, which is why
//! the note path can carry its data on wires and still have somewhere to ask
//! "is this the same note".

use plugin_host::{NoteEvent, NoteId};

/// How many notes may be alive at once.
///
/// Alive means "arrived and not yet finished with", which outlasts the key
/// being down: a note released into a long reverb tail is still here until its
/// plugin says otherwise.
pub const MAX_LIVE_NOTES: usize = 256;

const CHANNELS: usize = 16;
const KEYS: usize = 128;

/// An index into the entry pool, and the note id the graph hands out.
type Idx = u16;

#[derive(Debug, Clone, Copy)]
struct Entry {
    port: i16,
    channel: i16,
    key: i16,
    /// The id the DAW gave this note, kept only so a report can be addressed
    /// the way the DAW will recognise.
    daw_id: NoteId,
    /// Arrival order, for picking the oldest when the pool runs out.
    serial: u64,
    /// The next note on the same `(channel, key)`, later in arrival order.
    next: Option<Idx>,
    /// How many sub-plugins were handed this note's note-on and have not said
    /// they are done with it.
    voices: u16,
    /// Whether the DAW's note-off for this note has come through.
    released: bool,
    /// Whether any sub-plugin was ever handed it. A note that reached one and
    /// has now been let go is finished; a note that never reached one is
    /// finished as soon as the block it arrived in is over.
    ever_delivered: bool,
    /// Whether the end of this note has been reported to the DAW. Once only.
    reported: bool,
    /// Which voice of a per-voice parameter evaluation this note owns.
    ///
    /// Unused today and reserved deliberately: CLAP's `events.h` gives the
    /// existence of `NOTE_END` as "when using polyphonic modulations, the host
    /// has to allocate and release voices for its polyphonic modulator", which
    /// is this ledger. Keeping the field means the entry is not a fixed triple
    /// that a later voice allocator would have to break open.
    #[allow(dead_code)]
    voice_slot: Option<u16>,
    live: bool,
}

impl Entry {
    const EMPTY: Entry = Entry {
        port: 0,
        channel: 0,
        key: 0,
        daw_id: None,
        serial: 0,
        next: None,
        voices: 0,
        released: false,
        ever_delivered: false,
        reported: false,
        voice_slot: None,
        live: false,
    };
}

/// A note the graph has finished with, to be reported back to the DAW.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub port: i16,
    pub channel: i16,
    pub key: i16,
    pub daw_id: NoteId,
}

pub struct NoteLedger {
    entries: Vec<Entry>,
    /// Free entry indices. A stack, so a note reuses the most recently
    /// finished slot and the pool stays warm.
    free: Vec<Idx>,
    /// `(channel, key)` → the oldest live note at that address.
    head: Vec<Option<Idx>>,
    next_serial: u64,
    /// Notes whose note-on arrived during the block being processed, so the
    /// end-of-block sweep knows which ones to judge for "reached nobody".
    arrived: Vec<Idx>,
    /// Ids the graph handed out but the pool could not hold, since the last
    /// reset. A stolen note is a real fault and the number is the only way
    /// anyone would find out.
    stolen: u64,
}

impl Default for NoteLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl NoteLedger {
    pub fn new() -> NoteLedger {
        NoteLedger {
            entries: vec![Entry::EMPTY; MAX_LIVE_NOTES],
            free: (0..MAX_LIVE_NOTES as Idx).rev().collect(),
            head: vec![None; CHANNELS * KEYS],
            next_serial: 0,
            arrived: Vec::with_capacity(MAX_LIVE_NOTES),
            stolen: 0,
        }
    }

    /// Forget everything. For a transport jump, where the notes that were
    /// sounding are not coming back.
    pub fn clear(&mut self) {
        self.entries.iter_mut().for_each(|e| *e = Entry::EMPTY);
        self.free.clear();
        self.free.extend((0..MAX_LIVE_NOTES as Idx).rev());
        self.head.iter_mut().for_each(|h| *h = None);
        self.arrived.clear();
        self.next_serial = 0;
    }

    /// How many notes have been forced out of the pool to make room.
    pub fn stolen(&self) -> u64 {
        self.stolen
    }

    /// Give an event from the DAW the graph's own note id.
    ///
    /// Returns the event to put on the wire. Anything without a note identity
    /// — a control change, raw bytes — comes back unchanged.
    pub fn translate(&mut self, event: NoteEvent) -> NoteEvent {
        match event {
            NoteEvent::NoteOn {
                note_id,
                port,
                channel,
                key,
                velocity,
                sample_offset,
            } => {
                let id = self.open(port, channel, key, note_id);
                NoteEvent::NoteOn {
                    note_id: Some(i32::from(id)),
                    port,
                    channel,
                    key,
                    velocity,
                    sample_offset,
                }
            }
            NoteEvent::NoteOff {
                note_id,
                port,
                channel,
                key,
                velocity,
                sample_offset,
            } => {
                let id = self.close(channel, key, note_id);
                NoteEvent::NoteOff {
                    note_id: id.map(i32::from),
                    port,
                    channel,
                    key,
                    velocity,
                    sample_offset,
                }
            }
            // A note expression names a note that is already here; it does not
            // open or close one. Matching it by address is the same problem as
            // matching a note-off, minus the unlinking.
            NoteEvent::Expression {
                note_id,
                port,
                channel,
                key,
                expression,
                value,
                sample_offset,
            } => NoteEvent::Expression {
                note_id: self.find(channel, key, note_id).map(i32::from),
                port,
                channel,
                key,
                expression,
                value,
                sample_offset,
            },
            other => other,
        }
    }

    /// A note-on arrived: take an entry and put it at the end of its address's
    /// chain.
    fn open(&mut self, port: i16, channel: i16, key: i16, daw_id: NoteId) -> Idx {
        let index = match self.free.pop() {
            Some(index) => index,
            None => self.steal(),
        };
        let serial = self.next_serial;
        self.next_serial += 1;
        self.entries[index as usize] = Entry {
            port,
            channel,
            key,
            daw_id,
            serial,
            next: None,
            voices: 0,
            released: false,
            ever_delivered: false,
            reported: false,
            voice_slot: None,
            live: true,
        };
        self.arrived.push(index);

        // The tail, so the chain reads oldest first and a note-off with no id
        // to go on releases the note that has been sounding longest — which is
        // what CLAP's own example expects.
        match self.slot(channel, key) {
            None => return index,
            Some(slot) => match self.head[slot] {
                None => self.head[slot] = Some(index),
                Some(mut at) => {
                    while let Some(next) = self.entries[at as usize].next {
                        at = next;
                    }
                    self.entries[at as usize].next = Some(index);
                }
            },
        }
        index
    }

    /// A note-off arrived: find its note, unlink it, and mark it released.
    fn close(&mut self, channel: i16, key: i16, daw_id: NoteId) -> Option<Idx> {
        let slot = self.slot(channel, key)?;
        let mut prev: Option<Idx> = None;
        let mut at = self.head[slot];

        // With an id, the note that carries it; without one, the oldest. A
        // wrong id falls through to the oldest rather than dropping the
        // note-off, because a note left with no way to end is worse than one
        // ended early.
        if daw_id.is_some() {
            let mut walk_prev: Option<Idx> = None;
            let mut walk = at;
            while let Some(index) = walk {
                if self.entries[index as usize].daw_id == daw_id {
                    prev = walk_prev;
                    at = Some(index);
                    break;
                }
                walk_prev = walk;
                walk = self.entries[index as usize].next;
            }
        }

        let index = at?;
        let next = self.entries[index as usize].next;
        match prev {
            Some(prev) => self.entries[prev as usize].next = next,
            None => self.head[slot] = next,
        }
        let entry = &mut self.entries[index as usize];
        entry.next = None;
        entry.released = true;
        Some(index)
    }

    /// The note an event addressed by `(channel, key)` and an optional id
    /// means, without changing anything.
    fn find(&self, channel: i16, key: i16, daw_id: NoteId) -> Option<Idx> {
        let slot = self.slot(channel, key)?;
        let mut at = self.head[slot];
        if daw_id.is_some() {
            while let Some(index) = at {
                if self.entries[index as usize].daw_id == daw_id {
                    return Some(index);
                }
                at = self.entries[index as usize].next;
            }
            return self.head[slot];
        }
        at
    }

    /// Take the entry of the note that can best afford to lose it.
    ///
    /// A released note first — it is only waiting for a plugin to say it is
    /// finished, and stealing it costs a `NOTE_END` that nothing was going to
    /// act on. Failing that, the oldest sounding note, which is what a
    /// polyphonic instrument out of voices does.
    fn steal(&mut self) -> Idx {
        self.stolen += 1;
        let pick = |released: bool, entries: &[Entry]| {
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.live && e.released == released)
                .min_by_key(|(_, e)| e.serial)
                .map(|(index, _)| index as Idx)
        };
        let index = pick(true, &self.entries)
            .or_else(|| pick(false, &self.entries))
            // Every entry dead and the free list empty cannot happen, but
            // taking entry 0 is a better answer than a panic on the audio
            // thread.
            .unwrap_or(0);
        self.unlink(index);
        self.entries[index as usize].live = false;
        index
    }

    /// Take an entry out of its address's chain, wherever it sits.
    fn unlink(&mut self, index: Idx) {
        let entry = self.entries[index as usize];
        let Some(slot) = self.slot(entry.channel, entry.key) else {
            return;
        };
        let mut at = self.head[slot];
        let mut prev: Option<Idx> = None;
        while let Some(here) = at {
            if here == index {
                match prev {
                    Some(prev) => self.entries[prev as usize].next = entry.next,
                    None => self.head[slot] = entry.next,
                }
                return;
            }
            prev = at;
            at = self.entries[here as usize].next;
        }
    }

    fn slot(&self, channel: i16, key: i16) -> Option<usize> {
        let channel = usize::try_from(channel).ok()?;
        let key = usize::try_from(key).ok()?;
        (channel < CHANNELS && key < KEYS).then_some(channel * KEYS + key)
    }

    /// A sub-plugin was handed this note's note-on.
    ///
    /// Counted where the note is actually delivered, not where a wire branches:
    /// a branch that a gate later swallows would never be decremented, and the
    /// count would never reach zero.
    pub fn delivered(&mut self, id: i32) {
        if let Some(entry) = self.entry_mut(id) {
            entry.voices = entry.voices.saturating_add(1);
            entry.ever_delivered = true;
        }
    }

    /// A sub-plugin says it has finished with this note.
    pub fn finished(&mut self, id: i32) {
        if let Some(entry) = self.entry_mut(id) {
            entry.voices = entry.voices.saturating_sub(1);
        }
    }

    fn entry_mut(&mut self, id: i32) -> Option<&mut Entry> {
        let index = usize::try_from(id).ok()?;
        self.entries.get_mut(index).filter(|e| e.live)
    }

    /// Settle the block: report the notes that have ended and free what can be
    /// freed.
    ///
    /// A note is reported once, when the last sub-plugin holding it finishes —
    /// or at the end of the block it arrived in, if it reached none at all.
    /// The second case is not an error: every branch of the graph may have
    /// been gated shut, and the DAW is holding a voice for a note that made no
    /// sound. Telling it so is the honest answer.
    ///
    /// Reporting and freeing are separate because they can happen in either
    /// order. A one-shot whose envelope finishes while the key is still down
    /// ends before its note-off, and CLAP says nothing to forbid that; a note
    /// held into a reverb tail ends long after. The entry lives until the DAW
    /// has been told, the note-off has come through, and no plugin still holds
    /// it.
    pub fn end_block(&mut self, out: &mut Vec<Ended>) {
        for index in 0..self.entries.len() {
            let entry = self.entries[index];
            if !entry.live {
                continue;
            }
            // Either it has been somewhere and come back, or the block it
            // arrived in is over and it never went anywhere at all.
            let settled = entry.ever_delivered || self.arrived.contains(&(index as Idx));
            if !entry.reported && entry.voices == 0 && settled {
                if out.len() < out.capacity() {
                    out.push(Ended {
                        port: entry.port,
                        channel: entry.channel,
                        key: entry.key,
                        daw_id: entry.daw_id,
                    });
                }
                self.entries[index].reported = true;
            }
            let entry = self.entries[index];
            if entry.reported && entry.released && entry.voices == 0 {
                self.unlink(index as Idx);
                self.entries[index] = Entry::EMPTY;
                self.free.push(index as Idx);
            }
        }
        self.arrived.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(channel: i16, key: i16, id: Option<i32>) -> NoteEvent {
        NoteEvent::NoteOn {
            note_id: id,
            port: 0,
            channel,
            key,
            velocity: 1.0,
            sample_offset: 0,
        }
    }

    fn off(channel: i16, key: i16, id: Option<i32>) -> NoteEvent {
        NoteEvent::NoteOff {
            note_id: id,
            port: 0,
            channel,
            key,
            velocity: 0.0,
            sample_offset: 0,
        }
    }

    fn id_of(event: NoteEvent) -> Option<i32> {
        event.note_id()
    }

    #[test]
    fn a_note_gets_an_id_of_our_own_and_keeps_it_to_the_end() {
        let mut ledger = NoteLedger::new();
        let opened = id_of(ledger.translate(on(0, 60, None))).expect("an id was handed out");
        let closed = id_of(ledger.translate(off(0, 60, None)));
        assert_eq!(closed, Some(opened), "the note-off names the same note");
    }

    /// The case the whole chain exists for: CLAP's own example has the host
    /// send `NOTE_ON [.. 60, 184]` then `NOTE_OFF [.. 60, -1]` and expect the
    /// first voice released.
    #[test]
    fn a_note_off_with_no_id_releases_the_oldest_note_on_that_key() {
        let mut ledger = NoteLedger::new();
        let first = id_of(ledger.translate(on(0, 60, Some(184)))).unwrap();
        let second = id_of(ledger.translate(on(0, 60, Some(185)))).unwrap();
        assert_ne!(first, second, "overlapping notes get their own ids");

        assert_eq!(id_of(ledger.translate(off(0, 60, None))), Some(first));
        assert_eq!(id_of(ledger.translate(off(0, 60, None))), Some(second));
    }

    #[test]
    fn a_note_off_that_carries_an_id_picks_that_note_out_of_the_chain() {
        let mut ledger = NoteLedger::new();
        let first = id_of(ledger.translate(on(0, 60, Some(1)))).unwrap();
        let second = id_of(ledger.translate(on(0, 60, Some(2)))).unwrap();

        assert_eq!(id_of(ledger.translate(off(0, 60, Some(2)))), Some(second));
        assert_eq!(
            id_of(ledger.translate(off(0, 60, None))),
            Some(first),
            "the one left is the one left"
        );
    }

    #[test]
    fn the_same_key_on_two_channels_is_two_notes() {
        let mut ledger = NoteLedger::new();
        let a = id_of(ledger.translate(on(0, 60, None))).unwrap();
        let b = id_of(ledger.translate(on(1, 60, None))).unwrap();
        assert_ne!(a, b);
        assert_eq!(id_of(ledger.translate(off(1, 60, None))), Some(b));
        assert_eq!(id_of(ledger.translate(off(0, 60, None))), Some(a));
    }

    /// A note nothing played is still a note the DAW is holding a voice for.
    #[test]
    fn a_note_that_reached_no_plugin_is_reported_at_the_end_of_its_block() {
        let mut ledger = NoteLedger::new();
        ledger.translate(on(0, 60, Some(7)));
        let mut ended = Vec::with_capacity(8);
        ledger.end_block(&mut ended);
        assert_eq!(
            ended,
            vec![Ended {
                port: 0,
                channel: 0,
                key: 60,
                daw_id: Some(7),
            }]
        );
    }

    #[test]
    fn a_delivered_note_is_reported_when_the_last_plugin_finishes() {
        let mut ledger = NoteLedger::new();
        let id = id_of(ledger.translate(on(0, 60, None))).unwrap();
        ledger.delivered(id);
        ledger.delivered(id);

        let mut ended = Vec::with_capacity(8);
        ledger.end_block(&mut ended);
        assert!(ended.is_empty(), "two plugins still hold it");

        ledger.finished(id);
        ledger.end_block(&mut ended);
        assert!(ended.is_empty(), "one still does");

        ledger.finished(id);
        ledger.end_block(&mut ended);
        assert_eq!(ended.len(), 1, "and now nobody does");

        ledger.end_block(&mut ended);
        assert_eq!(ended.len(), 1, "reported once, not once per block");
    }

    /// A one-shot's envelope can finish while the key is still down. CLAP puts
    /// no ordering on `NOTE_END` against `NOTE_OFF`, and forbidding it would
    /// make drums impossible.
    #[test]
    fn a_note_can_end_before_it_is_released() {
        let mut ledger = NoteLedger::new();
        let id = id_of(ledger.translate(on(0, 60, None))).unwrap();
        ledger.delivered(id);
        ledger.finished(id);

        let mut ended = Vec::with_capacity(8);
        ledger.end_block(&mut ended);
        assert_eq!(ended.len(), 1, "reported as soon as the plugin is done");

        // Still findable, because the key is still down: the note-off has to
        // land on it rather than on nothing.
        assert_eq!(id_of(ledger.translate(off(0, 60, None))), Some(id));
    }

    #[test]
    fn an_id_comes_back_into_use_once_the_note_is_finished_with() {
        let mut ledger = NoteLedger::new();
        let first = id_of(ledger.translate(on(0, 60, None))).unwrap();
        ledger.translate(off(0, 60, None));
        let mut ended = Vec::with_capacity(8);
        ledger.end_block(&mut ended);

        let second = id_of(ledger.translate(on(0, 62, None))).unwrap();
        assert_eq!(second, first, "the slot was free and got used again");
    }

    /// Running out is a fault, not a crash. The note that can best afford to
    /// lose its entry is one already released — it is only waiting for a
    /// plugin to say it is finished.
    #[test]
    fn a_full_pool_steals_from_a_released_note_first() {
        let mut ledger = NoteLedger::new();
        // One released note holding an entry, then fill the rest with held
        // ones.
        let released = id_of(ledger.translate(on(0, 0, None))).unwrap();
        ledger.delivered(released);
        ledger.translate(off(0, 0, None));
        for key in 1..MAX_LIVE_NOTES as i16 {
            ledger.translate(on(0, key, None));
        }
        assert_eq!(ledger.stolen(), 0, "the pool is exactly full");

        let stolen = id_of(ledger.translate(on(1, 64, None))).unwrap();
        assert_eq!(ledger.stolen(), 1);
        assert_eq!(stolen, released, "the released note gave up its entry");
    }
}
