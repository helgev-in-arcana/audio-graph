//! Delay line read and write nodes.
//!
//! Delay lines are split into write and read endpoints paired by a shared line ID
//! rather than a direct graph edge, avoiding cycles in the topological sort
//! while supporting feedback topologies.

use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::graph::LineId;
use crate::ir::{AudioOp, Op};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback, line_control};
use crate::port::{Port, PortType};

/// The write endpoint of a delay line.
///
/// Receives an input signal and writes it to the designated delay buffer.
/// It has no graph output ports, breaking feedback cycles during compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayWrite {
    pub line: LineId,
    pub ty: PortType,
}

/// The read endpoint of a delay line.
///
/// Reads from the designated delay buffer at a specified delay time. Multiple
/// read nodes can reference the same line for multi-tap delays.
///
/// `time` is in seconds and is clamped at runtime to a minimum floor equal to
/// one processing quantum (sub-block duration).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayRead {
    pub line: LineId,
    pub ty: PortType,
    /// The maximum delay time in seconds.
    ///
    /// A ring buffer is allocated during activation because allocating on the audio
    /// thread is forbidden. Therefore, `max_time` cannot be automatable, as changing
    /// it on the fly would require reallocating the buffer.
    ///
    /// Delay lines start with zero accumulated latency because a line is a cut,
    /// not an edge.
    pub max_time: f64,
    pub time: f64,
}

impl Node for DelayWrite {
    fn title(&self) -> String {
        format!("Delay {} write", self.line + 1)
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("in", self.ty)]
    }

    fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn declare(&self, cx: &mut DeclareCx) -> Result<(), CompileError> {
        cx.declare_line(self.line, self.ty, true)
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        if matches!(self.ty, PortType::Audio { .. }) {
            return Ok(());
        }
        let line = cx.line_index(self.line);
        // If the input is unconnected, no write operation is emitted.
        if let Some(reg) = cx.input(0) {
            cx.emit_deferred(Op::DelayWrite { line, a: reg });
        }
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        if !matches!(self.ty, PortType::Audio { .. }) {
            return Ok(());
        }
        let line = cx.audio_line(self.line)?;
        if let Some((buf, _)) = cx.source_at_socket_width(0)? {
            cx.consume(buf);
            cx.emit_deferred(AudioOp::DelayWrite { line, a: buf });
        }
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        line_control(ui, &mut self.line)
    }
}

#[cfg(feature = "ui")]
impl DelayWrite {
    /// Delay write nodes are instantiated together with their read counterparts.
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, DelayWrite)> {
        Vec::new()
    }
}

impl Node for DelayRead {
    fn title(&self) -> String {
        format!("Delay {} read", self.line + 1)
    }

    /// The single input port controls the delay time via a parameter signal.
    fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("time")]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", self.ty)]
    }

    fn declare(&self, cx: &mut DeclareCx) -> Result<(), CompileError> {
        cx.declare_line(self.line, self.ty, false)
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let line = cx.line_index(self.line);
        let time_reg = cx.input(0);
        if matches!(self.ty, PortType::Audio { .. }) {
            // For audio lines, route the dynamic delay time parameter register
            // to drive the audio processing lane.
            if let Some(reg) = time_reg {
                cx.drive_audio(0, reg)?;
            }
            return Ok(());
        }
        let out = cx.alloc()?;
        cx.emit(Op::DelayRead {
            out,
            line,
            // A negative time would read the future.
            time: self.time.max(0.0),
            time_reg,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let PortType::Audio { channels } = self.ty else {
            return Ok(());
        };
        let line = cx.audio_line(self.line)?;
        cx.want_ring(line, self.max_time);
        let out = cx.alloc(channels, cx.readers())?;
        cx.emit(AudioOp::DelayRead {
            out,
            line,
            lane: cx.lane(0),
            time: self.time.max(0.0),
            max_time: self.max_time.max(0.0),
        });
        // Delay line outputs start with zero accumulated latency.
        cx.produce(0, out, 0);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let mut changed = line_control(ui, &mut self.line);
        // Minimum delay floor corresponds to one processing quantum in seconds.
        let floor = cx.quantum as f64 / cx.sample_rate.max(1.0);
        if self.time < floor {
            self.time = floor;
            changed = true;
        }
        ui.horizontal(|ui| {
            ui.label("max (s)");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.max_time)
                        .speed(0.01)
                        .range(0.01..=2.0),
                )
                .changed();
        });
        if matches!(self.ty, PortType::Audio { .. }) {
            ui.weak(format!("at least {:.1} ms (one sub-block)", floor * 1000.0));
        }
        changed
    }

    /// The delay time, on the row of the socket that sweeps it.
    ///
    /// Clamps the delay time to the minimum sub-block quantum duration.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        cx: &mut NodeUi<'_>,
    ) -> bool {
        if port != 0 {
            return false;
        }
        let floor = cx.quantum as f64 / cx.sample_rate.max(1.0);
        let max_time = self.max_time;
        let time = &mut self.time;
        let mut changed = false;
        if *time < floor {
            *time = floor;
            changed = true;
        }
        changed
            | fallback(ui, connected, |ui| {
                ui.add(
                    egui::DragValue::new(time)
                        .speed(0.001)
                        .range(floor..=max_time)
                        .suffix(" s"),
                )
                .on_hover_text("wire this socket to sweep it — the pitch moves with it")
                .changed()
            })
    }
}

#[cfg(feature = "ui")]
impl DelayRead {
    /// See [`DelayWrite::catalogue_defaults`].
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, DelayRead)> {
        Vec::new()
    }
}
