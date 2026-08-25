//! Both halves of a delay line (§14.4).
//!
//! One node in the user's head, two in the graph. The split is what keeps a
//! cycle out of the topological sort: the halves are paired by `line`, never
//! by an edge, so the compiler walks a graph that is still acyclic even when
//! the signal is not. That is ADR-8, and it is the reason these two live in
//! one file — they are the only pair in the node set that has to agree about
//! anything.

use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::graph::LineId;
use crate::ir::{AudioOp, Op};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback, line_control};
use crate::port::{Port, PortType};

/// The writing half of a delay line (§14.4).
///
/// Has an input and no output, so a graph that goes through a delay has no
/// cycle for the topological sort to find. That is the whole mechanism: the
/// two halves are paired by `line`, never by an edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayWrite {
    pub line: LineId,
    pub ty: PortType,
}

/// The reading half of a delay line (§14.4).
///
/// Has an output and no input. Several reads may share one line — that is a
/// multi-tap delay, and it falls out for free.
///
/// `time` is in seconds and is clamped at run time to the floor of §14.4;
/// the compiler cannot do the clamping itself because the floor depends on
/// the sample rate and the sub-block size, neither of which it knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayRead {
    pub line: LineId,
    pub ty: PortType,
    /// Longest delay this line will ever be asked for. Not automatable: the
    /// ring is allocated for it at activate, and §9.1 forbids allocating in
    /// `process`.
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
        // Nothing plugged in writes silence, the same way an unwired SlotOut
        // simply does not take its slot over.
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
        if let Some((buf, _)) = cx.source(0) {
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
    /// Not in the menu: both halves arrive together, through `Graph::add_delay`
    /// (ADR-8). A `DelayWrite` on its own is a line nothing reads.
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, DelayWrite)> {
        Vec::new()
    }
}

impl Node for DelayRead {
    fn title(&self) -> String {
        format!("Delay {} read", self.line + 1)
    }

    /// The one input a `DelayRead` has is its own delay time (§14.5). It is a
    /// param, never audio, so it cannot close a loop through the line it
    /// belongs to — the type check in `check_links` is what makes that true
    /// rather than a convention.
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
            // The audio half owns this line. All the param half does is carry
            // the time across to it, and only when the user has wired
            // something to the control: a lane is a scarce thing to spend on a
            // number that never changes (§14.12, and §14.5 for why a lane at
            // all - the audio pass runs after every sub-block of the param
            // pass, so a register would hold the wrong one).
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
        // A line is a cut, not an edge: what comes out of it did not travel
        // here through the paths §14.6 is lining up, so it arrives with no
        // latency of its own to compensate for.
        cx.produce(0, out, 0);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let mut changed = line_control(ui, &mut self.line);
        // The floor of §14.4, in the units the control is in. It is the
        // sub-block size, which the user chose, so it moves when they change
        // that setting — and the value is raised with it rather than the delay
        // quietly running longer than it says.
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
            ui.weak(format!(
                "at least {:.1} ms — one sub-block (§14.4)",
                floor * 1000.0
            ));
        }
        changed
    }

    /// The delay time, on the row of the socket that sweeps it.
    ///
    /// The floor is re-applied here as well as in `controls`, because this is
    /// where the number is now set and the sub-block size can move under it
    /// while the patch is open (§14.4).
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
