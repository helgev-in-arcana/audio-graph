//! Parameter rows for one audio block, cut at the parameter resolution.
//!
//! A block's parameter values travel as rows of lanes, one row every
//! `resolution` samples.
//!
//! One value per block is not enough: the DAW's own automation arrives that
//! way, but a node graph does not — an LFO has a value at every sample, and
//! sending one point per block turns a 4 Hz sweep into an audible staircase.
//!
//! One value per *sample* is on offer, but it is not a sensible default. Both
//! plugin formats carry parameter changes as events, and the events are large
//! relative to their payload: CLAP's `clap_event_param_mod_t` is 56 bytes to
//! carry 8 bytes of value, VST3 is no better, and neither format's authors
//! expect this to scale to audio rate. So the resolution is the caller's
//! trade between smoothness and event traffic.
//!
//! The resolution says nothing about how audio is chunked. A row is where a
//! parameter value is known; how often a sub-plugin is called is the caller's
//! business, and the two are allowed to differ in either direction.
//!
//! Buffers are preallocated for the finest resolution ([`MIN_RESOLUTION`]) so
//! it can be changed during playback without real-time allocation.

/// Finest parameter resolution in samples, and the one the schedule is sized
/// for.
///
/// One sample: a row per sample of the largest block, which for 112 lanes and
/// a 4096-sample block is about 3.7 MB. Sizing for anything coarser would make
/// the finest setting the one that cannot be chosen during playback.
pub const MIN_RESOLUTION: u32 = 1;

/// Supported parameter resolutions in samples. Powers of two only: the
/// arithmetic is exact and the boundaries line up with anything else that
/// divides a block, audio chunks included.
pub const RESOLUTION_CHOICES: [u32; 6] = [1, 4, 16, 32, 64, 128];

/// Default parameter resolution in samples: about 0.67 ms at 48 kHz, fine
/// enough that a sweep reads as a sweep and coarse enough that a moving value
/// costs a sub-plugin one event per row rather than one per sample.
pub const DEFAULT_RESOLUTION: u32 = 32;

/// Rows of parameter lane values across one audio block.
pub struct SlotSchedule {
    /// Number of parameter lanes (slots plus direct graph parameters) per row.
    ///
    /// A caller's number, not this crate's: the wrapper packs its own slots,
    /// the parameters its graph drives directly and any audio-side control it
    /// automates into one buffer, because they are produced by the same pass
    /// and consumed by the same merge. What matters on this side is only that
    /// the ranges are disjoint and fixed, so each consumer reads its own and
    /// no other.
    lanes: usize,
    /// Contiguous storage for scheduled values (`lanes` per row), with the
    /// end row after the last row the capacity allows.
    values: Vec<f64>,
    resolution: u32,
    /// Number of rows in the current audio block, set by [`begin`][Self::begin].
    rows: usize,
    frames: u32,
    max_frames: u32,
    /// Whether the end row has been written since [`begin`][Self::begin].
    end_written: bool,
}

/// Read-only view of the rows prepared for one audio block.
///
/// The view borrows only for the duration of the audio call. The caller drops
/// it before the next parameter stage updates the schedule rows.
#[derive(Clone, Copy)]
pub struct ScheduleView<'a> {
    values: &'a [f64],
    end: Option<&'a [f64]>,
    lanes: usize,
    rows: usize,
    resolution: u32,
    frames: u32,
}

