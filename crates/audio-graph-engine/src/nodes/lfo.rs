use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
pub use crate::ir::Waveform;
use crate::ir::{Op, RateSpec};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, rate_control};
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

impl Node for Lfo {
    fn title(&self) -> String {
        "LFO".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let state = cx.lfo_state()?;
        let out = cx.alloc()?;
        cx.emit(Op::Lfo {
            out,
            state,
            waveform: self.waveform,
            rate: match self.rate {
                Rate::Hz(hz) => RateSpec::Hz(hz.max(0.0)),
                // Zero beats per cycle is treated as a stationary (zero frequency) LFO.
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

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = combo(
            ui,
            "wave",
            &mut self.waveform,
            &Waveform::ALL,
            Waveform::label,
        );
        changed |= rate_control(ui, &mut self.rate);
        ui.horizontal(|ui| {
            ui.label("phase");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.phase)
                        .speed(0.01)
                        .range(0.0..=1.0),
                )
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("depth");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.depth)
                        .speed(0.01)
                        .range(-1.0..=1.0),
                )
                .changed();
            ui.label("centre");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.offset)
                        .speed(0.01)
                        .range(-1.0..=1.0),
                )
                .changed();
        });
        changed
    }
}

#[cfg(feature = "ui")]
impl Lfo {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Lfo)> {
        vec![(
            "LFO",
            Lfo {
                waveform: Waveform::Sine,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                // Default depth and offset swing across the normalized [0.0, 1.0] range.
                depth: 0.5,
                offset: 0.5,
            },
        )]
    }
}
