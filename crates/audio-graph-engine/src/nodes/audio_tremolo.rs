use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::AudioOp;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::nodes::{Node, TremoloKeys};
use crate::port::{Port, PortType};

/// How long the gain takes to cross between cut and sounding, in
/// milliseconds: an Audio Gate's fade, for an Audio Gate's reason. At 120 bpm
/// a 32nd note is 62.5 ms, so a 5 ms slide leaves most of a step at full
/// level.
#[cfg(feature = "ui")]
const FADE_MS: f64 = 5.0;

/// Cuts audio into steps while a key switch says so: what cutting a
/// recording up by hand would give, for release-cut playing, without the
/// cutting.
///
/// The keys come in on a note socket and are read by the same clock a MIDI
/// Tremolo runs, so the two, set alike and fed the same stream, cut on the
/// same samples — a part and a recording of it can be cut together. See
/// [`AudioOp::Tremolo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTremolo {
    pub channels: u16,
    #[serde(flatten)]
    pub keys: TremoloKeys,
    /// Milliseconds the gain takes to rise from cut to sounding, and to fall.
    /// Zero cuts hard, which clicks on anything loud.
    pub fade_in_ms: f64,
    pub fade_out_ms: f64,
}

impl Node for AudioTremolo {
    fn title(&self) -> String {
        "Audio Tremolo".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![
            Port::new(
                "in",
                PortType::Audio {
                    channels: self.channels,
                },
            ),
            Port::new("keys", PortType::Note),
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
        let _ = cx;
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let readers = cx.readers();
        // Booked before anything that can return, so the clock belongs to the
        // node whether or not anything is wired into it yet.
        let state = cx.tremolo_state()?;
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
            notes: cx.note_source_of(1),
            state,
            spec: self.keys.spec(),
            fade_in: self.fade_in_ms.max(0.0) / 1000.0,
            fade_out: self.fade_out_ms.max(0.0) / 1000.0,
        });
        cx.produce(0, out, late);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = self.keys.controls(ui);
        for (label, value) in [
            ("fade in (ms)", &mut self.fade_in_ms),
            ("fade out (ms)", &mut self.fade_out_ms),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                changed |= ui
                    .add(egui::DragValue::new(value).speed(0.1).range(0.0..=1000.0))
                    .on_hover_text("zero cuts hard, which clicks on anything loud")
                    .changed();
            });
        }
        changed
    }
}

#[cfg(feature = "ui")]
impl AudioTremolo {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, AudioTremolo)> {
        vec![(
            "Audio Tremolo",
            AudioTremolo {
                channels: 2,
                keys: TremoloKeys::defaults(),
                fade_in_ms: FADE_MS,
                fade_out_ms: FADE_MS,
            },
        )]
    }
}
