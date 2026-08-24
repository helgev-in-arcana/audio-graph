//! The node set (§9.3 for the v1 core, §14 for the M8 additions).
//!
//! One file per node. Everything a node *is* — its settings, its sockets, its
//! title, and in time the code the compiler emits for it — belongs in that
//! file, so that adding a node is reading one example rather than finding
//! four places that already mention every other node.
//!
//! [`NodeKind`] stays an enum, and stays the only way a node reaches the rest
//! of the crate. That is what keeps the exhaustiveness check, the derived
//! `Serialize` / `Deserialize` / `PartialEq`, and static dispatch: a node is
//! not a `Box<dyn …>` here, it is a variant carrying its own struct. The arms
//! below are the whole cost of that, and they are one line each.

mod audio_io;
mod constant;
mod delay;
mod expression;
mod lfo;
mod math;
mod mix;
mod note_in;
mod plugin;
mod range_map;
mod slot;

pub use audio_io::{AudioIn, AudioOut};
pub use constant::Constant;
pub use delay::{DelayRead, DelayWrite};
pub use expression::Expression;
pub use lfo::{Lfo, Rate};
pub use math::Math;
pub use mix::Mix;
pub use note_in::NoteIn;
pub use plugin::{ParamPort, Plugin, PluginPorts};
pub use range_map::RangeMap;
pub use slot::{SlotIn, SlotOut};

use serde::{Deserialize, Serialize};

use crate::port::Port;

/// One node's identity and settings.
///
/// Each variant is a newtype over the struct of the same name. That spelling
/// is not cosmetic: `{"Lfo": {"waveform": …}}` is exactly what a struct
/// variant wrote, so patches saved before the split reopen unchanged, and it
/// is what lets a node's whole implementation move into its own file without
/// the enum having to know any of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    Constant(Constant),
    SlotIn(SlotIn),
    Lfo(Lfo),
    Expression(Expression),
    Math(Math),
    RangeMap(RangeMap),
    SlotOut(SlotOut),

    // --- M8 (§14) ---
    AudioIn(AudioIn),
    AudioOut(AudioOut),
    /// Carries nothing, so it stays a unit variant: see [`NoteIn`].
    NoteIn,
    Plugin(Plugin),
    DelayWrite(DelayWrite),
    Mix(Mix),
    DelayRead(DelayRead),
}
/// Run `$body` against whichever node the kind is carrying.
///
/// The one place the fourteen variants are listed. Every delegating method
/// below is one line through here, so adding a node means adding an arm here
/// and nothing else in this file — and the exhaustiveness check still makes
/// forgetting it a compile error rather than a silent no-op.
///
/// `NoteIn` carries nothing and so has nothing to bind; the arm makes one on
/// the spot, which is free.
macro_rules! for_kind {
    ($kind:expr, $node:ident => $body:expr) => {
        match $kind {
            NodeKind::Constant($node) => $body,
            NodeKind::SlotIn($node) => $body,
            NodeKind::Lfo($node) => $body,
            NodeKind::Expression($node) => $body,
            NodeKind::Math($node) => $body,
            NodeKind::RangeMap($node) => $body,
            NodeKind::SlotOut($node) => $body,
            NodeKind::AudioIn($node) => $body,
            NodeKind::AudioOut($node) => $body,
            NodeKind::NoteIn => {
                let $node = &mut NoteIn;
                $body
            }
            NodeKind::Plugin($node) => $body,
            NodeKind::DelayWrite($node) => $body,
            NodeKind::Mix($node) => $body,
            NodeKind::DelayRead($node) => $body,
        }
    };
}

impl NodeKind {
    /// This kind's input sockets, in order. Empty for a source node.
    ///
    /// Returns owned ports because a plugin node's sockets depend on what the
    /// plugin turned out to have (§14.2) and so cannot be a static slice. Every
    /// caller is on the main thread — the audio thread sees only a `Program`.
    pub fn input_ports(&self) -> Vec<Port> {
        for_kind!(self, node => node.input_ports())
    }

    /// This kind's output sockets, in order. Empty for a sink node.
    pub fn output_ports(&self) -> Vec<Port> {
        for_kind!(self, node => node.output_ports())
    }

    pub fn title(&self) -> String {
        for_kind!(self, node => node.title())
    }
}
