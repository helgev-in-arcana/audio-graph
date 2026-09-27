use serde::{Deserialize, Serialize};

use crate::ir::MAX_MERGE_INPUTS;
use crate::nodes::Node;
use crate::port::{Port, PortType};

fn two() -> u8 {
    2
}

/// Joins several note streams into one: two keyboards into one synth, or the
/// branches of a split back together.
///
/// A note that reaches it along two branches of one split is the same note,
/// and goes on once — see [`NoteOp::Merge`][crate::ir::NoteOp::Merge]. Two
/// different notes that happen to be on one key are two notes, and both go
/// on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteMerge {
    /// How many streams it joins, `2..=MAX_MERGE_INPUTS`.
    #[serde(default = "two")]
    pub inputs: u8,
}

impl NoteMerge {
    fn count(&self) -> u8 {
        self.inputs.clamp(1, MAX_MERGE_INPUTS as u8)
    }
}

impl Node for NoteMerge {
    fn title(&self) -> String {
        "MIDI Merge".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        (0..self.count())
            .map(|i| {
                let port = Port::new(format!("notes {}", i + 1), PortType::Note);
                #[cfg(feature = "ui")]
                let port = port.removable(self.count() > 2);
                port
            })
            .collect()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    fn note_merge(&self, port: u8) -> Vec<u8> {
        if port == 0 {
            (0..self.count()).collect()
        } else {
            Vec::new()
        }
    }

    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        (usize::from(self.count()) < MAX_MERGE_INPUTS).then_some("another stream")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        self.inputs = self.count() + 1;
    }

    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        if self.count() <= 2 || port >= self.count() {
            return 0;
        }
        self.inputs = self.count() - 1;
        1
    }
}

#[cfg(feature = "ui")]
impl NoteMerge {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteMerge)> {
        vec![("MIDI Merge", NoteMerge { inputs: 2 })]
    }
}
