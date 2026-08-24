use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::Port;

/// A fixed number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constant {
    pub value: f64,
}

impl Node for Constant {
    fn title(&self) -> String {
        "Constant".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        cx.emit(Op::Const {
            out,
            value: self.value,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        ui.add(egui::Slider::new(&mut self.value, 0.0..=1.0))
            .changed()
    }
}

#[cfg(feature = "ui")]
impl Constant {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Constant)> {
        vec![("Constant", Constant { value: 0.5 })]
    }
}
