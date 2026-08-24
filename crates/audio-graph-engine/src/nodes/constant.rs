use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::port::Port;

/// A fixed number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constant {
    pub value: f64,
}

impl Constant {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        "Constant".into()
    }
}

impl Constant {
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        cx.emit(Op::Const {
            out,
            value: self.value,
        });
        cx.bind_output(0, out);
        Ok(())
    }
}
