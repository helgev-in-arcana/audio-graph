use serde::{Deserialize, Serialize};

pub use crate::ir::MathOp;

/// Two inputs and an operator. Input 1 falls back to `b` when unconnected,
/// so a "multiply by 0.5" node needs no second node feeding it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Math {
    pub op: MathOp,
    pub b: f64,
}
