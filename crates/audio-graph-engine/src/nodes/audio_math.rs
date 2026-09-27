use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError};
use crate::ir::{AudioMathOp, AudioOp, Buf};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo};
use crate::port::{Port, PortType};

fn stereo() -> u16 {
    2
}

/// A sample-by-sample operation on an audio signal: taking out a DC offset,
/// flipping the polarity, rectifying, or multiplying by a second signal.
///
/// One node with a choice of op rather than one node per op, the way `Math`
/// is for parameters: they are the same kind of thing to the user, and a menu
/// of four near-identical entries is harder to find one in than one entry with
/// a dropdown.
///
/// The second input is always there, and only the op that reads it reads it.
/// A socket that came and went with the op would take its link with it on
/// every trip through the dropdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioMath {
    #[serde(default = "stereo")]
    pub channels: u16,
    #[serde(default)]
    pub op: AudioMathOp,
}

impl AudioMath {
    fn ty(&self) -> PortType {
        PortType::Audio {
            channels: self.channels,
        }
    }
}

impl Node for AudioMath {
    fn title(&self) -> String {
        format!("Audio {}", self.op.label())
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("in", self.ty()), Port::new("by", self.ty()).aux()]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", self.ty())]
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let readers = cx.readers();
        // Booked whatever the op, so that switching ops does not move any
        // other node's state to a different index.
        let state = cx.dsp_state()?;
        let Some((_, late_a)) = cx.source(0) else {
            let out = cx.alloc(self.channels, readers)?;
            cx.emit(AudioOp::Silence { out });
            cx.produce(0, out, 0);
            return Ok(());
        };
        let late_b = if self.op.takes_b() {
            cx.source(1).map(|(_, late)| late)
        } else {
            None
        };

        // Both signals have to arrive together or the product is of two
        // moments that never met — the same rule a mix follows.
        let arrive = late_a.max(late_b.unwrap_or(0));
        for (port, late) in [(0, Some(late_a)), (1, late_b)] {
            if let (Some(late), Some((buf, _))) = (late, cx.source(port))
                && arrive > late
            {
                cx.compensate(buf, arrive - late)?;
            }
        }
        let Some((a, _)) = cx.source_at_socket_width(0)? else {
            return Ok(());
        };
        let b: Option<Buf> = match late_b {
            Some(_) => cx.source_at_socket_width(1)?.map(|(buf, _)| buf),
            None => None,
        };
        cx.consume(a);
        if let Some(b) = b {
            cx.consume(b);
        }
        // `a` may become the output, since each sample is read before it is
        // written; `b` may not, or the product would read its own result.
        let avoid: Vec<Buf> = b.into_iter().collect();
        let out = cx.alloc_avoiding(self.channels, readers, &avoid)?;
        cx.emit(AudioOp::Math {
            out,
            a,
            b,
            op: self.op,
            state,
        });
        cx.produce(0, out, arrive);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        combo(
            ui,
            "op",
            &mut self.op,
            &AudioMathOp::ALL,
            AudioMathOp::label,
        )
    }

    /// Says so on the second socket's row when the op does not read it.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        _connected: bool,
        _cx: &mut NodeUi<'_>,
    ) -> bool {
        if port == 1 && !self.op.takes_b() {
            ui.weak("unused")
                .on_hover_text(format!("{} reads only the first input", self.op.label()));
        }
        false
    }
}

#[cfg(feature = "ui")]
impl AudioMath {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, AudioMath)> {
        vec![(
            "Audio Math",
            AudioMath {
                channels: 2,
                op: AudioMathOp::RemoveDc,
            },
        )]
    }
}
