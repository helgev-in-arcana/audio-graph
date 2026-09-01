use crate::nodes::Node;
use crate::port::{Port, PortType};

/// MIDI note stream input from the host DAW.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoteIn;

impl Node for NoteIn {
    fn title(&self) -> String {
        "MIDI In".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    /// The DAW's primary note bus.
    fn note_source(&self) -> Option<u16> {
        Some(0)
    }
}

#[cfg(feature = "ui")]
impl NoteIn {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteIn)> {
        vec![("MIDI In", NoteIn)]
    }
}
