//! Slot values over time within one block (ARCHITECTURE.md §9.2).
//!
//! Before M5 a slot had one value per block, because the only thing driving it
//! was the DAW's automation and that is what the DAW gives us. A node graph
//! changes that: an LFO has a value at every sample, and sending one point per
//! block turns a 4 Hz sweep into a staircase you can hear.
//!
//! Sending one point per *sample* is not the answer either. §9.2 records why:
//! CLAP's `clap_event_param_mod_t` is 56 bytes to carry 8 bytes of value, and
//! the format's own authors do not expect it to scale to audio rate. VST3 is no
//! better. So the graph runs as fast as it likes internally and the boundary is
//! quantised — 32 samples by default, and adjustable, because "block-sized" is
//! the one choice that cannot be fixed later without changing the structure.
//!
//! This type is the buffer that quantisation lives in. It is allocated once, at
//! activate, for the finest granularity on offer, so changing the sub-block
//! size while the DAW is running costs nothing and allocates nothing.

use crate::slots::SLOT_COUNT;

/// How many values one sub-block carries.
///
/// The 32 slots the DAW automates, then one lane per parameter the graph drives
/// directly (§14.12), then one per audio-side control the graph automates — a
/// delay time (§14.5) or a gain. One buffer
/// rather than three because they are produced by the same evaluator pass and
/// consumed by the same merge: the evaluator writes a lane exactly the way it
/// writes a slot, and nothing below the compiler has to know which is which.
///
/// The ranges are disjoint and fixed, which is what lets each consumer read
/// only its own: the sub-plugin adapter never sees a delay time or a gain, and
/// the audio half never sees a parameter.
pub const LANES: usize =
    SLOT_COUNT + wrapper_engine::MAX_GRAPH_PARAMS + wrapper_engine::MAX_AUDIO_LANES;

/// The finest sub-block the schedule is sized for.
pub const MIN_QUANTUM: u32 = 16;

/// What the user may choose. Powers of two only: the arithmetic is exact and
/// the boundaries line up with anything else that divides a block.
pub const QUANTUM_CHOICES: [u32; 4] = [16, 32, 64, 128];

/// The default of §9.2.
pub const DEFAULT_QUANTUM: u32 = 32;

/// Slot and graph-parameter values at each sub-block boundary of one process
/// call.
pub struct SlotSchedule {
    /// [`LANES`] values per sub-block, one sub-block after another.
    values: Vec<f64>,
    quantum: u32,
    /// Set by `begin`, valid until the next one.
    blocks: usize,
    frames: u32,
}

impl SlotSchedule {
    /// Allocate for the worst case: a full block cut into `MIN_QUANTUM` pieces.
    ///
    /// Sizing for the finest granularity rather than the current one is what
    /// makes [`set_quantum`][Self::set_quantum] free.
    pub fn new(max_block: u32, quantum: u32) -> SlotSchedule {
        let capacity = max_block.div_ceil(MIN_QUANTUM).max(1) as usize * LANES;
        SlotSchedule {
            values: vec![0.0; capacity],
            quantum: sanitise(quantum),
            blocks: 0,
            frames: 0,
        }
    }

    /// The largest number of sub-blocks any call can produce, for callers
    /// sizing their own buffers.
    pub fn max_blocks(&self) -> usize {
        self.values.len() / LANES
    }

    pub fn quantum(&self) -> u32 {
        self.quantum
    }

    /// Change the sub-block size. Allocation-free, so it is safe from the audio
    /// thread when the user moves the setting mid-playback.
    pub fn set_quantum(&mut self, quantum: u32) {
        self.quantum = sanitise(quantum);
    }

    /// Start a block of `frames` samples. Returns the number of sub-blocks.
    pub fn begin(&mut self, frames: u32) -> usize {
        self.frames = frames;
        // Never zero: a block of no samples still wants one boundary, so a
        // caller can write values without special-casing it.
        self.blocks = frames.div_ceil(self.quantum).max(1) as usize;
        self.blocks = self.blocks.min(self.max_blocks());
        self.blocks
    }

