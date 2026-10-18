use super::lines::Latch;
use super::notes::{NoteBuf, key_bit};
use crate::ir::{Operand, ParamBindings};

/// A node's inputs can alias or be scattered, so a borrowed lookup avoids
/// gathering values that a kernel may never read.
pub(crate) struct ParamInputs<'a> {
    registers: &'a [f64],
    bindings: &'a [Operand],
}

impl ParamInputs<'_> {
    pub(crate) fn read(&self, index: usize) -> f64 {
        match self.bindings[index] {
            Operand::Reg(reg) => self.registers[reg as usize],
            Operand::Value(value) => value,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bindings.len()
    }
}

pub(crate) struct ParamNotes<'a> {
    buffer: &'a NoteBuf,
}

impl ParamNotes<'_> {
    pub(crate) fn struck(&self, key: u8) -> bool {
        key_bit(i16::from(key)).is_some_and(|bit| self.buffer.struck & bit != 0)
    }
}

/// Only declared inputs, dedicated outputs and the node's own latch are
/// exposed, keeping kernel evaluation independent of global register IDs.
pub(crate) struct ParamEvalContext<'a> {
    pub inputs: ParamInputs<'a>,
    pub outputs: &'a mut [f64],
    pub latch: Option<&'a mut f64>,
    pub notes: Option<ParamNotes<'a>>,
}

impl<'a> ParamEvalContext<'a> {
    pub(super) fn new(
        bindings: &'a ParamBindings,
        registers: &'a mut [f64],
        latches: &'a mut [Latch],
        notes: &'a [NoteBuf],
    ) -> Self {
        let start = usize::from(bindings.outputs.start);
        let end = usize::from(bindings.outputs.end);
        let (inputs, outputs) = registers.split_at_mut(start);
        Self {
            inputs: ParamInputs {
                registers: inputs,
                bindings: &bindings.inputs,
            },
            outputs: &mut outputs[..end - start],
            latch: bindings.latch.map(|i| &mut latches[i as usize].value),
            notes: bindings.notes.map(|i| ParamNotes {
                buffer: &notes[i as usize],
            }),
        }
    }
}
