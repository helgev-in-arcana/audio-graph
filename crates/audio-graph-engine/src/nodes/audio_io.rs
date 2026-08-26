//! The wrapper's own audio buses (§14).

use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError};
use crate::ir::AudioOp;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
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

/// Bus 0 is the signal path; the rest are aux, and are drawn as such (§14.11).
fn bus_port(name: &'static str, bus: usize, channels: u16) -> Port {
    let port = Port::new(name, PortType::Audio { channels });
    if bus == 0 { port } else { port.aux() }
}

// Audio nodes carry no param register at all. The audio pass walks the same
// order again and emits their half (§14.9).

impl Node for AudioIn {
    fn title(&self) -> String {
        format!("Audio In {}", self.bus + 1)
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![bus_port("out", self.bus, self.channels)]
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let out = cx.alloc(self.channels, cx.readers())?;
        cx.emit(AudioOp::Input {
            out,
            bus: self.bus as u16,
        });
        cx.produce(0, out, 0);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        bus_control(ui, &mut self.bus)
    }
}

#[cfg(feature = "ui")]
impl AudioIn {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, AudioIn)> {
        vec![(
            "Audio In",
            AudioIn {
                bus: 0,
                channels: 2,
            },
        )]
    }
}

/// The bus picker both audio nodes share.
#[cfg(feature = "ui")]
fn bus_control(ui: &mut egui::Ui, bus: &mut usize) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("bus");
        // One-based on screen: the DAW calls them "Main" and "Sidechain", not
        // "0" and "1".
        let mut shown = *bus as u32 + 1;
        if ui
            .add(egui::DragValue::new(&mut shown).range(1..=2))
            .changed()
        {
            *bus = (shown - 1) as usize;
            changed = true;
        }
        ui.weak(if *bus == 0 { "main" } else { "sidechain" });
    });
    changed
}

impl Node for AudioOut {
    fn title(&self) -> String {
        format!("Audio Out {}", self.bus + 1)
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![bus_port("in", self.bus, self.channels)]
    }

    fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        if let Some((buf, late)) = cx.source(0) {
            cx.report_latency(late);
            cx.consume(buf);
            cx.emit(AudioOp::Output {
                a: buf,
                bus: self.bus as u16,
            });
        }
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        bus_control(ui, &mut self.bus)
    }
}

#[cfg(feature = "ui")]
impl AudioOut {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, AudioOut)> {
        vec![(
            "Audio Out",
            AudioOut {
                bus: 0,
                channels: 2,
            },
        )]
    }
}
