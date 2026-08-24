//! The wrapper's own audio buses (§14).

use serde::{Deserialize, Serialize};

/// Audio arriving from the DAW on one of the wrapper's own input buses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioIn {
    pub bus: usize,
    pub channels: u16,
}

/// Audio leaving for the DAW on one of the wrapper's own output buses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioOut {
    pub bus: usize,
    pub channels: u16,
}
