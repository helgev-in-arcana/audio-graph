use serde::{Deserialize, Serialize};

pub use crate::ir::MathOp;

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
