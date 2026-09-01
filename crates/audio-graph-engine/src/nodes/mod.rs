//! Graph node definitions and common trait interface.
//!
//! One file per node. Everything a node *is* — its settings, its sockets, its
//! title, and the code the compiler emits for it — belongs in that file, so
//! that adding a node is reading one example rather than finding four places
//! that already mention every other node.
//!
//! [`NodeKind`] stays an enum, and stays the only way a node reaches the rest of
//! the crate. That is what keeps the exhaustiveness check, the derived
//! `Serialize` / `Deserialize` / `PartialEq`, and static dispatch: a node is not
//! a `Box<dyn …>` here, it is a variant carrying its own struct. The delegating
//! arms are the whole cost of that, and they are one line each.

#[cfg(feature = "ui")]
pub mod widgets;

mod audio_io;
mod cc_in;
mod constant;
mod delay;
mod gate;
mod key_param;
mod key_switch;
mod lfo;
mod math;
mod mix;
mod note_filter;
mod note_follow;
mod note_gate;
mod note_in;
mod note_mute;
mod param_to_cc;
mod plugin;
mod range_map;
mod slot;
mod switch;

pub use audio_io::{AudioIn, AudioOut};
pub use cc_in::CcIn;
pub use constant::Constant;
pub use delay::{DelayRead, DelayWrite};
pub use gate::Gate;
pub use key_param::{KeyParam, KeyParamMode};
pub use key_switch::{KeySwitch, KeySwitchMode};
pub use lfo::{Lfo, Rate};
pub use math::Math;
pub use mix::{Mix, db_to_linear, linear_to_db};
pub use note_filter::{FilterMode, NoteFilter};
pub use note_follow::NoteFollow;
pub use note_gate::NoteGate;
pub use note_in::NoteIn;
pub use note_mute::NoteMute;
pub use param_to_cc::ParamToCc;
pub use plugin::{ParamPort, Plugin, PluginPorts};
pub use range_map::RangeMap;
pub use slot::SlotIn;
pub use switch::Switch;

use serde::{Deserialize, Serialize};

/// What every node is, in one declaration.
///
/// Defines port layout, compilation hooks for the parameter and audio passes,
/// note routing behaviour, and optional UI rendering callbacks. A new node is
/// one file that implements this.
///
/// The defaults are the point of most of it. A `Constant` has no audio half, a
/// `Math` declares nothing, and only `NoteIn` is a source of notes — so those
/// nodes say nothing about any of it, and what is left in their files is what
/// makes them different from each other.
///
/// **Why an enum and not `Box<dyn Node>`.** [`NodeKind`] keeps delegating
/// through `for_kind!` on purpose: an enum keeps the exhaustiveness check, the
/// derived `Serialize`/`Deserialize`/`PartialEq`, and static dispatch, at the
/// cost of one line per node in that macro. Trait objects would buy third-party
/// nodes and cost all four — plus a public contract for the patch format, a
/// receptacle for unknown kinds, and a validation pass over `Program`, since an
/// outside node could emit an instruction stream the engine indexes without
/// checking.
///
/// Not in this trait: `catalogue_defaults`, which returns `Self` and so would
/// make the trait un-object-safe for no gain — and which is not one-per-node
/// anyway, since `Mix` offers itself twice.
pub(crate) trait Node {
    fn title(&self) -> String;
    fn input_ports(&self) -> Vec<Port>;
    fn output_ports(&self) -> Vec<Port>;

