//! Sub-block parameter scheduling and value buffer management.
//!
//! Parameter automation is quantized into sub-blocks (default 32 samples)
//! to provide sample-accurate automation without generating per-sample event overhead.
//! Buffers are preallocated for the finest supported granularity ([`MIN_QUANTUM`])
//! so the quantum can be adjusted during playback without real-time allocation.

/// Minimum sub-block quantum size in samples.
pub const MIN_QUANTUM: u32 = 16;

/// Supported sub-block quantum sizes in samples (powers of two).
pub const QUANTUM_CHOICES: [u32; 4] = [16, 32, 64, 128];

/// Default sub-block quantum size in samples.
pub const DEFAULT_QUANTUM: u32 = 32;

/// Schedule buffer storing parameter lane values across sub-block intervals for an audio block.
pub struct SlotSchedule {
    /// Number of parameter lanes (slots plus direct graph parameters) per sub-block.
    lanes: usize,
    /// Contiguous storage for scheduled values (`lanes` per sub-block).
    values: Vec<f64>,
    quantum: u32,
    /// Number of sub-blocks in the current audio block, initialized by [`begin`][Self::begin].
    blocks: usize,
    frames: u32,
}

impl SlotSchedule {
    /// Creates a new schedule buffer preallocated for `max_block` at [`MIN_QUANTUM`] granularity.
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

    /// Returns the maximum number of sub-blocks the preallocated buffer can store.
    pub fn max_blocks(&self) -> usize {
        self.values.len() / self.lanes
    }

    pub fn quantum(&self) -> u32 {
        self.quantum
    }

    /// Updates the sub-block quantum size without allocating memory.
    pub fn set_quantum(&mut self, quantum: u32) {
        self.quantum = sanitise(quantum);
    }

    /// Initializes the schedule for an audio block of `frames` samples and returns the sub-block count.
    pub fn begin(&mut self, frames: u32) -> usize {
        self.frames = frames;
        // Ensure at least one sub-block even for empty blocks.
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

    /// Returns the number of frames contained in the specified sub-block.
    pub fn frames_of(&self, index: usize) -> u32 {
        let start = index as u32 * self.quantum;
        self.frames.saturating_sub(start).min(self.quantum)
    }

    /// Returns a flat slice of all active sub-block rows.
    pub fn rows(&self) -> &[f64] {
        &self.values[..self.blocks * self.lanes]
    }

    pub fn block(&self, index: usize) -> &[f64] {
        &self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    pub fn block_mut(&mut self, index: usize) -> &mut [f64] {
        &mut self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    /// Fills all sub-blocks with uniform parameter values, zeroing unsupplied lanes.
    pub fn fill(&mut self, values: &[f64]) {
        let n = values.len().min(self.lanes);
        for index in 0..self.blocks {
            let block = self.block_mut(index);
            block[..n].copy_from_slice(&values[..n]);
            // Zero remaining lanes not covered by the input slice to prevent stale values.
            block[n..].fill(0.0);
        }
    }
}

fn sanitise(quantum: u32) -> u32 {
    // Clamp quantum to valid bounds.
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
        // Sub-block offsets must strictly precede the total frame count.
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
