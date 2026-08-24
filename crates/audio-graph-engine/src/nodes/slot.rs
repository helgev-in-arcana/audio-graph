//! The two ends of the wrapper's slot table (§8).
//!
//! One node reads what the DAW is automating, the other takes the slot over.
//! Neither knows what the slot is bound to — that is the outer layer's
//! business, and it is what keeps this crate free of any idea of a plugin
//! parameter.

use serde::{Deserialize, Serialize};

use crate::port::Port;

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

impl SlotIn {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        format!("Slot {} in", self.slot + 1)
    }
}

impl SlotOut {
    pub fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("in")]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn title(&self) -> String {
        format!("Slot {} out", self.slot + 1)
    }
}
