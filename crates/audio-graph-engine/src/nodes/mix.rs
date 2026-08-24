use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, Buf, MixIn};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Sum several audio inputs of the same width into one, each at its own
/// gain.
///
/// The only way two audio sources reach one destination. An input takes one
/// link everywhere in this graph, so mixing is a node rather than a rule —
/// and being a node is what lets the compiler see the merge and line the
/// paths up (§14.6).
///
/// The gains are here rather than in a node of their own because a mix with
/// one input *is* a gain, and the audio half needed a multiply anyway: a
/// feedback delay whose loop gain is one never decays, and `Math` is the
/// param half's multiply and cannot touch a buffer. Two nodes that share
/// every line of their implementation are one node.
///
/// Each gain has a socket of its own, after the audio inputs; the number
/// here is what is used while that socket is unconnected, the same rule
/// `Math`'s `b` follows. Missing entries are 1.0, which is what makes a
/// patch saved before the gains existed still mix the way it did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mix {
    pub channels: u16,
    pub inputs: u8,
    #[serde(default)]
    pub gains: Vec<f64>,
}

impl Node for Mix {
    fn title(&self) -> String {
        "Mix".into()
    }

    /// Each input next to its own gain, rather than all the signals followed
    /// by all the gains: they are one row of one control on screen, and a
    /// socket list that does not read that way makes the user count.
    fn input_ports(&self) -> Vec<Port> {
        (0..self.inputs)
            .flat_map(|i| {
                [
                    Port::new(
                        format!("in {}", i + 1),
                        PortType::Audio {
                            channels: self.channels,
                        },
                    ),
                    Port::param(format!("gain {}", i + 1)),
                ]
            })
            .collect()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new(
            "out",
            PortType::Audio {
                channels: self.channels,
            },
        )]
    }

    /// A mix's gains are params, so the param half is where their lanes are
    /// booked; the scaling itself is the audio half's.
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        for i in 0..self.inputs {
            // Signal, gain, signal, gain: the gain for input `i` is the socket
            // right after it.
            let port = 2 * i + 1;
            if let Some(reg) = cx.input(port) {
                cx.drive_audio(port, reg)?;
            }
        }
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        // §14.6, the merge point: every branch waits for the latest one or they
        // phase-cancel.
        let arrive = cx
            .sources()
            .iter()
            .filter_map(|s| s.map(|(_, late)| late))
            .max()
            .unwrap_or(0);
        let wired: Vec<(usize, Buf, u32)> = cx
            .sources()
            .iter()
            .enumerate()
            .filter_map(|(port, s)| s.map(|(buf, late)| (port, buf, late)))
            .collect();

        let mut inputs = Vec::new();
        for (port, buf, late) in wired {
            if arrive > late {
                cx.compensate(buf, arrive - late)?;
            }
            inputs.push(MixIn {
                buf,
                // Signal, gain, signal, gain: the gain for input `port` is the
                // socket right after it.
                lane: cx.lane((2 * port + 1) as u8),
                gain: self.gains.get(port).copied().unwrap_or(1.0),
            });
        }
        for input in &inputs {
            cx.consume(input.buf);
        }
        // The first input may be reused as the destination — that is what makes
        // the mix an accumulate rather than a copy — but the rest may not, or
        // the sum would be built out of a buffer that has already been written
        // over.
        let avoid: Vec<Buf> = inputs.iter().skip(1).map(|i| i.buf).collect();
        let out = cx.alloc_avoiding(self.channels, cx.readers(), &avoid)?;
        cx.emit(AudioOp::Mix { out, inputs });
        cx.produce(0, out, arrive);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("inputs");
            let mut count = self.inputs as u32;
            // One is allowed, and useful: a mix of one input *is* a gain, which
            // is what turns a feedback delay's loop down below unity so it
            // decays.
            if ui
                .add(egui::DragValue::new(&mut count).range(1..=8))
                .changed()
            {
                self.inputs = count as u8;
                changed = true;
            }
        });
        // Grown here rather than at load: a patch saved before the gains
        // existed has none, and every missing one is unity.
        self.gains.resize(self.inputs as usize, 1.0);
        for (i, gain) in self.gains.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("gain {}", i + 1));
                changed |= ui
                    .add(egui::DragValue::new(gain).speed(0.005).range(0.0..=2.0))
                    .changed();
            });
        }
        ui.weak("a gain is used only while its socket is unconnected");
        changed
    }
}

#[cfg(feature = "ui")]
impl Mix {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Mix)> {
        vec![
            (
                "Mix",
                Mix {
                    channels: 2,
                    inputs: 2,
                    gains: vec![1.0, 1.0],
                },
            ),
            (
                "Gain",
                Mix {
                    channels: 2,
                    inputs: 1,
                    // Half back round is a delay that decays over a few
                    // repeats, which is what a one-input mix is nearly always
                    // dropped in to do. It is the same node as the one above —
                    // only the starting shape differs, and having both in the
                    // menu is cheaper than making the user work that out.
                    gains: vec![0.5],
                },
            ),
        ]
    }
}
