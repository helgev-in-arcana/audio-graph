use serde::{Deserialize, Serialize};

use crate::nodes::Node;
use crate::port::Port;

/// A node this build could not read, kept as the JSON it was saved as.
///
/// What a patch saved by a newer AudioGraph holds when it has a kind of node
/// this one does not know, or settings for a known kind in a shape it cannot
/// parse. Refusing the whole patch over one such node would silence and lock
/// every other node in it; guessing at its sockets or behaviour would run
/// something the patch never said to run. So it stays where it was, runs
/// nothing, and is written back unchanged — see [`NodeKind::Unknown`] for how
/// the JSON gets in and out.
///
/// It has no sockets, because nothing says what they were. The links that
/// touched it are kept anyway ([`Graph::prune`] leaves them alone), so saving
/// the patch again loses none of them.
///
/// [`NodeKind::Unknown`]: crate::NodeKind::Unknown
/// [`Graph::prune`]: crate::Graph::prune
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Unknown(pub serde_json::Value);

impl Unknown {
    /// The kind the JSON names, when it is shaped the way a node kind is
    /// written — `{"Kind": {...}}` or `"Kind"`.
    pub fn kind_name(&self) -> Option<&str> {
        match &self.0 {
            serde_json::Value::String(name) => Some(name),
            serde_json::Value::Object(map) if map.len() == 1 => {
                map.keys().next().map(String::as_str)
            }
            _ => None,
        }
    }
}

impl Node for Unknown {
    fn title(&self) -> String {
        match self.kind_name() {
            Some(name) => format!("{name} (unknown)"),
            None => "Unknown node".into(),
        }
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut super::widgets::NodeUi<'_>) -> bool {
        ui.weak("Not readable by this version.\nKept as saved; does not run.");
        false
    }
}
