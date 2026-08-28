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
/// Each input pair consists of an audio signal socket and an associated parameter gain socket.
/// If a gain socket is unconnected, it falls back to its configured scalar gain.
///
/// The gains are integrated here rather than in a node of their own because a mix with
/// one input is a gain, and the audio half needed a multiply anyway. `Math` is the
/// parameter half's multiply and cannot touch a buffer. Two nodes that share
/// every line of their implementation are one node.
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

    /// Interleaves audio signal ports with their corresponding parameter gain ports.
    fn input_ports(&self) -> Vec<Port> {
        (0..self.inputs)
            .flat_map(|i| {
                let signal = Port::new(
                    format!("in {}", i + 1),
                    PortType::Audio {
                        channels: self.channels,
                    },
                );
                // Allow removal only if more than one input pair exists.
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

    // Routes parameter registers to drive gain lanes in the audio processing pass.
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
        // Align latency across all input branches to prevent phase cancellation.
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
        // Prevent buffer reuse collision with subsequent mix inputs.
        let avoid: Vec<Buf> = inputs.iter().skip(1).map(|i| i.buf).collect();
        let out = cx.alloc_avoiding(self.channels, cx.readers(), &avoid)?;
        cx.emit(AudioOp::Mix { out, inputs });
        cx.produce(0, out, arrive);
        Ok(())
    }

    /// Input control for the fallback gain on odd-numbered parameter sockets.
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
        // Ensure gains vector matches current input count, defaulting to 0 dB.
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

    /// Label for adding another input pair up to a maximum of 8 inputs.
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        (self.inputs < 8).then_some("another input")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        self.inputs += 1;
        self.gains.resize(self.inputs as usize, 0.0);
    }

    /// Removes an input pair (signal and gain sockets), returning the number of removed ports (2).
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
    /// Default catalog entry for the Mix node with 2 stereo inputs at unity gain.
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