    /// Says what has to be booked before anything is emitted — today, delay
    /// lines. Runs over the whole graph before either half compiles.
    fn declare(&self, cx: &mut DeclareCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// Compiles parameter processing operations into the parameter context.
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// The channel and controller number this node adds to the stream leaving
    /// output `port`, when it makes controller events of its own.
    ///
    /// The value comes off the audio lane the node booked for its own input
    /// socket 0, so a node answering this must also call
    /// [`ParamCx::drive_audio`][crate::compile::ParamCx::drive_audio] for it.
    fn note_emits(&self, port: u8) -> Option<(u8, u8)> {
        let _ = port;
        None
    }

    /// Which MIDI channels the notes leaving output `port` are allowed on —
    /// bit `c` set means channel `c` passes.
    ///
    /// Defaults to all sixteen. A node with no opinion about channels must say
    /// so rather than say nothing, because a mask of zero is a stream that
    /// carries nothing.
    fn note_channels(&self, port: u8) -> u16 {
        let _ = port;
        crate::ir::ALL_CHANNELS
    }

    /// Which controller numbers survive output `port` — bit `n` set means
    /// controller `n` passes. Defaults to all 128, for the same reason.
    fn note_controllers(&self, port: u8) -> u128 {
        let _ = port;
        crate::ir::ALL_CONTROLLERS
    }

    /// Compiles audio processing operations into the audio context.
    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// The DAW note bus this node reads, if it is where notes come from.
    fn note_source(&self) -> Option<u16> {
        None
    }

    /// Which of this node's inputs the notes leaving output `port` came in
    /// through, for a node that passes notes on rather than making them.
    ///
    /// This is what makes a note node a filter: the compiler gives the output
    /// the buffer that arrived at `port`, or a copy of it with whatever this
    /// node refuses taken out. A node that answers neither this nor
    /// [`Node::note_source`] produces no note buffer, and a plugin behind it
    /// hears nothing.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        let _ = port;
        None
    }

    /// Whether the notes leaving output `port` pass only while a condition this
    /// node binds is open — see [`crate::compile::ParamCx::bind_note_gate`].
    ///
    /// Each gate applies its own condition to its own copy of the stream, so a
    /// node that merely hands notes on answers `false` and costs nothing.
    fn note_gated(&self, port: u8) -> bool {
        let _ = port;
        false
    }

    /// Which MIDI keys this node takes *out* of the stream leaving output
    /// `port` — bit `k` set means key `k` does not go on.
    ///
    /// A key switch's own keys are the case: they are played to steer, not to
    /// sound, and by default the thing being steered should never hear them.
    /// Several switches in series each swallow their own, because each is its
    /// own filter on the stream.
    fn note_mute(&self, port: u8) -> u128 {
        let _ = port;
        0
    }

    /// Draws this node's own controls, inside the frame the canvas laid out.
    /// Returns whether anything changed.
    ///
    /// What belongs here is what belongs to the *node*: an LFO's waveform, a
    /// slot picker, a bus number. Anything that stands in for one socket belongs
    /// on that socket's row instead — see [`Node::input_control`].
    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        let _ = (ui, cx);
        false
    }

    /// The title as the canvas shows it.
    ///
    /// Separate from [`Node::title`] because a plugin node's name is the name of
    /// what is loaded in it, and this crate has no idea what that is until the
    /// wrapper hands it over.
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
    /// when it is, while a control that still applies with a link in place (a
    /// plugin's choice of *which* parameter) ignores it.
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

    /// The control that stands in for output socket `port`, drawn on that
    /// socket's own row and right up against the socket.
    ///
    /// The mirror of [`Node::input_control`], and there for the same reason: a
    /// key switch's key belongs to the output it steers, and a node-wide list of
    /// keys somewhere else is a thing to match up by counting.
    #[cfg(feature = "ui")]
    fn output_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        cx: &mut widgets::NodeUi<'_>,
    ) -> bool {
        let _ = (ui, port, cx);
        false
    }

    /// The output side's [`Node::add_input_label`], and drawn the same way — as
    /// a "+", though against the edge the output sockets are on.
    #[cfg(feature = "ui")]
    fn add_output_label(&self) -> Option<&'static str> {
        None
    }

    /// Give this node another output. Only called when
    /// [`Node::add_output_label`] offered one.
    #[cfg(feature = "ui")]
    fn add_output(&mut self) {}

    /// Takes away output `port`, and says how many sockets went with it.
    #[cfg(feature = "ui")]
    fn remove_output(&mut self, port: u8) -> u8 {
        let _ = port;
        0
    }

    /// What the button that grows this node's inputs should say it adds, or
    /// `None` for a node whose inputs are fixed — or already at its ceiling.
    ///
    /// The button itself is drawn as "+", because it sits under the row it makes
    /// more of and the word was the wider half of it. This is the tooltip, so it
    /// reads as a thing rather than as a label: "another input", not "+ input".
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        None
    }

    /// Give this node another input group. Only called when
    /// [`Node::add_input_label`] offered one.
    #[cfg(feature = "ui")]
    fn add_input(&mut self) {}

    /// Takes away the input group beginning at `port`, and says how many
    /// sockets went with it — the canvas needs the count to slide the links in
    /// the sockets after it down by that much.
    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let _ = port;
        0
    }
}

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::port::Port;

