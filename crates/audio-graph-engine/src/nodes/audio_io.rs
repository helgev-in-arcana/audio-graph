//! The wrapper's own audio buses (§14).

use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::ir::{AudioOp, NoteSource};
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
impl AudioIn {
    pub(crate) fn compile(&self, _cx: &mut ParamCx) -> Result<(), CompileError> {
        Ok(())
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

    pub(crate) fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let out = cx.alloc(self.channels, cx.readers())?;
        cx.emit(AudioOp::Input {
            out,
            bus: self.bus as u16,
        });
        cx.produce(0, out, 0);
        Ok(())
    }

    pub(crate) fn declare(&self, _cx: &mut DeclareCx) -> Result<(), CompileError> {
        Ok(())
    }

    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        None
    }
}

#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;

#[cfg(feature = "ui")]
impl AudioIn {
    pub fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        bus_control(ui, &mut self.bus)
    }

    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, AudioIn)> {
        vec![(
            "Audio in",
            AudioIn {
                bus: 0,
                channels: 2,
            },
        )]
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

    pub(crate) fn compile(&self, _cx: &mut ParamCx) -> Result<(), CompileError> {
        Ok(())
    }

    pub(crate) fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
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

    pub(crate) fn declare(&self, _cx: &mut DeclareCx) -> Result<(), CompileError> {
        Ok(())
    }

    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        None
    }
}

#[cfg(feature = "ui")]
impl AudioOut {
    pub fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        bus_control(ui, &mut self.bus)
    }

    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, AudioOut)> {
        vec![(
            "Audio out",
            AudioOut {
                bus: 0,
                channels: 2,
            },
        )]
    }
}
