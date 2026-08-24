//! One hosted sub-plugin, and the layout it turned out to have.

use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::ParamTarget;
use crate::port::{Port, PortType};

/// One sub-plugin parameter the graph is allowed to drive.
///
/// A plugin node does not get a socket per parameter — Chroma has 2106 of them.
/// The user picks which ones to expose, exactly as they pick slot bindings
/// today (§8.3), and each pick becomes a port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamPort {
    /// The sub-plugin's own id for the parameter. A plain `u32` because the
    /// common data model is CLAP-shaped (ADR-4) — nothing here is VST3.
    pub id: u32,
    pub name: String,
}

/// A sub-plugin's port layout, as discovered after loading (§14.2).
///
/// Cached in the graph rather than asked for on demand. A patch has to reopen
/// with the right shape *before* its plugins have finished loading, and a node
/// whose plugin has gone missing still has to draw with the links it had.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginPorts {
    /// Channel count of each input bus. Bus 0 is main; the rest are aux
    /// (sidechain). Discovered from what the plugin *accepted*, not from what
    /// was asked for.
    #[serde(default)]
    pub audio_in: Vec<u16>,
    #[serde(default)]
    pub audio_out: Vec<u16>,
    #[serde(default)]
    pub accepts_notes: bool,
    #[serde(default)]
    pub params: Vec<ParamPort>,
    /// The plugin's reported latency, in samples.
    ///
    /// Discovered after loading like everything else here, and re-read when the
    /// plugin says `kLatencyChanged`. The compiler needs it to line up parallel
    /// paths (§14.6) and to work out how short a feedback loop may be (§14.4),
    /// so a change to it means a recompile.
    #[serde(default)]
    pub latency: u32,
}

impl PluginPorts {
    /// Build a node's ports from what a loaded plugin reported (§14.2).
    ///
    /// `params` is deliberately left empty. The parameter sockets are the
    /// user's choice, not the plugin's: a compressor with 90 parameters would
    /// otherwise arrive as a node with 90 sockets. The editor adds them one at
    /// a time.
    ///
    /// Widths are clamped to [`MAX_CHANNELS`][crate::MAX_CHANNELS]. M8 is
    /// stereo throughout (§14.8), and a node drawn with a socket the compiler
    /// will refuse is worse than one drawn narrow.
    pub fn from_layout(layout: &plugin_host_api::IoLayout, latency: u32) -> PluginPorts {
        let widths = |buses: &[plugin_host_api::BusInfo]| -> Vec<u16> {
            buses
                .iter()
                .map(|b| b.channels.min(crate::MAX_CHANNELS as u16))
                .filter(|&c| c > 0)
                .collect()
        };
        PluginPorts {
            audio_in: widths(&layout.inputs),
            audio_out: widths(&layout.outputs),
            accepts_notes: layout.accepts_notes,
            params: Vec::new(),
            latency,
        }
    }
}

/// One hosted sub-plugin.
///
/// `instance` indexes the wrapper's table of loaded sub-plugins, the same
/// way `slot` indexes the slot table: which file that is, and how it was
/// bound, stays outside the graph (§8.3). `ports` is the layout that was
/// discovered after loading (§14.2), cached here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    pub instance: usize,
    pub ports: PluginPorts,
}

impl Plugin {
    /// Main audio, then aux (sidechain), then notes if it takes them, then one
    /// socket per exposed parameter.
    ///
    /// The order matters more than it looks: it is what link indices mean, so
    /// inserting a category in the middle would re-point every saved link.
    /// Grow it only at the end.
    pub fn input_ports(&self) -> Vec<Port> {
        let mut out = Vec::new();
        for (i, &channels) in self.ports.audio_in.iter().enumerate() {
            let name = match i {
                0 => "in".to_string(),
                1 => "sidechain".to_string(),
                _ => format!("aux {i}"),
            };
            let port = Port::new(name, PortType::Audio { channels });
            out.push(if i == 0 { port } else { port.aux() });
        }
        if self.ports.accepts_notes {
            out.push(Port::new("notes", PortType::Note));
        }
        for param in &self.ports.params {
            out.push(Port::param(param.name.clone()));
        }
        out
    }

    pub fn output_ports(&self) -> Vec<Port> {
        self.ports
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
            .collect()
    }

    pub fn title(&self) -> String {
        format!("Plugin {}", self.instance + 1)
    }
}

impl Plugin {
    /// The param half of a plugin node is only its parameter sockets; the
    /// audio pass walks the same order again and emits the rest (§14.9).
    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        // A plugin node's parameter sockets sit after its audio inputs and its
        // notes port. Only the ones with something wired to them cost anything.
        let first = self.ports.audio_in.len() + usize::from(self.ports.accepts_notes);
        for (index, param) in self.ports.params.iter().enumerate() {
            let Some(reg) = cx.input((first + index) as u8) else {
                continue;
            };
            cx.drive_param(
                ParamTarget {
                    instance: self.instance as u32,
                    param: param.id,
                },
                reg,
            )?;
        }
        Ok(())
    }
}
