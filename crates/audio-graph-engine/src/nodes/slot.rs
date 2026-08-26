//! The way into the wrapper's slot table (§8).
//!
//! One node, reading what the DAW is automating. It does not know what the
//! slot is bound to — that is the outer layer's business, and it is what
//! keeps this crate free of any idea of a plugin parameter.
//!
//! There was a second node here, `SlotOut`, which drove a slot from the graph
//! and so took it away from the DAW. It is gone: a slot the graph writes is a
//! lane the DAW is still drawing automation into, and the two fought — with
//! the graph winning silently, which is the worst way for that argument to
//! end. Everything it could do, editing the node that reads the slot already
//! does, and §14.12's parameter sockets are the honest route from the graph
//! to a sub-plugin parameter.

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
