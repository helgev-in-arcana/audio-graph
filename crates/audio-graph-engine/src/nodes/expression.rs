use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
pub use crate::ir::ExprSource;
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo};
use crate::port::Port;

/// A note expression, reduced to one value (see [`ExprSource`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expression {
    pub source: ExprSource,
}

impl Node for Expression {
    fn title(&self) -> String {
        self.source.label().into()
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        cx.emit(Op::Expr {
            out,
            source: self.source,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let changed = combo(
            ui,
            "source",
            &mut self.source,
            &ExprSource::ALL,
            ExprSource::label,
        );
        if self.source.is_per_note() && !cx.poly_modulation {
            // §3.3 asks for per-voice sources to be greyed out when the
            // sub-plugin cannot take per-voice modulation. In v1 the graph is
            // monophonic, so these still do something useful — they are just
            // flattened. Saying so is more use than disabling a control that
            // works.
            ui.colored_label(egui::Color32::from_rgb(200, 160, 70), "newest note only")
                .on_hover_text(
                    "the sub-plugin cannot take per-voice modulation, so every \
                     held note contributes to one value",
                );
        }
        changed
    }
}

#[cfg(feature = "ui")]
impl Expression {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Expression)> {
        vec![(
            "Expression",
            Expression {
                source: ExprSource::Pressure,
            },
        )]
    }
}
