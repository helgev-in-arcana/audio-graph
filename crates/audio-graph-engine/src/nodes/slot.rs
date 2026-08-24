//! The two ends of the wrapper's slot table (§8).
//!
//! One node reads what the DAW is automating, the other takes the slot over.
//! Neither knows what the slot is bound to — that is the outer layer's
//! business, and it is what keeps this crate free of any idea of a plugin
//! parameter.

use serde::{Deserialize, Serialize};

/// The DAW's automation for one wrapper slot, 0..1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotIn {
    pub slot: usize,
}

/// Drive a wrapper slot, replacing the DAW's automation for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotOut {
    pub slot: usize,
}
