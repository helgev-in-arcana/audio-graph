//! Graph node definitions and common trait interface.
//!
//! Each node type is implemented in its own submodule and exposed through
//! the [`Node`] trait and the [`NodeKind`] enum wrapper for static dispatch
//! and serialization support.

#[cfg(feature = "ui")]
pub mod widgets;

mod audio_io;
mod constant;
mod delay;
mod expression;
mod gate;
mod key_param;
mod key_switch;
mod lfo;
mod math;
mod mix;
mod note_gate;
mod note_in;
mod plugin;
mod range_map;
mod slot;
mod switch;

pub use audio_io::{AudioIn, AudioOut};
pub use constant::Constant;
pub use delay::{DelayRead, DelayWrite};
pub use expression::Expression;
pub use gate::Gate;
pub use key_param::{KeyParam, KeyParamMode};
pub use key_switch::{KeySwitch, KeySwitchMode};
pub use lfo::{Lfo, Rate};
pub use math::Math;
pub use mix::{Mix, db_to_linear, linear_to_db};
pub use note_gate::NoteGate;
pub use note_in::NoteIn;
pub use plugin::{ParamPort, Plugin, PluginPorts};
pub use range_map::RangeMap;
pub use slot::SlotIn;
pub use switch::Switch;

use serde::{Deserialize, Serialize};

/// Trait implemented by all graph node types.
///
/// Defines port layout, compilation hooks for parameter and audio passes,
/// note routing behavior, and optional UI rendering callbacks.
pub(crate) trait Node {
    fn title(&self) -> String;
    fn input_ports(&self) -> Vec<Port>;
    fn output_ports(&self) -> Vec<Port>;

    /// Pre-compilation declaration pass (e.g. declaring delay lines).
    fn declare(&self, cx: &mut DeclareCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// Compiles parameter processing operations into the parameter context.
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// Compiles audio processing operations into the audio context.
    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    /// Identifies the note stream originating from this node, if it is a note source.
    fn note_identity(&self) -> Option<NoteSource> {
        None
    }

    /// Maps an output note port back to its corresponding input note port for passthrough routing.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        let _ = port;
        None
    }

    /// Bitmask of MIDI note keys (0..=127) muted on the specified output port.
    fn note_mute(&self, port: u8) -> u128 {
        let _ = port;
        0
    }

    /// Renders node body UI controls and returns whether any state changed.
    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        let _ = (ui, cx);
        false
    }

    /// Returns the display title for the node UI header.
    #[cfg(feature = "ui")]
    fn ui_title(&self, cx: &widgets::NodeUi<'_>) -> String {
        let _ = cx;
        self.title()
    }

    /// Renders controls in the node's title bar.
    #[cfg(feature = "ui")]
    fn title_controls(&mut self, ui: &mut egui::Ui, cx: &mut widgets::NodeUi<'_>) -> bool {
        let _ = (ui, cx);
        false
    }

    /// Renders inline controls for an input port row.
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

    /// Renders inline controls for an output port row.
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

    /// Tooltip label for dynamically adding an output port, or `None` if fixed.
    #[cfg(feature = "ui")]
    fn add_output_label(&self) -> Option<&'static str> {
        None
    }

    /// Give this node another output. Only called when
    /// [`Node::add_output_label`] offered one.
    #[cfg(feature = "ui")]
    fn add_output(&mut self) {}

    /// Removes the output port at `port` and returns the number of removed ports.
    #[cfg(feature = "ui")]
    fn remove_output(&mut self, port: u8) -> u8 {
        let _ = port;
        0
    }

    /// Tooltip label for dynamically adding an input port group, or `None` if fixed.
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        None
    }

    /// Give this node another input group. Only called when
    /// [`Node::add_input_label`] offered one.
    #[cfg(feature = "ui")]
    fn add_input(&mut self) {}

    /// Removes the input port group at `port` and returns the number of removed ports.
    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let _ = port;
        0
    }
}

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::port::Port;
use subhost_adapter::NoteSource;

/// Enumeration of all graph node types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    Constant(Constant),
    SlotIn(SlotIn),
    Lfo(Lfo),
    Expression(Expression),
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
    DelayRead(DelayRead),
}

/// Dispatches a method or expression against the inner node struct of a [`NodeKind`].
macro_rules! for_kind {
    ($kind:expr, $node:ident => $body:expr) => {
        match $kind {
            NodeKind::Constant($node) => $body,
            NodeKind::SlotIn($node) => $body,
            NodeKind::Lfo($node) => $body,
            NodeKind::Expression($node) => $body,
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
            NodeKind::DelayRead($node) => $body,
        }
    };
}

impl NodeKind {
    /// Returns the input ports for this node kind.
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
    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        for_kind!(self, node => node.note_identity())
    }

    /// Where the notes leaving output `port` came in — see
    /// [`Node::note_passthrough`].
    pub(crate) fn note_passthrough(&self, port: u8) -> Option<u8> {
        for_kind!(self, node => node.note_passthrough(port))
    }

    /// The keys this node swallows on output `port` — see [`Node::note_mute`].
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

/// Node category in the editor's node creation catalog.
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

/// Returns the catalog of default node templates available for creation in the editor.
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
        Expression::catalogue_defaults(),
        NodeKind::Expression,
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
