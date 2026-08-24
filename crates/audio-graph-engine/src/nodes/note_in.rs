/// Notes arriving from the DAW.
use crate::port::{Port, PortType};
///
/// Carries nothing, and stays a unit variant of [`NodeKind`][crate::NodeKind]
/// for that reason: a newtype around an empty struct would spell itself
/// `{"NoteIn": null}` on disk instead of `"NoteIn"`, and patches already
/// saved say the latter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoteIn;

impl NoteIn {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    pub fn title(&self) -> String {
        "Note in".into()
    }
}
