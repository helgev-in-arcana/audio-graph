//! Reading a parameter off audio.

use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
pub use crate::ir::Detect;
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo};
use crate::port::{Port, PortType};

/// How loud what is wired into it is, as a parameter.
///
/// Every other node either carries audio or carries a value; this one turns
/// the first into the second, which is what a sidechain, a ducker, a level
/// meter and an auto-gain all need to be drawable at all. The alternative — a
/// compressor plugin with a hidden sidechain input — is exactly the invisible
/// route the canvas exists to replace.
///
/// The reading is per sub-block, because that is what a parameter is: the
/// value in force at a boundary. So the attack and release cannot be shorter
/// than one sub-block, and at the default quantum of 32 that is two thirds of
/// a millisecond at 48 kHz. Fast enough for a compressor, not for a clipper.
///
/// There is no latency. The stage holding this runs after the one that made
/// the audio, and that stage covered the whole block, so the window read for a
/// sub-block is that sub-block's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeFollower {
    pub detect: Detect,
    /// Seconds to rise. Zero follows the level exactly.
    pub attack: f64,
    /// Seconds to fall. Zero follows the level exactly.
    pub release: f64,
}

impl Default for EnvelopeFollower {
    fn default() -> Self {
        // A compressor's ballpark: quick to catch a hit, slow to let go.
        EnvelopeFollower {
            detect: Detect::Peak,
            attack: 0.005,
            release: 0.100,
        }
    }
}

impl Node for EnvelopeFollower {
    fn title(&self) -> String {
        match self.detect {
            Detect::Peak => "Envelope (peak)".into(),
            Detect::Rms => "Envelope (RMS)".into(),
        }
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("in", PortType::STEREO)]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("level")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        // The buffer is not known yet: the audio half hands them out and runs
        // after this one. What is booked here is a hole and which socket
        // fills it, the same arrangement the note half uses for its lanes.
        match cx.audio_source_of(0) {
            Some(socket) => {
                let state = cx.latch()?;
                cx.emit_follow(
                    Op::Follow {
                        out,
                        // Patched by `resolve_follows` once the audio half has
                        // said which buffer leaves that socket.
                        buf: 0,
                        state,
                        detect: self.detect,
                        attack: self.attack.max(0.0),
                        release: self.release.max(0.0),
                    },
                    socket,
                );
            }
            // Nothing wired: silence is as loud as nothing.
            None => cx.emit(Op::Const { out, value: 0.0 }),
        }
        cx.bind_output(0, out);
        Ok(())
    }

    fn compile_audio(&self, _cx: &mut AudioCx) -> Result<(), CompileError> {
        // Deliberately nothing. Not emitting a read is also how the buffer
        // stays alive: the pool frees one when every reader it counted has
        // taken it, and this node was counted. The parameter op reads it in a
        // later stage, by which time anything that did consume it would have
        // let it be handed out again.
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let mut changed = combo(
            ui,
            "reads",
            &mut self.detect,
            &[Detect::Peak, Detect::Rms],
            |detect| match detect {
                Detect::Peak => "peak",
                Detect::Rms => "RMS",
            },
        );
        for (label, value, most) in [
            ("attack (s)", &mut self.attack, 1.0),
            ("release (s)", &mut self.release, 4.0),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                changed |= ui
                    .add(egui::DragValue::new(value).speed(0.005).range(0.0..=most))
                    .changed();
            });
        }
        // What the reading cannot go faster than, and why.
        ui.weak(format!(
            "moves at most once every {:.2} ms (one sub-block)",
            cx.quantum as f64 / cx.sample_rate.max(1.0) * 1000.0
        ));
        changed
    }
}

#[cfg(feature = "ui")]
impl EnvelopeFollower {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, EnvelopeFollower)> {
        vec![("Envelope Follower", EnvelopeFollower::default())]
    }
}