/// One node's identity and settings.
///
/// Each variant is a newtype over the struct of the same name. That spelling is
/// not cosmetic: `{"Lfo": {"waveform": …}}` is exactly what a struct variant
/// wrote, so patches saved before the split reopen unchanged, and it is what
/// lets a node's whole implementation move into its own file without the enum
/// having to know any of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    Constant(Constant),
    SlotIn(SlotIn),
    Lfo(Lfo),
    NoteFollow(NoteFollow),
    Math(Math),
    RangeMap(RangeMap),
    Switch(Switch),

    // --- Audio & MIDI Nodes ---
    AudioIn(AudioIn),
    AudioOut(AudioOut),
    /// Carries nothing, so it stays a unit variant: see [`NoteIn`].
    NoteIn,
    Plugin(Plugin),
    DelayWrite(DelayWrite),
    Mix(Mix),
    Gate(Gate),
    NoteGate(NoteGate),
    KeySwitch(KeySwitch),
    KeyParam(KeyParam),
    NoteMute(NoteMute),
    NoteFilter(NoteFilter),
    ParamToCc(ParamToCc),
    CcIn(CcIn),
    DelayRead(DelayRead),
}

/// Runs `$body` against whichever node the kind is carrying.
///
/// The one place every variant is listed. Every delegating method below is one
/// line through here, so adding a node means adding an arm here and nothing else
/// in this file — and the exhaustiveness check still makes forgetting it a
/// compile error rather than a silent no-op.
///
/// `NoteIn` carries nothing and so has nothing to bind; the arm makes one on the
/// spot, which is free.
macro_rules! for_kind {
    ($kind:expr, $node:ident => $body:expr) => {
        match $kind {
            NodeKind::Constant($node) => $body,
            NodeKind::SlotIn($node) => $body,
            NodeKind::Lfo($node) => $body,
            NodeKind::NoteFollow($node) => $body,
            NodeKind::Math($node) => $body,
            NodeKind::RangeMap($node) => $body,
            NodeKind::Switch($node) => $body,
            NodeKind::AudioIn($node) => $body,
            NodeKind::AudioOut($node) => $body,
            NodeKind::NoteIn => {
                let $node = &mut NoteIn;
                $body
            }
            NodeKind::Plugin($node) => $body,
            NodeKind::DelayWrite($node) => $body,
            NodeKind::Mix($node) => $body,
            NodeKind::Gate($node) => $body,
            NodeKind::NoteGate($node) => $body,
            NodeKind::KeySwitch($node) => $body,
            NodeKind::KeyParam($node) => $body,
            NodeKind::NoteMute($node) => $body,
            NodeKind::NoteFilter($node) => $body,
            NodeKind::ParamToCc($node) => $body,
            NodeKind::CcIn($node) => $body,
            NodeKind::DelayRead($node) => $body,
        }
    };
}

impl NodeKind {
    /// This kind's input sockets, in order. Empty for a source node.
    ///
    /// Returns owned ports because a plugin node's sockets depend on what the
    /// plugin turned out to have, and so cannot be a static slice. Every caller
    /// is on the main thread — the audio thread sees only a `Program`.
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

