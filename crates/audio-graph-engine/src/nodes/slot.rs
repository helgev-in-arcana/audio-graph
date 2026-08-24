//! The two ends of the wrapper's slot table (§8).
//!
//! One node reads what the DAW is automating, the other takes the slot over.
//! Neither knows what the slot is bound to — that is the outer layer's
//! business, and it is what keeps this crate free of any idea of a plugin
//! parameter.

use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, slot_picker};
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

impl Node for SlotIn {
    fn title(&self) -> String {
        format!("Slot {} in", self.slot + 1)
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
        vec![("Slot in", SlotIn { slot: 0 })]
    }
}

impl Node for SlotOut {
    fn title(&self) -> String {
        format!("Slot {} out", self.slot + 1)
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("in")]
    }

    fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        cx.check_slot(self.slot)?;
        cx.claim_slot(self.slot)?;
        // An output with nothing plugged in is not an error - it is a node the
        // user has placed and not yet wired. It just does not take the slot
        // over from the DAW.
        if let Some(reg) = cx.input(0) {
            cx.drive_slot(self.slot, reg);
        }
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        slot_picker(ui, &mut self.slot, cx)
    }
}

#[cfg(feature = "ui")]
impl SlotOut {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, SlotOut)> {
        vec![("Slot out", SlotOut { slot: 0 })]
    }
}
