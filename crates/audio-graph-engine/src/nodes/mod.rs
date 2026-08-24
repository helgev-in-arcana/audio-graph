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

use crate::port::{Port, PortType};

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
impl NodeKind {
    /// This kind's input sockets, in order. Empty for a source node.
    ///
    /// Returns owned ports because a plugin node's sockets depend on what the
    /// plugin turned out to have (§14.2) and so cannot be a static slice. Every
    /// caller is on the main thread — the audio thread sees only a `Program`.
    pub fn input_ports(&self) -> Vec<Port> {
        match self {
            NodeKind::Constant(Constant { .. })
            | NodeKind::SlotIn(SlotIn { .. })
            | NodeKind::Lfo(Lfo { .. })
            | NodeKind::Expression(Expression { .. })
            | NodeKind::AudioIn(AudioIn { .. })
            | NodeKind::NoteIn => Vec::new(),
            // The one input a `DelayRead` has is its own delay time (§14.5).
            // It is a param, never audio, so it cannot close a loop through
            // the line it belongs to — the type check in `check_links` is what
            // makes that true rather than a convention.
            NodeKind::DelayRead(DelayRead { .. }) => vec![Port::param("time")],
            NodeKind::Math(Math { .. }) => vec![Port::param("a"), Port::param("b")],
            NodeKind::RangeMap(RangeMap { .. }) | NodeKind::SlotOut(SlotOut { .. }) => {
                vec![Port::param("in")]
            }
            NodeKind::AudioOut(AudioOut { bus, channels }) => {
                let port = Port::new(
                    "in",
                    PortType::Audio {
                        channels: *channels,
                    },
                );
                vec![if *bus == 0 { port } else { port.aux() }]
            }
            NodeKind::DelayWrite(DelayWrite { ty, .. }) => vec![Port::new("in", *ty)],
            // Each input next to its own gain, rather than all the signals
            // followed by all the gains: they are one row of one control on
            // screen, and a socket list that does not read that way makes the
            // user count.
            NodeKind::Mix(Mix {
                channels, inputs, ..
            }) => (0..*inputs)
                .flat_map(|i| {
                    [
                        Port::new(
                            format!("in {}", i + 1),
                            PortType::Audio {
                                channels: *channels,
                            },
                        ),
                        Port::param(format!("gain {}", i + 1)),
                    ]
                })
                .collect(),
            NodeKind::Plugin(Plugin { ports, .. }) => plugin_input_ports(ports),
        }
    }

    /// This kind's output sockets, in order. Empty for a sink node.
    pub fn output_ports(&self) -> Vec<Port> {
        match self {
            NodeKind::SlotOut(SlotOut { .. })
            | NodeKind::AudioOut(AudioOut { .. })
            | NodeKind::DelayWrite(DelayWrite { .. }) => Vec::new(),
            NodeKind::Constant(Constant { .. })
            | NodeKind::SlotIn(SlotIn { .. })
            | NodeKind::Lfo(Lfo { .. })
            | NodeKind::Expression(Expression { .. })
            | NodeKind::Math(Math { .. })
            | NodeKind::RangeMap(RangeMap { .. }) => vec![Port::param("out")],
            NodeKind::AudioIn(AudioIn { bus, channels }) => {
                let port = Port::new(
                    "out",
                    PortType::Audio {
                        channels: *channels,
                    },
                );
                vec![if *bus == 0 { port } else { port.aux() }]
            }
            NodeKind::NoteIn => vec![Port::new("out", PortType::Note)],
            NodeKind::DelayRead(DelayRead { ty, .. }) => vec![Port::new("out", *ty)],
            NodeKind::Mix(Mix { channels, .. }) => {
                vec![Port::new(
                    "out",
                    PortType::Audio {
                        channels: *channels,
                    },
                )]
            }
            NodeKind::Plugin(Plugin { ports, .. }) => ports
                .audio_out
                .iter()
                .enumerate()
                .map(|(i, &channels)| {
                    let name = if i == 0 {
                        "out".to_string()
                    } else {
                        format!("out {}", i + 1)
                    };
                    Port::new(name, PortType::Audio { channels })
                })
                .collect(),
        }
    }

    pub fn title(&self) -> String {
        match self {
            NodeKind::Constant(Constant { .. }) => "Constant".into(),
            NodeKind::SlotIn(SlotIn { slot }) => format!("Slot {} in", slot + 1),
            NodeKind::Lfo(Lfo { .. }) => "LFO".into(),
            NodeKind::Expression(Expression { source }) => source.label().into(),
            NodeKind::Math(Math { op, .. }) => op.label().into(),
            NodeKind::RangeMap(RangeMap { .. }) => "Range map".into(),
            NodeKind::SlotOut(SlotOut { slot }) => format!("Slot {} out", slot + 1),
            NodeKind::AudioIn(AudioIn { bus, .. }) => format!("Audio in {}", bus + 1),
            NodeKind::AudioOut(AudioOut { bus, .. }) => format!("Audio out {}", bus + 1),
            NodeKind::NoteIn => "Note in".into(),
            NodeKind::Plugin(Plugin { instance, .. }) => format!("Plugin {}", instance + 1),
            NodeKind::DelayWrite(DelayWrite { line, .. }) => format!("Delay {} write", line + 1),
            NodeKind::DelayRead(DelayRead { line, .. }) => format!("Delay {} read", line + 1),
            NodeKind::Mix(Mix { .. }) => "Mix".into(),
        }
    }
}

/// A plugin node's inputs: main audio, then aux (sidechain), then notes if it
/// takes them, then one socket per exposed parameter.
///
/// The order matters more than it looks: it is what link indices mean, so
/// inserting a category in the middle would re-point every saved link. Grow it
/// only at the end.
fn plugin_input_ports(ports: &PluginPorts) -> Vec<Port> {
    let mut out = Vec::new();
    for (i, &channels) in ports.audio_in.iter().enumerate() {
        let name = match i {
            0 => "in".to_string(),
            1 => "sidechain".to_string(),
            _ => format!("aux {i}"),
        };
        let port = Port::new(name, PortType::Audio { channels });
        out.push(if i == 0 { port } else { port.aux() });
    }
    if ports.accepts_notes {
        out.push(Port::new("notes", PortType::Note));
    }
    for param in &ports.params {
        out.push(Port::param(param.name.clone()));
    }
    out
}
