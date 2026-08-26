use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, MixIn, Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Silence below -100 dB, which is what `db_to_linear` reads as a mute.
///
/// A gate is a `Mix` of one whose gain the parameter half switches, so
/// "closed" has to be spelled in the units the lane carries.
const CLOSED_DB: f64 = -100.0;
const OPEN_DB: f64 = 0.0;

/// Pass audio through or silence it, by where a control sits against a
/// threshold.
///
/// A `Mix` with one input is already a gain, and a gain of -100 dB is already
/// silence — so the whole of this node is the parameter half deciding which of
/// the two the gain is. That is deliberate: a second way of scaling a buffer
/// would be a second place for the mix rules to drift.
///
/// The switch is hard, at chunk boundaries. Under `WholeBlock` chunking that
/// is once per block the DAW hands us, so gating a loud signal can click; the
/// honest fix is a ramp in the audio half, and it is not here yet.
///
/// An unwired control reads as zero, like every other empty parameter socket,
/// which means a gate nobody has wired is shut rather than open. That is the
/// safe way round: a gate that passed everything until wired would look like
/// it was not working, and then like it had broken.
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

    /// The decision is a parameter, so it is made here; the audio half only
    /// applies the gain it lands on.
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
        // On the control's own socket, so the audio half finds it the way a
        // `Mix` finds a gain (§14.5).
        cx.drive_audio(1, out)
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let readers = cx.readers();
        let Some((buf, late)) = cx.source(0) else {
            let out = cx.alloc(self.channels, readers)?;
            cx.emit(AudioOp::Silence { out });
            cx.produce(0, out, 0);
            return Ok(());
        };
        let lane = cx.lane(1);
        cx.consume(buf);
        // May well come back as `buf` itself, which makes the gate a scaling
        // in place and costs no buffer at all — the same case a `Mix` of one
        // hits.
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
