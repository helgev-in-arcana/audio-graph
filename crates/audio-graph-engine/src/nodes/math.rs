use serde::{Deserialize, Serialize};

pub use crate::ir::MathOp;

use crate::compile::AudioCx;
use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::port::Port;

/// Two inputs and an operator. Input 1 falls back to `b` when unconnected,
/// so a "multiply by 0.5" node needs no second node feeding it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Math {
    pub op: MathOp,
    pub b: f64,
}

impl Math {
    pub fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("a"), Port::param("b")]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        self.op.label().into()
    }
}

impl Math {
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let a = cx.input_or_zero(0)?;
        let b = match cx.input(1) {
            Some(reg) => Operand::Reg(reg),
            None => Operand::Value(self.b),
        };
        let out = cx.alloc()?;
        cx.emit(Op::Math {
            out,
            a,
            b,
            op: self.op,
        });
        cx.bind_output(0, out);
        Ok(())
    }
}

impl Math {
    pub(crate) fn compile_audio(&self, _cx: &mut AudioCx) -> Result<(), CompileError> {
        Ok(())
    }
}
