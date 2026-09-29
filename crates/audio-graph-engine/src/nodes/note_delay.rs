use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, beats_control, decimals, fallback};
use crate::nodes::{Beats, Node, NoteDelay};
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
    /// The time while `sync` is off.
    pub seconds: f64,
    /// The time while `sync` is on. Both are kept, so turning sync on to try
    /// it and off again gives back the time that was there.
    pub beats: Beats,
    pub sync: bool,
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
            time: if self.sync {
                self.beats.value()
            } else {
                self.seconds
            },
            beats: self.sync,
        })
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui
                .selectable_label(self.sync, "sync")
                .on_hover_text("count the time in beats of the host's tempo")
                .clicked()
                .then(|| self.sync = !self.sync)
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
        if self.sync && !connected {
            let mut changed = false;
            ui.horizontal(|ui| changed = beats_control(ui, "time", &mut self.beats));
            return changed;
        }
        // What a wired socket carries is a plain number of beats, which no
        // fraction need spell, so a synced time shows as one while wired.
        let sync = self.sync;
        let mut shown = if sync {
            self.beats.value()
        } else {
            self.seconds
        };
        let changed = fallback(ui, connected, cx.input(port), &mut shown, |ui, time| {
            // A wired time is floored at zero and has no ceiling, so it is
            // shown the same way; the range is only for dragging.
            *time = time.max(0.0);
            ui.add(
                egui::DragValue::new(time)
                    .speed(0.01)
                    .range(0.0..=60.0)
                    .clamp_existing_to_range(false)
                    .fixed_decimals(if sync {
                        decimals::PLAIN
                    } else {
                        decimals::SECONDS
                    })
                    .suffix(if sync { " beats" } else { " s" }),
            )
            .changed()
        });
        if changed && !sync {
            self.seconds = shown;
        }
        changed
    }
}

#[cfg(feature = "ui")]
impl MidiDelay {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, MidiDelay)> {
        vec![(
            "MIDI Delay",
            MidiDelay {
                seconds: 0.25,
                // Half a beat is 0.25 s at 120 bpm.
                beats: Beats::new(1, 2),
                sync: true,
            },
        )]
    }
}
