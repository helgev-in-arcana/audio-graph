use serde::{Deserialize, Serialize};

pub use crate::ir::Waveform;

/// A free-running or tempo-synced oscillator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lfo {
    pub waveform: Waveform,
    pub rate: Rate,
    /// Starting phase, 0..1.
    pub phase: f64,
    /// Half the peak-to-peak swing.
    pub depth: f64,
    /// Centre of the swing. `depth 0.5 / offset 0.5` fills 0..1.
    pub offset: f64,
}

/// How fast an LFO runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Rate {
    Hz(f64),
    /// One cycle per this many beats, following the host's tempo.
    Beats(f64),
}