    pub fn blocks(&self) -> usize {
        self.blocks
    }

    /// How many samples the current block covers.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Where sub-block `index` starts, as a sample offset into the block.
    pub fn offset(&self, index: usize) -> u32 {
        (index as u32 * self.quantum).min(self.frames.saturating_sub(1))
    }

    /// How many samples sub-block `index` covers. The last one is short
    /// whenever the block size is not a multiple of the quantum.
    pub fn frames_of(&self, index: usize) -> u32 {
        let start = index as u32 * self.quantum;
        self.frames.saturating_sub(start).min(self.quantum)
    }

    /// Every row, one after another — the shape the audio half wants, because
    /// it walks chunks itself and picks a row per chunk (§14.9).
    pub fn rows(&self) -> &[f64] {
        &self.values[..self.blocks * LANES]
    }

    pub fn block(&self, index: usize) -> &[f64] {
        &self.values[index * LANES..(index + 1) * LANES]
    }

    pub fn block_mut(&mut self, index: usize) -> &mut [f64] {
        &mut self.values[index * LANES..(index + 1) * LANES]
    }

    /// Fill every sub-block with the same values — the shape a wrapper with no
    /// graph produces, and the one that reproduces the pre-M5 behaviour
    /// exactly.
    pub fn fill(&mut self, values: &[f64]) {
        let n = values.len().min(LANES);
        for index in 0..self.blocks {
            let block = self.block_mut(index);
            block[..n].copy_from_slice(&values[..n]);
            // Lanes the caller did not supply are graph-driven ones with no
            // graph running. Zeroing rather than leaving the last block's
            // values means a patch that stops driving a parameter stops
            // sending events for it, instead of repeating a stale one.
            block[n..].fill(0.0);
        }
    }
}

fn sanitise(quantum: u32) -> u32 {
    // A quantum that is not one of the offered sizes would still work, but
    // clamping keeps `max_blocks` an honest bound.
    quantum.clamp(MIN_QUANTUM, *QUANTUM_CHOICES.last().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_cut_into_whole_sub_blocks_plus_a_remainder() {
        let mut schedule = SlotSchedule::new(512, 32);
        assert_eq!(schedule.begin(100), 4);
        assert_eq!(schedule.offset(0), 0);
        assert_eq!(schedule.offset(3), 96);
        assert_eq!(schedule.frames_of(0), 32);
        assert_eq!(
            schedule.frames_of(3),
            4,
            "the last sub-block is the remainder"
        );
        assert_eq!(
            (0..4).map(|i| schedule.frames_of(i)).sum::<u32>(),
            100,
            "every sample must be covered exactly once"
        );
    }

    #[test]
    fn changing_the_quantum_never_needs_more_memory() {
        let mut schedule = SlotSchedule::new(512, 128);
        let capacity = schedule.max_blocks();
        schedule.set_quantum(16);
        assert_eq!(
            schedule.max_blocks(),
            capacity,
            "sized for the finest quantum from the start"
        );
        assert_eq!(schedule.begin(512), 32);
    }

    #[test]
    fn an_offset_never_points_past_the_block() {
        // A host is allowed to give us fewer samples than the maximum, and an
        // event at an offset past the end is a contract violation the
        // sub-plugin would be entitled to crash on.
        let mut schedule = SlotSchedule::new(512, 32);
        schedule.begin(8);
        for i in 0..schedule.blocks() {
            assert!(schedule.offset(i) < 8);
        }
    }

    #[test]
    fn filling_gives_every_sub_block_the_same_value() {
        let mut schedule = SlotSchedule::new(256, 32);
        schedule.begin(256);
        let mut values = vec![0.0; SLOT_COUNT];
        values[3] = 0.75;
        schedule.fill(&values);
        for i in 0..schedule.blocks() {
            assert_eq!(schedule.block(i)[3], 0.75);
        }
    }
}