impl ScheduleView<'_> {
    pub fn lanes(&self) -> usize {
        self.lanes
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    pub fn resolution(&self) -> u32 {
        self.resolution
    }

    pub fn frames(&self) -> u32 {
        self.frames
    }

    pub fn offset(&self, index: usize) -> u32 {
        (index as u32 * self.resolution).min(self.frames.saturating_sub(1))
    }

    /// The row in force at sample `offset` of the block.
    ///
    /// Clamped to the last row, so a caller asking about the end of the block
    /// hears the value that was in force there rather than nothing.
    pub fn row_at(&self, offset: u32) -> usize {
        ((offset / self.resolution) as usize).min(self.rows.saturating_sub(1))
    }

    pub fn row(&self, index: usize) -> &[f64] {
        &self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    pub fn rows(&self) -> &[f64] {
        self.values
    }

    /// The lanes' values at the block's last sample boundary, `frames`, where
    /// the next block's first row starts — when the caller worked them out.
    ///
    /// What lets a consumer that joins points with lines draw the last row's
    /// line to where the value is going, rather than holding it flat until
    /// the next block and stepping there.
    pub fn end_row(&self) -> Option<&[f64]> {
        self.end
    }
}

impl<'a> ScheduleView<'a> {
    pub fn from_parts(
        values: &'a [f64],
        lanes: usize,
        rows: usize,
        resolution: u32,
        frames: u32,
    ) -> Result<Self, &'static str> {
        if resolution == 0
            || (rows != frames.div_ceil(resolution).max(1) as usize && (frames != 0 || rows != 0))
            || rows.checked_mul(lanes) != Some(values.len())
        {
            return Err("inconsistent schedule shape");
        }
        Ok(Self {
            values,
            end: None,
            lanes,
            rows,
            resolution,
            frames,
        })
    }

    /// The same view, with the values at the end of the block as well. See
    /// [`end_row`][Self::end_row].
    pub fn with_end(mut self, end: Option<&'a [f64]>) -> Result<Self, &'static str> {
        if end.is_some_and(|end| end.len() != self.lanes) {
            return Err("inconsistent schedule shape");
        }
        self.end = end;
        Ok(self)
    }
}

impl SlotSchedule {
    /// Creates a new schedule buffer preallocated for the worst case: a full
    /// `max_block` cut into [`MIN_RESOLUTION`] rows.
    ///
    /// Sizing for the finest resolution rather than the current one is what
    /// makes [`set_resolution`][Self::set_resolution] allocation-free.
    pub fn new(
        lanes: usize,
        max_block: u32,
        resolution: u32,
    ) -> Result<SlotSchedule, &'static str> {
        // One more than the rows, for the end row.
        let capacity = (max_block.div_ceil(MIN_RESOLUTION).max(1) as usize + 1)
            .checked_mul(lanes)
            .filter(|n| *n <= isize::MAX as usize / size_of::<f64>())
            .ok_or("schedule capacity overflow")?;
        Ok(SlotSchedule {
            lanes,
            values: vec![0.0; capacity],
            resolution: sanitise(resolution),
            rows: 0,
            frames: 0,
            max_frames: max_block,
            end_written: false,
        })
    }

    /// Returns the number of parameter lanes per row.
    pub fn lanes(&self) -> usize {
        self.lanes
    }

    /// Returns the maximum number of rows the preallocated buffer can store,
    /// for callers sizing their own buffers.
    pub fn max_rows(&self) -> usize {
        self.max_frames.div_ceil(MIN_RESOLUTION).max(1) as usize
    }

    pub fn max_frames(&self) -> u32 {
        self.max_frames
    }

    pub fn resolution(&self) -> u32 {
        self.resolution
    }

    /// Updates the parameter resolution. Allocation-free, so it is safe from
    /// the audio thread when the user moves the setting mid-playback.
    pub fn set_resolution(&mut self, resolution: u32) {
        self.resolution = sanitise(resolution);
        if self.rows != 0 {
            self.rows = self.frames.div_ceil(self.resolution).max(1) as usize;
        }
    }

    /// Initializes the schedule for an audio block of `frames` samples and
    /// returns the row count.
    pub fn begin(&mut self, frames: u32) -> Result<usize, &'static str> {
        self.end_written = false;
        if frames > self.max_frames {
            self.frames = 0;
            self.rows = 0;
            return Err("block exceeds schedule capacity");
        }
        self.frames = frames;
        // Never zero: a block of no samples still wants one boundary, so a
        // caller can write values without special-casing it.
        self.rows = frames.div_ceil(self.resolution).max(1) as usize;
        Ok(self.rows)
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Returns the total frame count for the current block.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Returns the sample offset where the given row begins.
    pub fn offset(&self, index: usize) -> u32 {
        (index as u32 * self.resolution).min(self.frames.saturating_sub(1))
    }

    /// Returns the number of frames the given row covers. The last one is
    /// short whenever the block size is not a multiple of the resolution.
    pub fn frames_of(&self, index: usize) -> u32 {
        let start = index as u32 * self.resolution;
        self.frames.saturating_sub(start).min(self.resolution)
    }

    /// Returns a flat slice of all active rows, one after another — the shape
    /// the audio half wants, because it walks chunks itself and picks the rows
    /// each chunk covers.
    pub fn rows(&self) -> &[f64] {
        &self.values[..self.rows * self.lanes]
    }

    pub fn view(&self) -> ScheduleView<'_> {
        ScheduleView {
            values: self.rows(),
            end: self.end_row(),
            lanes: self.lanes,
            rows: self.rows,
            resolution: self.resolution,
            frames: self.frames,
        }
    }

    pub fn row(&self, index: usize) -> &[f64] {
        &self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    pub fn row_mut(&mut self, index: usize) -> &mut [f64] {
        &mut self.values[index * self.lanes..(index + 1) * self.lanes]
    }

    /// The values at the end of the block, if they were written since
    /// [`begin`][Self::begin]. See [`ScheduleView::end_row`].
    pub fn end_row(&self) -> Option<&[f64]> {
        let at = self.max_rows() * self.lanes;
        self.end_written.then(|| &self.values[at..at + self.lanes])
    }

    /// The end row, to write. Handing it out counts as writing it: a caller
    /// that asks for it and leaves it alone has said the block ends on the
    /// values the row held before, which for a fresh block is all zeros.
    pub fn end_row_mut(&mut self) -> &mut [f64] {
        self.end_written = true;
        let at = self.max_rows() * self.lanes;
        &mut self.values[at..at + self.lanes]
    }

    /// Fills all rows with uniform parameter values — the shape a wrapper with
    /// no graph running produces.
    pub fn fill(&mut self, values: &[f64]) {
        let n = values.len().min(self.lanes);
        let rows = (0..self.rows).map(|index| index * self.lanes);
        let end = self.max_rows() * self.lanes;
        self.end_written = true;
        for at in rows.chain([end]) {
            let row = &mut self.values[at..at + self.lanes];
            row[..n].copy_from_slice(&values[..n]);
            // Lanes the caller did not supply are graph-driven ones with no
            // graph running. Zeroing rather than leaving the last block's
            // values means a patch that stops driving a parameter stops
            // sending events for it, instead of repeating a stale one.
            row[n..].fill(0.0);
        }
    }
}

