use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, MixIn, Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Gain level in decibels when the gate is closed (-100 dB represents mute).
const CLOSED_DB: f64 = -100.0;
/// Gain level in decibels when the gate is open (0 dB represents unity gain).
const OPEN_DB: f64 = 0.0;

/// Gates an audio signal based on whether a parameter control value meets a threshold.
///
/// Passes audio through at unity gain (0 dB) when open, or silences it (-100 dB)
/// when closed. If the control input is unconnected, it defaults to zero (closed).
///
/// **Bug:** Gating a loud signal can click because the switch is hard, happening
/// at a chunk boundary. The proper fix, which is a ramp in the audio half,
/// is not implemented yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub channels: u16,
    /// The gate opens at this value and above — or below it, with `invert`.
    pub threshold: f64,
    pub invert: bool,
}

impl Node for Gate {
    fn title(&self) -> String {
        "Audio Gate".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![
            Port::new(
                "in",
                PortType::Audio {
                    channels: self.channels,
                },
            ),
            Port::param("control"),
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

    // Evaluates the gate state as a parameter and routes the resulting gain to the audio pass.
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let control = cx.input_or_zero(1)?;
        let (low, high) = if self.invert {
            (OPEN_DB, CLOSED_DB)
        } else {
            (CLOSED_DB, OPEN_DB)
        };
        let out = cx.alloc()?;
        cx.emit(Op::Select {
            out,
            control,
            threshold: self.threshold,
            low: Operand::Value(low),
            high: Operand::Value(high),
        });
        // Drive the audio lane associated with the gate control socket.
        cx.drive_audio(1, out)
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let readers = cx.readers();
        let Some((buf, late)) = cx.source_at_socket_width(0)? else {
            let out = cx.alloc(self.channels, readers)?;
            cx.emit(AudioOp::Silence { out });
            cx.produce(0, out, 0);
            return Ok(());
        };
        let lane = cx.lane(1);
        cx.consume(buf);
        // Allocate buffer and emit a mix operation scaling by the gated gain.
        // The gate may well reuse the input buffer as the destination, making
        // it an in-place scaling that costs no additional buffer.
        let out = cx.alloc(self.channels, readers)?;
        cx.emit(AudioOp::Mix {
            out,
            inputs: vec![MixIn {
                buf,
                lane,
                gain: 1.0,
            }],
        });
        cx.produce(0, out, late);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(if self.invert { "shut at" } else { "open at" });
            changed |= ui
                .add(egui::DragValue::new(&mut self.threshold).speed(0.01))
                .changed();
            changed |= ui
                .selectable_label(self.invert, "invert")
                .on_hover_text("pass while the control is below the threshold")
                .clicked()
                .then(|| self.invert = !self.invert)
                .is_some();
        });
        changed
    }
}

#[cfg(feature = "ui")]
impl Gate {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Gate)> {
        vec![(
            "Audio Gate",
            Gate {
                channels: 2,
                threshold: 0.5,
                invert: false,
            },
        )]
    }
}
