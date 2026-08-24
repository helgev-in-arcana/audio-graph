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

#[cfg(feature = "ui")]
pub mod widgets;

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

/// What every node is, in one declaration.
///
/// Before this existed, the answer to "what does a node have to do" was
/// spread over four `match` statements in three crates, and the only way to
/// find out was to add a variant and read the compiler's complaints. Now it is
/// here, and a new node is one file that implements this.
///
/// The defaults are the point of most of it. A `Constant` has no audio half, a
/// `Math` declares nothing, and only `NoteIn` is a source of notes — so those
/// nodes say nothing about any of it, and what is left in their files is what
/// makes them different from each other.
///
/// [`NodeKind`] stays an enum and keeps delegating through `for_kind!`. That
/// is deliberate, and ADR-14 records the trade: an enum keeps the
/// exhaustiveness check, the derived `Serialize`/`Deserialize`/`PartialEq`,
/// and static dispatch, at the cost of one line per node in the macro. A
/// `Box<dyn Node>` would buy third-party nodes and cost all four — plus a
/// public contract for the patch format, a receptacle for unknown kinds, and a
/// validation pass over `Program`, since an outside node could emit an
/// instruction stream the engine indexes without checking.
///
/// Not in here: `catalogue_defaults`, which returns `Self` and so would make
/// the trait un-object-safe for no gain — and which is not one-per-node
/// anyway, since `Mix` offers itself twice.
pub(crate) trait Node {
    fn title(&self) -> String;
    fn input_ports(&self) -> Vec<Port>;
    fn output_ports(&self) -> Vec<Port>;

    /// Say what has to be booked before anything is emitted — today, delay
    /// lines. Runs over the whole graph before either half compiles.
    fn declare(&self, cx: &mut DeclareCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// Emit the parameter half (§9.2).
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// Emit the audio half (§14.9).
    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// The note stream a plugin wired to this node's output plays from, if
    /// this node is a source of notes at all (§14.10).
    fn note_identity(&self) -> Option<NoteSource> {
        None
    }

    /// Draw this node's own controls, inside the frame the canvas laid out.
    /// Returns whether anything changed.
    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        let _ = (ui, cx);
        false
    }
}

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::ir::NoteSource;
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

    /// Say what this node needs booked before anything is emitted.
    pub(crate) fn declare(&self, cx: &mut DeclareCx) -> Result<(), CompileError> {
        for_kind!(self, node => node.declare(cx))
    }

    /// The note stream a plugin wired to this node's output plays from, if it
    /// is a source of notes at all (§14.10).
    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        for_kind!(self, node => node.note_identity())
    }

    /// Emit this node's parameter-half instructions (§9.2).
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        for_kind!(self, node => node.compile(cx))
    }

    /// Emit this node's audio-half instructions (§14.9).
    pub(crate) fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        for_kind!(self, node => node.compile_audio(cx))
    }

    /// Draw this node's own controls, inside the frame the canvas has already
    /// laid out. Returns whether anything changed.
    #[cfg(feature = "ui")]
    pub fn controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        for_kind!(self, node => node.controls(ui, cx))
    }
}

/// What the editor's "add a node" menu offers, in the order it offers it.
///
/// A free function rather than a method, because two entries can be the same
/// node: `Mix` and `Gain` differ only in their starting shape, and the menu is
/// the right place to say so. The delay pair is absent for the opposite
/// reason — both halves arrive together, through `Graph::add_delay`.
#[cfg(feature = "ui")]
pub fn catalogue() -> Vec<(&'static str, NodeKind)> {
    let mut out = Vec::new();
    fn take<T>(
        out: &mut Vec<(&'static str, NodeKind)>,
        entries: Vec<(&'static str, T)>,
        wrap: fn(T) -> NodeKind,
    ) {
        out.extend(entries.into_iter().map(|(name, node)| (name, wrap(node))));
    }
    take(&mut out, Constant::catalogue_defaults(), NodeKind::Constant);
    take(&mut out, SlotIn::catalogue_defaults(), NodeKind::SlotIn);
    take(&mut out, Lfo::catalogue_defaults(), NodeKind::Lfo);
    take(
        &mut out,
        Expression::catalogue_defaults(),
        NodeKind::Expression,
    );
    take(&mut out, Math::catalogue_defaults(), NodeKind::Math);
    take(&mut out, RangeMap::catalogue_defaults(), NodeKind::RangeMap);
    take(&mut out, SlotOut::catalogue_defaults(), NodeKind::SlotOut);
    take(&mut out, AudioIn::catalogue_defaults(), NodeKind::AudioIn);
    take(&mut out, AudioOut::catalogue_defaults(), NodeKind::AudioOut);
    out.extend(
        NoteIn::catalogue_defaults()
            .into_iter()
            .map(|(name, _)| (name, NodeKind::NoteIn)),
    );
    take(&mut out, Plugin::catalogue_defaults(), NodeKind::Plugin);
    take(
        &mut out,
        DelayWrite::catalogue_defaults(),
        NodeKind::DelayWrite,
    );
    take(&mut out, Mix::catalogue_defaults(), NodeKind::Mix);
    take(
        &mut out,
        DelayRead::catalogue_defaults(),
        NodeKind::DelayRead,
    );
    out
}