    /// Identifies the note stream originating from this node, if it is a note source.
    pub(crate) fn note_source(&self) -> Option<u16> {
        for_kind!(self, node => node.note_source())
    }

    /// Where the notes leaving output `port` came in — see
    /// [`Node::note_passthrough`].
    pub(crate) fn note_passthrough(&self, port: u8) -> Option<u8> {
        for_kind!(self, node => node.note_passthrough(port))
    }

    /// Whether output `port` carries a gate of this node's own — see
    /// [`Node::note_gated`].
    pub(crate) fn note_gated(&self, port: u8) -> bool {
        for_kind!(self, node => node.note_gated(port))
    }

    /// The keys this node swallows on output `port` — see [`Node::note_mute`].
    pub(crate) fn note_emits(&self, port: u8) -> Option<(u8, u8)> {
        for_kind!(self, node => node.note_emits(port))
    }

    pub(crate) fn note_channels(&self, port: u8) -> u16 {
        for_kind!(self, node => node.note_channels(port))
    }

    pub(crate) fn note_controllers(&self, port: u8) -> u128 {
        for_kind!(self, node => node.note_controllers(port))
    }

    pub(crate) fn note_mute(&self, port: u8) -> u128 {
        for_kind!(self, node => node.note_mute(port))
    }

    /// Compiles parameter processing operations.
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        for_kind!(self, node => node.compile(cx))
    }

    /// Compiles audio processing operations.
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

    /// The control on one output socket's row — see [`Node::output_control`].
    #[cfg(feature = "ui")]
    pub fn output_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        cx: &mut widgets::NodeUi<'_>,
    ) -> bool {
        for_kind!(self, node => node.output_control(ui, port, cx))
    }

    /// The label of this node's "another output" button, if it has one.
    #[cfg(feature = "ui")]
    pub fn add_output_label(&self) -> Option<&'static str> {
        for_kind!(self, node => node.add_output_label())
    }

    /// Give this node another output.
    #[cfg(feature = "ui")]
    pub fn add_output(&mut self) {
        for_kind!(self, node => node.add_output())
    }

    /// Take away output `port`, returning how many sockets went.
    #[cfg(feature = "ui")]
    pub fn remove_output(&mut self, port: u8) -> u8 {
        for_kind!(self, node => node.remove_output(port))
    }

    /// The label of this node's "another input" button, if it has one.
    #[cfg(feature = "ui")]
    pub fn add_input_label(&self) -> Option<&'static str> {
        for_kind!(self, node => node.add_input_label())
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

/// Which half of the graph a node belongs to, for the "add a node" menu.
///
/// These three are the three kinds of wire the editor has, so they are the three
/// piles a reader is already sorting the nodes into. Without them the menu is a
/// wall of buttons in which "Param Map" sits beside "MIDI In".
///
/// A node is filed by what it is *for*, not by every socket it owns: a gate
/// takes a parameter to decide with, but it is an audio node because audio is
/// what comes out the other side.
#[cfg(feature = "ui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeGroup {
    /// Sound in, sound out.
    Audio,
    /// Notes in, notes out.
    Note,
    /// Numbers: what drives the slots and the sub-plugins' parameters.
    Param,
}

#[cfg(feature = "ui")]
impl NodeGroup {
    /// The three, in the order the menu lists them.
    pub const ALL: [NodeGroup; 3] = [NodeGroup::Audio, NodeGroup::Note, NodeGroup::Param];

    pub fn label(self) -> &'static str {
        match self {
            NodeGroup::Audio => "Audio",
            NodeGroup::Note => "MIDI",
            NodeGroup::Param => "Parameter",
        }
    }
}

