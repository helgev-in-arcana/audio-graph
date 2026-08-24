use serde::{Deserialize, Serialize};

pub use crate::ir::ExprSource;

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
