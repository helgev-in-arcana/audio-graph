use super::KeyTrigger;
use serde::{Deserialize, Serialize};

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
/// Sixty-four velocity bands cover twelve keys with several choices while bounding event work.
pub const MAX_GRANULAR_BINDINGS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GranularMode {
    #[default]
    Hold,
    Select,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum GranularAction {
    Record,
    Reset,
    Play,
    Stop,
    Size(f64),
    Interval(f64),
    Position(f64),
    Wet(f64),
    InvertLoop,
    Update(f64),
    History(f64),
    Block(f64),
}

impl GranularAction {
    pub(crate) fn parameter(self) -> Option<(usize, f64)> {
        Some(match self {
            Self::Size(v) => (0, v),
            Self::Interval(v) => (1, v),
            Self::Position(v) => (2, v),
            Self::Wet(v) => (3, v),
            Self::Update(v) => (5, v),
            Self::History(v) => (6, v),
            Self::Block(v) => (7, v),
            _ => return None,
        })
    }

    pub fn valid(self) -> bool {
        match self.parameter() {
            None => true,
            Some((target, value)) => {
                value.is_finite()
                    && match target {
                        0 | 1 => (0.05..=1.0).contains(&value),
                        2..=5 => (0.0..=1.0).contains(&value),
                        _ => value > 0.0 && value <= 64.0,
                    }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GranularBinding {
    pub trigger: KeyTrigger,
    pub action: GranularAction,
    pub mode: GranularMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GranularStatus {
    pub history_seconds: f64,
    pub recorded_seconds: f64,
    pub recording: bool,
    pub playing: bool,
    pub selected: [Option<f64>; 8],
    pub revisions: [u64; 8],
    pub loop_inverted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GranularSpec {
    pub block_beats: f64,
    pub history_beats: f64,
    pub looping: bool,
    pub update: f64,
    pub bindings: Vec<GranularBinding>,
    pub revisions: [u64; 8],
}
