//! Sub-block parameter scheduling and value buffer management.
//!
//! Parameter automation is quantized into sub-blocks (default 32 samples).
//!
//! One value per block is not enough: the DAW's own automation arrives that
//! way, but a node graph does not — an LFO has a value at every sample, and
//! sending one point per block turns a 4 Hz sweep into an audible staircase.
//!
//! One value per *sample* is not the answer either. Both plugin formats carry
//! parameter changes as events, and the events are large relative to their
//! payload: CLAP's `clap_event_param_mod_t` is 56 bytes to carry 8 bytes of
//! value, VST3 is no better, and neither format's authors expect this to scale
//! to audio rate. So the graph runs as fast as it likes internally and only the
//! boundary is quantised.
//!
//! Buffers are preallocated for the finest supported granularity
//! ([`MIN_QUANTUM`]) so the quantum can be adjusted during playback without
//! real-time allocation.

/// Minimum sub-block quantum size in samples — the granularity the schedule
/// is sized for.
pub const MIN_QUANTUM: u32 = 16;

/// Supported sub-block quantum sizes in samples. Powers of two only: the
/// arithmetic is exact and the boundaries line up with anything else that
/// divides a block.
pub const QUANTUM_CHOICES: [u32; 4] = [16, 32, 64, 128];

/// Default sub-block quantum size in samples.
pub const DEFAULT_QUANTUM: u32 = 32;

/// Schedule buffer storing parameter lane values across sub-block intervals for an audio block.
pub struct SlotSchedule {
    /// Number of parameter lanes (slots plus direct graph parameters) per
    /// sub-block.
    ///
    /// A caller's number, not this crate's: the wrapper packs its own slots,
    /// the parameters its graph drives directly and any audio-side control it
    /// automates into one buffer, because they are produced by the same pass
    /// and consumed by the same merge. What matters on this side is only that
    /// the ranges are disjoint and fixed, so each consumer reads its own and
    /// no other.
    lanes: usize,
    /// Contiguous storage for scheduled values (`lanes` per sub-block).
    values: Vec<f64>,
    quantum: u32,
    /// Number of sub-blocks in the current audio block, initialized by [`begin`][Self::begin].
    blocks: usize,
    frames: u32,
}

impl SlotSchedule {
    /// Creates a new schedule buffer preallocated for the worst case: a full
    /// `max_block` cut into [`MIN_QUANTUM`] pieces.
    ///
    /// Sizing for the finest granularity rather than the current one is what
    /// makes [`set_quantum`][Self::set_quantum] allocation-free.
    pub fn new(lanes: usize, max_block: u32, quantum: u32) -> SlotSchedule {
        let capacity = max_block.div_ceil(MIN_QUANTUM).max(1) as usize * lanes;
        SlotSchedule {
            lanes,
            values: vec![0.0; capacity],
            quantum: sanitise(quantum),
            blocks: 0,
            frames: 0,
        }
    }

    /// Returns the number of parameter lanes per sub-block.
    pub fn lanes(&self) -> usize {
        self.lanes
    }

    /// Returns the maximum number of sub-blocks the preallocated buffer can
    /// store, for callers sizing their own buffers.
    pub fn max_blocks(&self) -> usize {
        self.values.len() / self.lanes
    }

    pub fn quantum(&self) -> u32 {
        self.quantum
    }

    /// Updates the sub-block quantum size. Allocation-free, so it is safe
    /// from the audio thread when the user moves the setting mid-playback.
    pub fn set_quantum(&mut self, quantum: u32) {
        self.quantum = sanitise(quantum);
    }

    /// Initializes the schedule for an audio block of `frames` samples and returns the sub-block count.
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

    /// Returns the total frame count for the current block.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Returns the sample offset where the given sub-block begins.
    pub fn offset(&self, index: usize) -> u32 {
        (index as u32 * self.quantum).min(self.frames.saturating_sub(1))
    }

    /// Returns the number of frames contained in the specified sub-block. The
    /// last one is short whenever the block size is not a multiple of the
    /// quantum.
    pub fn frames_of(&self, index: usize) -> u32 {
        let start = index as u32 * self.quantum;
        self.frames.saturating_sub(start).min(self.quantum)
    }

    /// Returns a flat slice of all active sub-block rows, one after another
    /// — the shape the audio half wants, because it walks chunks itself and
    /// picks a row per chunk.
    pub fn rows(&self) -> &[f64] {
        &self.values[..self.blocks * self.lanes]
    }

    pub fn block(&self, index: usize) -> &[f64] {
        &self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    pub fn block_mut(&mut self, index: usize) -> &mut [f64] {
        &mut self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    /// Fills all sub-blocks with uniform parameter values — the shape a
    /// wrapper with no graph running produces.
    pub fn fill(&mut self, values: &[f64]) {
        let n = values.len().min(self.lanes);
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

    /// Test constants for slot and lane counts.
    const SLOTS: usize = 32;
    const LANES: usize = SLOTS + 64 + 16;

    #[test]
    fn a_block_is_cut_into_whole_sub_blocks_plus_a_remainder() {
        let mut schedule = SlotSchedule::new(LANES, 512, 32);
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
        let mut schedule = SlotSchedule::new(LANES, 512, 128);
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
        let mut schedule = SlotSchedule::new(LANES, 512, 32);
        schedule.begin(8);
        for i in 0..schedule.blocks() {
            assert!(schedule.offset(i) < 8);
        }
    }

    #[test]
    fn filling_gives_every_sub_block_the_same_value() {
        let mut schedule = SlotSchedule::new(LANES, 256, 32);
        schedule.begin(256);
        let mut values = vec![0.0; SLOTS];
        values[3] = 0.75;
        schedule.fill(&values);
        for i in 0..schedule.blocks() {
            assert_eq!(schedule.block(i)[3], 0.75);
        }
    }
}
