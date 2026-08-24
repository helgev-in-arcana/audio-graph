//! The wrapper's own audio buses (§14).

use serde::{Deserialize, Serialize};

use crate::port::{Port, PortType};

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

impl AudioIn {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![bus_port("out", self.bus, self.channels)]
    }

    pub fn title(&self) -> String {
        format!("Audio in {}", self.bus + 1)
    }
}

impl AudioOut {
    pub fn input_ports(&self) -> Vec<Port> {
        vec![bus_port("in", self.bus, self.channels)]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn title(&self) -> String {
        format!("Audio out {}", self.bus + 1)
    }
}

/// Bus 0 is the signal path; the rest are aux, and are drawn as such (§14.11).
fn bus_port(name: &'static str, bus: usize, channels: u16) -> Port {
    let port = Port::new(name, PortType::Audio { channels });
    if bus == 0 { port } else { port.aux() }
}