/// What the editor's "add a node" menu offers, in the order it offers it.
///
/// A free function rather than a method: `catalogue_defaults` returns `Self`,
/// which would make [`Node`] un-object-safe, and it is not one entry per node
/// anyway — both halves of a delay arrive together through `Graph::add_delay`
/// and so are offered here not at all.
#[cfg(feature = "ui")]
pub fn catalogue() -> Vec<(NodeGroup, &'static str, NodeKind)> {
    let mut out = Vec::new();
    fn take<T>(
        out: &mut Vec<(NodeGroup, &'static str, NodeKind)>,
        group: NodeGroup,
        entries: Vec<(&'static str, T)>,
        wrap: fn(T) -> NodeKind,
    ) {
        out.extend(
            entries
                .into_iter()
                .map(|(name, node)| (group, name, wrap(node))),
        );
    }

    // Audio.
    take(
        &mut out,
        NodeGroup::Audio,
        AudioIn::catalogue_defaults(),
        NodeKind::AudioIn,
    );
    take(
        &mut out,
        NodeGroup::Audio,
        AudioOut::catalogue_defaults(),
        NodeKind::AudioOut,
    );
    take(
        &mut out,
        NodeGroup::Audio,
        Mix::catalogue_defaults(),
        NodeKind::Mix,
    );
    take(
        &mut out,
        NodeGroup::Audio,
        Gate::catalogue_defaults(),
        NodeKind::Gate,
    );
    take(
        &mut out,
        NodeGroup::Audio,
        Plugin::catalogue_defaults(),
        NodeKind::Plugin,
    );
    take(
        &mut out,
        NodeGroup::Audio,
        DelayWrite::catalogue_defaults(),
        NodeKind::DelayWrite,
    );
    take(
        &mut out,
        NodeGroup::Audio,
        DelayRead::catalogue_defaults(),
        NodeKind::DelayRead,
    );

    // MIDI.
    out.extend(
        NoteIn::catalogue_defaults()
            .into_iter()
            .map(|(name, _)| (NodeGroup::Note, name, NodeKind::NoteIn)),
    );
    take(
        &mut out,
        NodeGroup::Note,
        NoteGate::catalogue_defaults(),
        NodeKind::NoteGate,
    );
    take(
        &mut out,
        NodeGroup::Note,
        KeySwitch::catalogue_defaults(),
        NodeKind::KeySwitch,
    );
    take(
        &mut out,
        NodeGroup::Note,
        NoteMute::catalogue_defaults(),
        NodeKind::NoteMute,
    );
    take(
        &mut out,
        NodeGroup::Note,
        NoteFilter::catalogue_defaults(),
        NodeKind::NoteFilter,
    );
    take(
        &mut out,
        NodeGroup::Note,
        ParamToCc::catalogue_defaults(),
        NodeKind::ParamToCc,
    );
    take(
        &mut out,
        NodeGroup::Note,
        CcIn::catalogue_defaults(),
        NodeKind::CcIn,
    );
    take(
        &mut out,
        NodeGroup::Note,
        NoteFollow::catalogue_defaults(),
        NodeKind::NoteFollow,
    );

    // Parameter.
    take(
        &mut out,
        NodeGroup::Param,
        Constant::catalogue_defaults(),
        NodeKind::Constant,
    );
    take(
        &mut out,
        NodeGroup::Param,
        SlotIn::catalogue_defaults(),
        NodeKind::SlotIn,
    );
    take(
        &mut out,
        NodeGroup::Param,
        Lfo::catalogue_defaults(),
        NodeKind::Lfo,
    );
    take(
        &mut out,
        NodeGroup::Param,
        Math::catalogue_defaults(),
        NodeKind::Math,
    );
    take(
        &mut out,
        NodeGroup::Param,
        RangeMap::catalogue_defaults(),
        NodeKind::RangeMap,
    );
    take(
        &mut out,
        NodeGroup::Param,
        Switch::catalogue_defaults(),
        NodeKind::Switch,
    );
    take(
        &mut out,
        NodeGroup::Param,
        KeyParam::catalogue_defaults(),
        NodeKind::KeyParam,
    );
    out
}
