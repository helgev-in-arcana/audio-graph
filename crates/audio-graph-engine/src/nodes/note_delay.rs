use plugin_host::{Event, NoteEvent};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::compile::{CompileError, NoteCx, ParamCx};
use crate::engine::NoteEvalContext;
use crate::ir::{KernelState, NOTE_BUF_CAPACITY, NoteKernel, NoteOwnership, ResetReason};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, beats_control, decimals, fallback};
use crate::nodes::{Beats, Node};
use crate::port::{Port, PortType};

/// Holds a note stream back by a time, in seconds or in beats of the host's
/// tempo: an echo for a plugin, a flam against its own copy, or a part pushed
/// behind the beat.
///
/// No feedback: what comes out is what went in, once. A repeat is a second
/// delay beside the first and a merge after them.
///
/// Everything in the stream waits the same, controllers included, so a pedal
/// stays with the notes it was pressed under. Shortening the time never lets
/// an event overtake one already waiting, keeping a note-off behind its note-on.
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

    fn compile_note(&self, port: u8, cx: &mut NoteCx) -> Result<bool, CompileError> {
        if port != 0 {
            return Ok(false);
        }
        if let Some(input) = cx.input(0) {
            let time = if self.sync {
                self.beats.value()
            } else {
                self.seconds
            };
            cx.emit_custom(
                input,
                &[(1, time.max(0.0))],
                DelayKernel { beats: self.sync },
            )?;
        }
        Ok(true)
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

/// A buffer's worth of events bounds memory while notes wait across blocks.
const QUEUE_CAPACITY: usize = NOTE_BUF_CAPACITY;

#[derive(Debug)]
struct DelayKernel {
    beats: bool,
}

/// Queued note-ons retain ledger ownership until delivery or node removal,
/// so a block without downstream delivery cannot end a note still in flight.
#[derive(Debug)]
struct DelayState {
    queue: VecDeque<(u64, Event)>,
    /// Absolute deadlines avoid visiting every waiting event on every row.
    clock: u64,
    last: u64,
}

impl KernelState for DelayState {
    fn reset(&mut self, _reason: ResetReason) {
        self.queue.clear();
        self.clock = 0;
        self.last = 0;
    }

    fn release(&mut self, notes: &mut NoteOwnership<'_>) {
        for (_, event) in self.queue.drain(..) {
            if let Event::Note(NoteEvent::NoteOn {
                note_id: Some(id), ..
            }) = event
            {
                notes.release(id);
            }
        }
        self.clock = 0;
        self.last = 0;
    }
}

impl NoteKernel for DelayKernel {
    type State = DelayState;

    fn create_state(&self) -> Self::State {
        DelayState {
            queue: VecDeque::with_capacity(QUEUE_CAPACITY),
            clock: 0,
            last: 0,
        }
    }

    fn eval(&self, state: &mut Self::State, cx: &mut NoteEvalContext<'_>) {
        if state.queue.is_empty() {
            if cx.input.is_empty() {
                return;
            }
            // With no deadlines outstanding, a fresh epoch keeps idle time
            // from consuming the clock's range.
            state.clock = 0;
            state.last = 0;
        }

        if !cx.input.is_empty() {
            let amount = cx.parameter(0).max(0.0);
            let seconds = if self.beats {
                amount * 60.0 / cx.tempo().max(1.0)
            } else {
                amount
            };
            let delay = (seconds * cx.sample_rate().max(0.0)).round() as u64;

            for &event in cx.input {
                if state.queue.len() == QUEUE_CAPACITY {
                    cx.output.drop_event();
                    continue;
                }
                let due = state
                    .clock
                    .saturating_add(u64::from(event.sample_offset().saturating_sub(cx.start())))
                    .saturating_add(delay)
                    .max(state.last);
                if let Event::Note(NoteEvent::NoteOn {
                    note_id: Some(id), ..
                }) = event
                {
                    cx.notes.retain(id);
                }
                state.queue.push_back((due, event));
                state.last = due;
            }
        }

        let end = state.clock.saturating_add(u64::from(cx.frames()));
        while let Some(&(due, event)) = state.queue.front() {
            if due >= end {
                break;
            }
            state.queue.pop_front();
            if let Event::Note(NoteEvent::NoteOn {
                note_id: Some(id), ..
            }) = event
            {
                cx.notes.release(id);
            }
            cx.output
                .push(event.at_offset(cx.start() + (due - state.clock) as u32));
        }
        state.clock = end;
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
