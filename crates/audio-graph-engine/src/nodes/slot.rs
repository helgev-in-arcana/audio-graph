//! Wrapper slot automation input node.
//!
//! Reads normalized [0.0, 1.0] automation values from the host DAW's automation slots.

use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, slot_picker};
use crate::port::Port;

/// Reads normalized [0.0, 1.0] automation for a wrapper slot from the DAW.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotIn {
    pub slot: usize,
}

impl Node for SlotIn {
    fn title(&self) -> String {
        format!("Slot {} In", self.slot + 1)
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        cx.check_slot(self.slot)?;
        let out = cx.alloc()?;
        cx.emit(Op::Slot {
            out,
            slot: self.slot as u16,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        slot_picker(ui, &mut self.slot, cx)
    }
}

#[cfg(feature = "ui")]
impl SlotIn {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, SlotIn)> {
        vec![("Slot In", SlotIn { slot: 0 })]
    }
}
