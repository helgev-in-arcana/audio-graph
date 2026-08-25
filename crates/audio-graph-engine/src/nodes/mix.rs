use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, Buf, MixIn};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback};
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
                let signal = Port::new(
                    format!("in {}", i + 1),
                    PortType::Audio {
                        channels: self.channels,
                    },
                );
                // The signal socket carries the pair's remove button. The last
                // input is not offered one: a mix of none is not a mix, and a
                // mix of one is a gain, which is a thing people want.
                #[cfg(feature = "ui")]
                let signal = if self.inputs > 1 {
                    signal.removable()
                } else {
                    signal
                };
                [signal, Port::param(format!("gain {}", i + 1))]
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

    /// The gain for input `i` sits on the row of its own socket, which is the
    /// odd port of each pair. The even one — the signal — has nothing to set:
    /// what it carries is whatever is wired to it.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        _cx: &mut NodeUi<'_>,
    ) -> bool {
        if port.is_multiple_of(2) {
            return false;
        }
        // Grown here rather than at load: a patch saved before the gains
        // existed has none, and every missing one is unity.
        self.gains.resize(self.inputs as usize, 1.0);
        let Some(gain) = self.gains.get_mut(port as usize / 2) else {
            return false;
        };
        fallback(ui, connected, |ui| {
            ui.add(egui::DragValue::new(gain).speed(0.005).range(0.0..=2.0))
                .changed()
        })
    }

    /// Eight is the ceiling the old spinner had, and nothing below it has
    /// changed: past that a mix is a wall of sockets and a second Mix reads
    /// better than a taller one.
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        (self.inputs < 8).then_some("+ input")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        self.inputs += 1;
        self.gains.resize(self.inputs as usize, 1.0);
    }

    /// One input is two sockets, and they go together: a signal with no gain
    /// beside it would leave every later gain a socket out of step.
    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let index = port as usize / 2;
        if self.inputs <= 1 || index >= self.inputs as usize {
            return 0;
        }
        self.inputs -= 1;
        self.gains.resize((self.inputs + 1) as usize, 1.0);
        self.gains.remove(index);
        2
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
