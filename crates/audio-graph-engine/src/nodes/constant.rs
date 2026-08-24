use serde::{Deserialize, Serialize};

/// A fixed number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constant {
    pub value: f64,
}
