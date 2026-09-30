use super::KeyTrigger;

/// Eight independent recordings fit a small layered patch without unbounded audio-thread state.
pub const MAX_GRANULARS: usize = 8;
/// At 192 kHz, ten seconds of stereo f32 recording occupies 15.36 MB per node.
pub const MAX_GRANULAR_SECONDS: f64 = 10.0;
/// A millisecond floor bounds slice metadata even at extreme tempos and beat divisions.
pub const MIN_GRANULAR_BLOCK_SECONDS: f64 = 0.001;
/// One marker per millisecond of history, plus the partially overwritten and current slices.
pub const MAX_GRANULAR_SLICES: usize = 10_002;
/// Sixty-four overlapping grains bound work when a tempo jump leaves long grains sounding.
pub const MAX_GRAINS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GranularSpec {
    pub block_beats: f64,
    pub history: f64,
    pub looping: bool,
    pub update: f64,
    pub latch: bool,
    pub keys: [KeyTrigger; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GranularParam {
    pub value: f64,
    pub lane: Option<u16>,
}
