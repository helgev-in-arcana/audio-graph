use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, Buf, MixIn};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback};
use crate::port::{Port, PortType};

/// Convert a decibel value to a linear amplitude multiplier.
///
/// -100 dB or below evaluates to 0.0 (silence / mute).
/// For values above -100 dB, this evaluates to 10^(db / 20).
#[inline]
pub fn db_to_linear(db: f64) -> f64 {
    if db <= -100.0 {
        0.0
    } else {
        10.0_f64.powf(db / 20.0)
    }
}

/// Convert a linear amplitude multiplier to decibels.
#[inline]
pub fn linear_to_db(linear: f64) -> f64 {
    if linear <= 0.0 {
        -100.0
    } else {
        (20.0 * linear.log10()).max(-100.0)
    }
}

/// Sums multiple audio inputs of matching channel width, applying per-input gain in decibels.
///
/// The only way two audio sources reach one destination. An input takes one
/// link everywhere in this graph, so mixing is a node rather than a rule — and
/// being a node is what lets the compiler see the merge and line the paths up.
///
/// Each input pair is an audio signal socket and a parameter gain socket beside
/// it. If a gain socket is unconnected, its configured scalar gain is used, the
/// same rule `Math`'s `b` follows.
///
/// The gains are integrated here rather than in a node of their own because a
/// mix with one input *is* a gain, and the audio half needed a multiply anyway:
/// a feedback delay whose loop gain is 0 dB never decays, and `Math` is the
/// parameter half's multiply and cannot touch a buffer. Two nodes that share
/// every line of their implementation are one node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mix {
    pub channels: u16,
    pub inputs: u8,
    /// Fallback gain in decibels per input, used while that input's gain socket
    /// is unconnected. A missing entry is 0.0 dB, so an empty vector is unity —
    /// which is what a mix did before it had gains at all.
    #[serde(default)]
    pub gains: Vec<f64>,
}

impl Node for Mix {
    fn title(&self) -> String {
        "Mix".into()
    }

    /// Each input next to its own gain, rather than all the signals followed by
    /// all the gains: they are one row of one control on screen, and a socket
    /// list that does not read that way makes the user count.
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

    // A mix's gains are params, so the param half is where their lanes are
    // booked; the scaling itself is the audio half's.
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        for i in 0..self.inputs {
            // Gain for input `i` corresponds to the parameter port at index `2 * i + 1`.
            let port = 2 * i + 1;
            if let Some(reg) = cx.input(port) {
                cx.drive_audio(port, reg)?;
            }
        }
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        // The merge point: every branch waits for the latest one or they
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
            // Compensated at the width it arrived in, before any conversion:
            // the ring is one channel cheaper that way, and the delay is the
            // same either side of the copy.
            if arrive > late {
                cx.compensate(buf, arrive - late)?;
            }
            // The sum runs one channel at a time across the mix's own width, so
            // a narrower input has to be widened first — otherwise its second
            // channel is read out of the next buffer along.
            let Some((buf, _)) = cx.source_at_socket_width(port)? else {
                continue;
            };
            inputs.push(MixIn {
                buf,
                // Associate each audio buffer with its corresponding gain lane.
                lane: cx.lane((2 * port + 1) as u8),
                gain: self
                    .gains
                    .get(port)
                    .copied()
                    .map(db_to_linear)
                    .unwrap_or(1.0),
            });
        }
        for input in &inputs {
            cx.consume(input.buf);
        }
        // The first input may be reused as the destination — that is what
        // makes the mix an accumulate rather than a copy — but the rest may
        // not, or the sum would be built out of a buffer already written over.
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
        // existed has none, and every missing one is unity (0 dB).
        self.gains.resize(self.inputs as usize, 0.0);
        let Some(gain) = self.gains.get_mut(port as usize / 2) else {
            return false;
        };
        fallback(ui, connected, |ui| {
            ui.add(
                egui::DragValue::new(gain)
                    .speed(0.1)
                    .range(-100.0..=20.0)
                    .suffix(" dB"),
            )
            .changed()
        })
    }

    /// Eight is the ceiling: past that a mix is a wall of sockets, and a second
    /// Mix reads better than a taller one.
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        (self.inputs < 8).then_some("another input")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        self.inputs += 1;
        self.gains.resize(self.inputs as usize, 0.0);
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
        self.gains.resize((self.inputs + 1) as usize, 0.0);
        self.gains.remove(index);
        2
    }
}

#[cfg(feature = "ui")]
impl Mix {
    /// One entry, called what it is.
    ///
    /// There was a second, "Gain", which was this node starting with one input
    /// — true to how the node works, and no help at all: picking Gain and
    /// watching a node called Mix appear reads as the menu having handed over
    /// the wrong thing. Taking an input off a Mix is how you get a gain, and the
    /// node says so on the row where it happens.
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Mix)> {
        vec![(
            "Mix",
            Mix {
                channels: 2,
                inputs: 2,
                gains: vec![0.0, 0.0],
            },
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibel_conversion_round_trips() {
        assert_eq!(db_to_linear(0.0), 1.0);
        assert_eq!(linear_to_db(1.0), 0.0);

        assert_eq!(db_to_linear(-100.0), 0.0);
        assert_eq!(db_to_linear(-120.0), 0.0);
        assert_eq!(linear_to_db(0.0), -100.0);

        let lin_6db = db_to_linear(6.0);
        assert!((lin_6db - 1.9952623149688795).abs() < 1e-10);
        assert!((linear_to_db(lin_6db) - 6.0).abs() < 1e-10);

        let lin_neg6db = db_to_linear(-6.0);
        assert!((lin_neg6db - 0.5011872336272722).abs() < 1e-10);
        assert!((linear_to_db(lin_neg6db) - -6.0).abs() < 1e-10);
    }
}
