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

/// How long the gain takes to travel between shut and open, in milliseconds.
///
/// Long enough that gating a loud signal does not click — a step of full scale
/// is a click whatever the material — and short enough that a gate used
/// rhythmically still sounds like it opens on the beat: 5 ms is under a
/// hundredth of a sixteenth note at 120 bpm.
const FADE_MS: f64 = 5.0;

fn default_fade_ms() -> f64 {
    FADE_MS
}

/// Gates an audio signal based on whether a parameter control value meets a threshold.
///
/// Passes audio through at unity gain (0 dB) when open, or silences it (-100 dB)
/// when closed. If the control input is unconnected, it defaults to zero (closed).
///
/// The switch itself is a parameter, so it happens at a sub-block boundary; what
/// the fade times buy is the shape of the crossing, not its timing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub channels: u16,
    /// The gate opens at this value and above — or below it, with `invert`.
    pub threshold: f64,
    pub invert: bool,
    /// Milliseconds the gain takes to open, and to close. Zero switches hard,
    /// which costs nothing and clicks on anything loud.
    ///
    /// A patch saved before the fades existed is read back with them, rather
    /// than with zeroes: a hard switch is the behaviour nobody asked for.
    #[serde(default = "default_fade_ms")]
    pub fade_in_ms: f64,
    #[serde(default = "default_fade_ms")]
    pub fade_out_ms: f64,
}

impl Gate {
    /// Whether the gain has any distance to travel in time, and so whether the
    /// node costs a latch and a ramp at all.
    fn fades(&self) -> bool {
        self.fade_in_ms > 0.0 || self.fade_out_ms > 0.0
    }
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
        // A fading gate keeps the gain it has reached in a latch, and only the
        // param half hands those out. Taken here rather than in the audio half
        // so that it survives a program swap — a recompile happens on every
        // drag of every control, and one landing mid-fade must not step the
        // gain the fade exists to stop stepping.
        if self.fades() {
            cx.latch()?;
        }
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
        // A gate asked for no fade is a `Mix` of one, which is the cheapest
        // thing that scales a buffer: no state to keep and no ramp to walk.
        match cx.latch_of().filter(|_| self.fades()) {
            Some(state) => cx.emit(AudioOp::Fade {
                out,
                a: buf,
                state,
                lane,
                gain: 1.0,
                rise: self.fade_in_ms.max(0.0) / 1000.0,
                fall: self.fade_out_ms.max(0.0) / 1000.0,
            }),
            None => cx.emit(AudioOp::Mix {
                out,
                inputs: vec![MixIn {
                    buf,
                    lane,
                    gain: 1.0,
                }],
            }),
        }
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
        // In and out mean opening and closing, whichever way `invert` has the
        // threshold pointing: what fades is the gain, not the control.
        for (label, value) in [
            ("fade in (ms)", &mut self.fade_in_ms),
            ("fade out (ms)", &mut self.fade_out_ms),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                changed |= ui
                    .add(egui::DragValue::new(value).speed(0.1).range(0.0..=1000.0))
                    .on_hover_text("zero switches hard, which clicks on anything loud")
                    .changed();
            });
        }
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
                fade_in_ms: FADE_MS,
                fade_out_ms: FADE_MS,
            },
        )]
    }
}