fn sanitise(resolution: u32) -> u32 {
    // A resolution that is not one of the offered sizes would still work, but
    // clamping keeps `max_rows` an honest bound.
    resolution.clamp(MIN_RESOLUTION, *RESOLUTION_CHOICES.last().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rejected blocks expose no partial rows, and an empty lane set requires no allocation.
    #[test]
    fn capacity_and_view_shape_are_checked() {
        let mut schedule = SlotSchedule::new(1, 16, 16).unwrap();
        assert!(schedule.begin(64).is_err());
        assert_eq!(
            (
                schedule.row_count(),
                schedule.frames(),
                schedule.rows().len()
            ),
            (0, 0, 0)
        );
        assert_eq!(schedule.begin(16), Ok(1));
        assert!(SlotSchedule::new(usize::MAX, 16, 16).is_err());
        assert!(ScheduleView::from_parts(&[], 1, 1, 16, 16).is_err());
        assert!(ScheduleView::from_parts(&[0.0], 1, 1, 0, 16).is_err());
        assert!(ScheduleView::from_parts(&[0.0], 1, 1, 16, 64).is_err());
        let mut empty = SlotSchedule::new(0, 64, 16).unwrap();
        assert_eq!(empty.begin(64), Ok(4));
        assert!(empty.view().row(3).is_empty());
    }

    /// Test constants for slot and lane counts.
    const SLOTS: usize = 32;
    const LANES: usize = SLOTS + 64 + 16;

    #[test]
    fn a_block_is_cut_into_whole_rows_plus_a_remainder() {
        let mut schedule = SlotSchedule::new(LANES, 512, 32).unwrap();
        assert_eq!(schedule.begin(100).unwrap(), 4);
        assert_eq!(schedule.offset(0), 0);
        assert_eq!(schedule.offset(3), 96);
        assert_eq!(schedule.frames_of(0), 32);
        assert_eq!(schedule.frames_of(3), 4, "the last row is the remainder");
        assert_eq!(
            (0..4).map(|i| schedule.frames_of(i)).sum::<u32>(),
            100,
            "every sample must be covered exactly once"
        );
    }

    /// Every offered resolution, the finest included, fits the memory
    /// allocated up front.
    #[test]
    fn changing_the_resolution_never_needs_more_memory() {
        let mut schedule = SlotSchedule::new(LANES, 512, 128).unwrap();
        let capacity = schedule.max_rows();
        for resolution in RESOLUTION_CHOICES {
            schedule.set_resolution(resolution);
            assert_eq!(schedule.max_rows(), capacity);
            assert_eq!(
                schedule.begin(512).unwrap(),
                512usize.div_ceil(resolution as usize)
            );
        }
        schedule.set_resolution(1);
        assert_eq!(schedule.row_count(), 512, "a row per sample");
        schedule.set_resolution(64);
        assert_eq!(schedule.row_count(), 8);
        let view = schedule.view();
        assert!(
            ScheduleView::from_parts(
                view.rows(),
                view.lanes(),
                view.row_count(),
                view.resolution(),
                view.frames()
            )
            .is_ok()
        );
    }

    #[test]
    fn an_offset_never_points_past_the_block() {
        // A host is allowed to give us fewer samples than the maximum, and an
        // event at an offset past the end is a contract violation the
        // sub-plugin would be entitled to crash on.
        let mut schedule = SlotSchedule::new(LANES, 512, 32).unwrap();
        schedule.begin(8).unwrap();
        for i in 0..schedule.row_count() {
            assert!(schedule.offset(i) < 8);
        }
    }

    /// A sample belongs to the row that started at or before it.
    #[test]
    fn a_sample_reads_the_row_it_falls_in() {
        let mut schedule = SlotSchedule::new(1, 512, 32).unwrap();
        schedule.begin(100).unwrap();
        let view = schedule.view();
        assert_eq!(view.row_at(0), 0);
        assert_eq!(view.row_at(31), 0);
        assert_eq!(view.row_at(32), 1);
        assert_eq!(view.row_at(99), 3);
        assert_eq!(view.row_at(100), 3, "the end of the block is the last row");
    }

    /// The end of the block is unknown until someone says what it is, and
    /// every block starts without knowing it again.
    #[test]
    fn the_end_row_is_only_there_once_written() {
        let mut schedule = SlotSchedule::new(2, 64, 1).unwrap();
        schedule.begin(64).unwrap();
        assert!(schedule.view().end_row().is_none());
        schedule.row_mut(63).copy_from_slice(&[0.1, 0.2]);
        schedule.end_row_mut().copy_from_slice(&[0.3, 0.4]);
        assert_eq!(schedule.row(63), &[0.1, 0.2], "the last row is its own");
        assert_eq!(schedule.view().end_row(), Some(&[0.3, 0.4][..]));
        schedule.begin(64).unwrap();
        assert!(schedule.end_row().is_none());
        schedule.fill(&[0.5]);
        assert_eq!(schedule.end_row(), Some(&[0.5, 0.0][..]));
    }

    #[test]
    fn filling_gives_every_row_the_same_value() {
        let mut schedule = SlotSchedule::new(LANES, 256, 32).unwrap();
        schedule.begin(256).unwrap();
        let mut values = vec![0.0; SLOTS];
        values[3] = 0.75;
        schedule.fill(&values);
        for i in 0..schedule.row_count() {
            assert_eq!(schedule.row(i)[3], 0.75);
        }
    }
}
