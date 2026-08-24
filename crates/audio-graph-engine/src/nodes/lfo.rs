use serde::{Deserialize, Serialize};

pub use crate::ir::Waveform;

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, RateSpec};
use crate::port::Port;

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

impl Lfo {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        "LFO".into()
    }
}

impl Lfo {
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let state = cx.lfo_state()?;
        let out = cx.alloc()?;
        cx.emit(Op::Lfo {
            out,
            state,
            waveform: self.waveform,
            rate: match self.rate {
                Rate::Hz(hz) => RateSpec::Hz(hz.max(0.0)),
                // Zero beats per cycle would be an infinitely fast LFO; treat
                // it as "does not move" rather than as NaN.
                Rate::Beats(beats) if beats > 0.0 => RateSpec::CyclesPerBeat(1.0 / beats),
                Rate::Beats(_) => RateSpec::CyclesPerBeat(0.0),
            },
            offset_phase: self.phase.rem_euclid(1.0),
            depth: self.depth,
            centre: self.offset,
        });
        cx.bind_output(0, out);
        Ok(())
    }
}
