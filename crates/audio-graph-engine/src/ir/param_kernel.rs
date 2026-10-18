use std::fmt::Debug;
use std::ops::Range;

use super::{NodeId, NoteBuf, Operand, Reg};
use crate::engine::ParamEvalContext;

/// Prepared node settings; all evolving state belongs to the context so that
/// end-row evaluation and program adoption can preserve it consistently.
/// Evaluation must not allocate, free, lock, or retain state through interior
/// mutability. Construction and destruction belong to the publishing thread.
pub(crate) trait ParamKernel: Debug + Send + Sync {
    fn eval(&self, cx: &mut ParamEvalContext<'_>);
}

#[derive(Debug)]
pub(crate) struct ParamBindings {
    pub inputs: Box<[Operand]>,
    /// Fresh registers after every input, allowing disjoint slice borrows.
    pub outputs: Range<Reg>,
    pub latch: Option<u16>,
    pub notes: Option<NoteBuf>,
}

#[derive(Debug)]
pub(crate) struct CustomParam {
    pub node: NodeId,
    pub bindings: ParamBindings,
    pub kernel: Box<dyn ParamKernel>,
}
