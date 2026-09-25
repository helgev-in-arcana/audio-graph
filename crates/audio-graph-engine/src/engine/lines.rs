// ============================================================================
//
// HUMAN REVIEW REQUIRED: THIS FILE HAS NOT BEEN REVIEWED BY A HUMAN.
//
// ============================================================================

//! State that belongs to a node and outlives the program it was compiled into.
//!
//! An LFO's phase, a delay line's ring, a latch: each sits at an index the
//! program chose, and a recompile may choose another. Each kind is one value
//! per slot, and [`reorder`] moves whole slots, so nothing kept about a slot
//! can be left behind at the index it used to occupy.

use super::*;

/// Move a delay line's contents into a differently sized ring.
///
/// The samples that matter are the most recent ones, so the copy walks
/// backwards from the old head and lands them at the end of the new ring.
/// Anything that no longer fits is the oldest of it, which is the part a
/// shorter line was never going to read again anyway.
///
/// `head` comes in pointing into the old ring and goes out pointing into the
/// new one.
pub(super) fn copy_ring(
    from: &[f32],
    from_len: usize,
    to: &mut [f32],
    to_len: usize,
    head: &mut usize,
) {
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

/// One slot of state that follows its node across a program swap.
pub(super) trait Slot {
    fn node(&self) -> u32;
    fn set_node(&mut self, node: u32);
    /// Start over, for a node the previous program did not have.
    fn clear(&mut self);
}

/// One parameter delay line: `MAX_DELAY_TAPS` sub-blocks of history.
pub(super) struct ParamLine {
    pub(super) ring: Vec<f64>,
    pub(super) head: usize,
    node: u32,
}

impl ParamLine {
    pub(super) fn new() -> ParamLine {
        ParamLine {
            ring: vec![0.0; MAX_DELAY_TAPS],
            head: 0,
            node: u32::MAX,
        }
    }
}

impl Slot for ParamLine {
    fn node(&self) -> u32 {
        self.node
    }
    fn set_node(&mut self, node: u32) {
        self.node = node;
    }
    fn clear(&mut self) {
        self.ring.fill(0.0);
        self.head = 0;
    }
}

/// One audio delay line.
///
/// The ring is allocated on the main thread and carried in on the program,
/// because this thread may not allocate and only that side knows both the
/// graph's `max_time` and the sample rate.
pub(super) struct AudioLine {
    pub(super) ring: Vec<f32>,
    /// Samples per channel in `ring`, or zero while the line has none. It
    /// decides whether a ring handed over with the program replaces this one.
    pub(super) len: usize,
    pub(super) head: usize,
    node: u32,
}

impl AudioLine {
    pub(super) fn new() -> AudioLine {
        AudioLine {
            ring: Vec::new(),
            len: 0,
            head: 0,
            node: u32::MAX,
        }
    }
}

impl Slot for AudioLine {
    fn node(&self) -> u32 {
        self.node
    }
    fn set_node(&mut self, node: u32) {
        self.node = node;
    }
    /// Emptied where it stands. `len` still describes the ring, which a new
    /// line either reuses or has replaced by one of its own.
    fn clear(&mut self) {
        self.ring.fill(0.0);
        self.head = 0;
    }
}

/// One LFO.
pub(super) struct Lfo {
    /// 0..1.
    pub(super) phase: f64,
    /// The sample-and-hold value.
    pub(super) hold: f64,
    node: u32,
}

impl Lfo {
    pub(super) fn new() -> Lfo {
        Lfo {
            phase: 0.0,
            hold: 0.0,
            node: u32::MAX,
        }
    }
}

impl Slot for Lfo {
    fn node(&self) -> u32 {
        self.node
    }
    fn set_node(&mut self, node: u32) {
        self.node = node;
    }
    fn clear(&mut self) {
        self.phase = 0.0;
        self.hold = 0.0;
    }
}

/// One latch, and the state of the ops that keep a single value between
/// blocks the same way (a fade's ramp, a follower's level).
pub(super) struct Latch {
    /// NaN for a latch nothing has set yet.
    pub(super) value: f64,
    node: u32,
}

impl Latch {
    pub(super) fn new() -> Latch {
        Latch {
            value: f64::NAN,
            node: u32::MAX,
        }
    }
}

impl Slot for Latch {
    fn node(&self) -> u32 {
        self.node
    }
    fn set_node(&mut self, node: u32) {
        self.node = node;
    }
    fn clear(&mut self) {
        self.value = f64::NAN;
    }
}

/// Move each slot to the index the new program gave its node, contents intact.
///
/// Work out the permutation first, then apply it by swapping whole slots: for
/// a ring that moves the outer `Vec` rather than its contents, which for an
/// audio line is 96 000 samples a channel. A slot whose node is new starts
/// over; one past the new program's count keeps its memory and forgets its
/// node, which no program will name.
pub(super) fn reorder<S: Slot>(slots: &mut [S], order: &mut [usize], want: &[u32]) {
    let lines = want.len().min(slots.len());
    for (i, slot) in order[..lines].iter_mut().enumerate() {
        *slot = slots
            .iter()
            .position(|s| s.node() == want[i])
            .unwrap_or(NOT_PRESENT);
    }

    // Move the surviving slots into place first. Clearing as we went would
    // wipe a slot that is still sitting where some later one wants it.
    for i in 0..lines {
        let from = order[i];
        // `from` is never below `i`: slots below `i` already hold earlier
        // nodes, which are all different from this one.
        if from == NOT_PRESENT || from == i {
            continue;
        }
        slots.swap(i, from);
        // Whatever was at `i` now sits at `from`; a slot still pointing at `i`
        // has to follow it there.
        for slot in order[i + 1..lines].iter_mut() {
            if *slot == i {
                *slot = from;
            }
        }
        order[i] = i;
    }
    // Whatever is left in a new node's slot belonged to a node that is gone.
    for i in 0..lines {
        if order[i] == NOT_PRESENT {
            slots[i].clear();
        }
        slots[i].set_node(want[i]);
    }
    for slot in slots[lines..].iter_mut() {
        slot.set_node(u32::MAX);
    }
}
