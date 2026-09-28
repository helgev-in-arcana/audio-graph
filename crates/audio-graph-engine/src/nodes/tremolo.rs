use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, RateSpec, Waveform};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, decimals, fallback, rate_control};
use crate::nodes::{Node, Rate};
use crate::port::{Port, PortType};

/// The waveforms a tremolo offers. A square wave steps the gain and a random
/// one steps it at random; both click, so neither is on the list.
#[cfg(feature = "ui")]
const SHAPES: [Waveform; 3] = [Waveform::Sine, Waveform::Triangle, Waveform::Saw];

/// A gain that swings periodically: a tremolo.
///
/// What an LFO wired into a gain almost is, and why it is a node of its own:
/// a parameter is known only at row boundaries, and at a tremolo's rate a
/// gain drawn in straight lines between them buzzes at every corner. Here the
/// oscillator runs at the sample rate. See [`AudioOp::Tremolo`].
///
/// The depth has a socket, so what turns the tremolo on can be anything the
/// graph makes — a key switch, a velocity, an envelope — and a change of it is
/// ramped rather than stepped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tremolo {
    pub channels: u16,
    pub waveform: Waveform,
    pub rate: Rate,
    /// How far the gain dips, 0..1: at 1 it reaches silence once a cycle, at
    /// 0 the node passes the signal unchanged.
    pub depth: f64,
}

impl Node for Tremolo {
    fn title(&self) -> String {
        "Tremolo".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![
            Port::new(
                "in",
                PortType::Audio {
                    channels: self.channels,
                },
            ),
            Port::param("depth"),
        ]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new(
            "out",
            PortType::Audio {
                channels: self.channels,
            },
        )]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        if let Some(reg) = cx.input(1) {
            cx.drive_audio(1, reg)?;
        }
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let readers = cx.readers();
        // Booked before anything that can return, so the phase belongs to the
        // node whether or not anything is wired into it yet.
        let state = cx.dsp_state()?;
        let Some((a, late)) = cx.source_at_socket_width(0)? else {
            let out = cx.alloc(self.channels, readers)?;
            cx.emit(AudioOp::Silence { out });
            cx.produce(0, out, 0);
            return Ok(());
        };
        cx.consume(a);
        let out = cx.alloc(self.channels, readers)?;
        cx.emit(AudioOp::Tremolo {
            out,
            a,
            state,
            lane: cx.lane(1),
            depth: self.depth,
            waveform: self.waveform,
            rate: match self.rate {
                Rate::Hz(hz) => RateSpec::Hz(hz.max(0.0)),
                Rate::Beats(beats) if beats > 0.0 => RateSpec::CyclesPerBeat(1.0 / beats),
                Rate::Beats(_) => RateSpec::CyclesPerBeat(0.0),
            },
        });
        cx.produce(0, out, late);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = combo(ui, "wave", &mut self.waveform, &SHAPES, Waveform::label);
        changed |= rate_control(ui, &mut self.rate);
        changed
    }

    /// The depth, on the row of the socket that can drive it instead.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        cx: &mut NodeUi<'_>,
    ) -> bool {
        if port != 1 {
            return false;
        }
        fallback(
            ui,
            connected,
            cx.input(port),
            &mut self.depth,
            |ui, depth| {
                ui.add(
                    egui::DragValue::new(depth)
                        .speed(0.01)
                        .range(0.0..=1.0)
                        .fixed_decimals(decimals::PLAIN),
                )
                .changed()
            },
        )
    }
}

#[cfg(feature = "ui")]
impl Tremolo {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Tremolo)> {
        vec![(
            "Tremolo",
            Tremolo {
                channels: 2,
                waveform: Waveform::Sine,
                rate: Rate::Hz(5.0),
                depth: 0.5,
            },
        )]
    }
}
