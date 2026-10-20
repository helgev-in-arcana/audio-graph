use plugin_host::{Event, NoteEvent};
use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::engine::{AudioEvalContext, AudioIo, AudioSamples, TremoloClock};
use crate::ir::{AudioKernel, AudioOp, KernelState, ResetReason, TremoloSpec};
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
/// same samples — a part and a recording of it can be cut together.
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

#[derive(Debug)]
struct TremoloKernel {
    spec: TremoloSpec,
    fade_in: f64,
    fade_out: f64,
}

#[derive(Debug)]
struct TremoloState {
    clock: TremoloClock,
    gain: f64,
}

impl KernelState for TremoloState {
    fn reset(&mut self, reason: ResetReason) {
        if reason == ResetReason::All {
            self.clock = TremoloClock::STOPPED;
            self.gain = 1.0;
        } else {
            self.clock.forget_keys();
        }
    }
}

impl AudioKernel for TremoloKernel {
    type State = TremoloState;

    fn create_state(&self) -> TremoloState {
        TremoloState {
            clock: TremoloClock::STOPPED,
            gain: 1.0,
        }
    }

    fn eval(&self, state: &mut TremoloState, cx: &mut AudioEvalContext<'_>) {
        match &mut cx.audio {
            AudioIo::InPlace(audio) => {
                self.run(state, audio, cx.notes, cx.start, cx.sample_rate, cx.tempo)
            }
            AudioIo::Separate(audio) => {
                self.run(state, audio, cx.notes, cx.start, cx.sample_rate, cx.tempo)
            }
        }
    }
}

impl TremoloKernel {
    fn run(
        &self,
        state: &mut TremoloState,
        audio: &mut impl AudioSamples,
        events: &[Event],
        start: usize,
        sample_rate: f64,
        tempo: f64,
    ) {
        let rate = sample_rate.max(1.0);
        let up = 1.0 / (self.fade_in * rate).max(1.0);
        let down = 1.0 / (self.fade_out * rate).max(1.0);
        state.clock.follow(tempo, rate);
        if events.is_empty() && state.clock.stopped() && state.gain == 1.0 {
            audio.passthrough();
            state.clock.advance(audio.frames());
            return;
        }
        let mut gain = state.gain;
        let mut next = 0;
        for i in 0..audio.frames() {
            let at = (start + i) as u32;
            state.clock.edge(&self.spec, rate);
            while let Some(event) = events.get(next)
                && event.sample_offset() <= at
            {
                next += 1;
                let (key, on) = match *event {
                    Event::Note(NoteEvent::NoteOn { key, .. }) => (key, true),
                    Event::Note(NoteEvent::NoteOff { key, .. }) => (key, false),
                    _ => continue,
                };
                if self.spec.steers(key) {
                    state.clock.switch(&self.spec, key, on);
                }
            }
            let target = if state.clock.sounding() { 1.0 } else { 0.0 };
            gain = if gain < target {
                (gain + up).min(target)
            } else {
                (gain - down).max(target)
            };
            let gain32 = gain as f32;
            for ch in 0..audio.channels() {
                audio.write(ch, i, audio.read(ch, i) * gain32);
            }
            state.clock.tick();
        }
        state.gain = gain;
    }
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
        // Keep the clock across temporarily disconnecting the audio input.
        let index = cx.custom(TremoloKernel {
            spec: self.keys.spec(),
            fade_in: self.fade_in_ms.max(0.0) / 1000.0,
            fade_out: self.fade_out_ms.max(0.0) / 1000.0,
        });
        let Some((a, late)) = cx.source_at_socket_width(0)? else {
            let out = cx.alloc(self.channels, readers)?;
            cx.emit(AudioOp::Silence { out });
            cx.produce(0, out, 0);
            return Ok(());
        };
        cx.consume(a);
        let out = cx.alloc(self.channels, readers)?;
        cx.emit_custom(index, a, out, cx.note_source_of(1));
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
