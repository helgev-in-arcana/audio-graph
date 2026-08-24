use serde::{Deserialize, Serialize};

pub use crate::ir::ExprSource;

/// A note expression, reduced to one value (see [`ExprSource`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expression {
    pub source: ExprSource,
}
