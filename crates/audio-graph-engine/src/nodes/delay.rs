//! Both halves of a delay line (§14.4).
//!
//! One node in the user's head, two in the graph. The split is what keeps a
//! cycle out of the topological sort: the halves are paired by `line`, never
//! by an edge, so the compiler walks a graph that is still acyclic even when
//! the signal is not. That is ADR-8, and it is the reason these two live in
//! one file — they are the only pair in the node set that has to agree about
//! anything.

use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::graph::LineId;
use crate::ir::Op;
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

impl DelayWrite {
    pub fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("in", self.ty)]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn title(&self) -> String {
        format!("Delay {} write", self.line + 1)
    }
}

impl DelayRead {
    /// The one input a `DelayRead` has is its own delay time (§14.5). It is a
    /// param, never audio, so it cannot close a loop through the line it
    /// belongs to — the type check in `check_links` is what makes that true
    /// rather than a convention.
    pub fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("time")]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", self.ty)]
    }

    pub fn title(&self) -> String {
        format!("Delay {} read", self.line + 1)
    }
}

impl DelayRead {
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
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
}

impl DelayWrite {
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
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
}
