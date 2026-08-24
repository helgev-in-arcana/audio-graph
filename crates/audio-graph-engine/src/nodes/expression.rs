use serde::{Deserialize, Serialize};

pub use crate::ir::ExprSource;

use crate::compile::AudioCx;
use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::port::Port;

/// A note expression, reduced to one value (see [`ExprSource`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expression {
    pub source: ExprSource,
}

impl Expression {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        self.source.label().into()
    }
}

impl Expression {
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        cx.emit(Op::Expr {
            out,
            source: self.source,
        });
        cx.bind_output(0, out);
        Ok(())
    }
}

impl Expression {
    pub(crate) fn compile_audio(&self, _cx: &mut AudioCx) -> Result<(), CompileError> {
        Ok(())
    }
}
