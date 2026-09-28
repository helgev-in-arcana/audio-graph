use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, decimals, fallback};
use crate::nodes::{Node, NoteDelay};
use crate::port::{Port, PortType};

/// Holds a note stream back by a time, in seconds or in beats of the host's
/// tempo: an echo for a plugin, a flam against its own copy, or a part pushed
/// behind the beat.
///
/// No feedback: what comes out is what went in, once. A repeat is a second
/// delay beside the first and a merge after them.
///
/// Everything in the stream waits the same, controllers included, so a pedal
/// stays with the notes it was pressed under. See
/// [`NoteOp::Delay`][crate::ir::NoteOp::Delay] for what happens when the time
/// changes with notes in flight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiDelay {
    /// Seconds, or beats when `beats` is set.
    pub time: f64,
    #[serde(default)]
    pub beats: bool,
}

impl Node for MidiDelay {
    fn title(&self) -> String {
        "MIDI Delay".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note), Port::param("time")]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        if let Some(reg) = cx.input(1) {
            cx.drive_audio(1, reg)?;
        }
        Ok(())
    }

    fn note_delay(&self, port: u8) -> Option<NoteDelay> {
        (port == 0).then_some(NoteDelay {
            input: 0,
            time_input: 1,
            time: self.time,
            beats: self.beats,
        })
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui
                .selectable_label(self.beats, "sync")
                .on_hover_text("count the time in beats of the host's tempo")
                .clicked()
                .then(|| self.beats = !self.beats)
                .is_some();
        });
        changed
    }

    /// The time, on the row of the socket that can drive it instead.
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
        let beats = self.beats;
        fallback(ui, connected, cx.input(port), &mut self.time, |ui, time| {
            // A wired time is floored at zero and has no ceiling, so it is
            // shown the same way; the range is only for dragging.
            *time = time.max(0.0);
            ui.add(
                egui::DragValue::new(time)
                    .speed(0.01)
                    .range(0.0..=60.0)
                    .clamp_existing_to_range(false)
                    .fixed_decimals(if beats {
                        decimals::BEATS
                    } else {
                        decimals::SECONDS
                    })
                    .suffix(if beats { " beats" } else { " s" }),
            )
            .changed()
        })
    }
}

#[cfg(feature = "ui")]
impl MidiDelay {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, MidiDelay)> {
        vec![(
            "MIDI Delay",
            MidiDelay {
                time: 0.5,
                beats: true,
            },
        )]
    }
}
