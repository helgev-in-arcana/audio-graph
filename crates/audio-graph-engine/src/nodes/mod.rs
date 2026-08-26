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
pub use mix::{Mix, db_to_linear, linear_to_db};
pub use note_in::NoteIn;
pub use plugin::{ParamPort, Plugin, PluginPorts};
pub use range_map::RangeMap;
pub use slot::SlotIn;

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
    ///
    /// What is left here is what belongs to the *node*: an LFO's waveform, a
    /// slot picker, a bus number. Anything that stands in for one socket
    /// belongs on that socket's row instead — see [`Node::input_control`].
    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        let _ = (ui, cx);
        false
    }

    /// The title as the canvas shows it.
    ///
    /// Separate from [`Node::title`] because a plugin node's name is the name
    /// of what is loaded in it, and this crate has no idea what that is until
    /// the wrapper hands it over.
    #[cfg(feature = "ui")]
    fn ui_title(&self, cx: &widgets::NodeUi<'_>) -> String {
        let _ = cx;
        self.title()
    }

    /// The controls that belong in this node's title bar, drawn to the left of
    /// the always-on toggle.
    ///
    /// Only for what is about the node as a whole rather than about one of its
    /// sockets: opening a sub-plugin's window is the only one so far.
    #[cfg(feature = "ui")]
    fn title_controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        let _ = (ui, cx);
        false
    }

    /// The control that stands in for input socket `port`, drawn on that
    /// socket's own row.
    ///
    /// A socket and the number it falls back to are one thing to the user —
    /// `Math`'s `b`, a `Mix`'s gain, a delay's time — and were two rows apart
    /// until they were drawn together. `connected` says whether anything is
    /// wired in; a fallback wraps itself in [`widgets::fallback`] to grey out
    /// when it is, while a control that still applies with a link in place
    /// (a plugin's choice of *which* parameter) ignores it.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        cx: &mut widgets::NodeUi<'_>,
    ) -> bool {
        let _ = (ui, port, connected, cx);
        false
    }

    /// The label for the button that gives this node another input, or `None`
    /// where the sockets are fixed. Drawn on the node's last row.
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        None
    }

    /// Give this node another input group. Only called when
    /// [`Node::add_input_label`] offered one.
    #[cfg(feature = "ui")]
    fn add_input(&mut self) {}

    /// Take away the input group beginning at `port`, and say how many sockets
    /// went with it — the canvas needs the count to slide the links into the
    /// sockets after it down by that much.
    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let _ = port;
        0
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
/// The one place the thirteen variants are listed. Every delegating method
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

    /// The title as the canvas shows it — see [`Node::ui_title`].
    #[cfg(feature = "ui")]
    pub fn ui_title(&self, cx: &widgets::NodeUi<'_>) -> String {
        // `for_kind!` binds mutably, and this takes `&self`. One arm of its
        // own is cheaper than a second macro.
        match self {
            NodeKind::Plugin(node) => node.ui_title(cx),
            other => other.title(),
        }
    }

    /// This node's title-bar controls — see [`Node::title_controls`].
    #[cfg(feature = "ui")]
    pub fn title_controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        for_kind!(self, node => node.title_controls(ui, cx))
    }

    /// The control on one input socket's row — see [`Node::input_control`].
    #[cfg(feature = "ui")]
    pub fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        cx: &mut widgets::NodeUi<'_>,
    ) -> bool {
        for_kind!(self, node => node.input_control(ui, port, connected, cx))
    }

    /// The label of this node's "another input" button, if it has one.
    #[cfg(feature = "ui")]
    pub fn add_input_label(&self) -> Option<&'static str> {
        match self {
            NodeKind::Mix(node) => node.add_input_label(),
            NodeKind::Plugin(node) => node.add_input_label(),
            _ => None,
        }
    }

    /// Give this node another input group.
    #[cfg(feature = "ui")]
    pub fn add_input(&mut self) {
        for_kind!(self, node => node.add_input())
    }

    /// Take away the input group at `port`, returning how many sockets went.
    #[cfg(feature = "ui")]
    pub fn remove_input(&mut self, port: u8) -> u8 {
        for_kind!(self, node => node.remove_input(port))
    }
}

/// What the editor's "add a node" menu offers, in the order it offers it.
///
/// A free function rather than a method: `catalogue_defaults` returns `Self`,
/// which would make [`Node`] un-object-safe, and it is not one entry per node
/// anyway — both halves of a delay arrive together through `Graph::add_delay`
/// and so are offered here not at all.
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
