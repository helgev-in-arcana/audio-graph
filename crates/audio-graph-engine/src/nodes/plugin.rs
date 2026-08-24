//! One hosted sub-plugin, and the layout it turned out to have.

use serde::{Deserialize, Serialize};

use crate::compile::AudioCx;
use crate::compile::{CompileError, ParamCx};
use crate::ir::ParamTarget;
use crate::ir::{AudioOp, Buf, InstanceIo, MAX_AUX_BUSES};
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

impl Plugin {
    pub(crate) fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let out_width = cx.out_width();
        if out_width == 0 {
            // A plugin with no output bus cannot be routed through. It is still
            // legal to place — an analyser is one — but there is nothing
            // downstream of it to compile.
            return Ok(());
        }

        // Which input buses the graph actually feeds (§14.11). A sidechain
        // nobody wired is left off entirely rather than activated and fed
        // silence: a compressor with an active, silent sidechain ducks to
        // nothing.
        let mut wired = self.ports.audio_in.len();
        while wired > 1 && cx.source(wired - 1).is_none() {
            wired -= 1;
        }
        if wired > 1 + MAX_AUX_BUSES {
            return Err(CompileError::TooLarge {
                what: "aux input buses on one plugin",
                limit: 1 + MAX_AUX_BUSES,
            });
        }
        let buses: Vec<u16> = self.ports.audio_in[..wired].to_vec();

        // One buffer per bus, at the width the plugin wants. An unwired bus
        // before a wired one still needs something to read.
        let mut in_latency = 0u32;
        let mut parts: Vec<(Buf, u16)> = Vec::with_capacity(buses.len());
        for (index, &width) in buses.iter().enumerate() {
            match cx.source(index) {
                Some((buf, late)) => {
                    in_latency = in_latency.max(late);
                    parts.push((buf, width));
                }
                None => {
                    let silent = cx.alloc(width, 1)?;
                    cx.emit(AudioOp::Silence { out: silent });
                    parts.push((silent, width));
                }
            }
        }

        // One bus at the right width already is the plugin's input region;
        // anything else has to be assembled. Skipping the copy in the common
        // case matters — most plugins are one stereo bus.
        let total: u16 = buses.iter().sum();
        let input = match parts.as_slice() {
            [] => {
                // An instrument. It is still handed a buffer, because the
                // caller's slice has to point somewhere.
                let silent = cx.alloc(out_width, 1)?;
                cx.emit(AudioOp::Silence { out: silent });
                silent
            }
            [(buf, width)] if cx.width_of(*buf) == *width => *buf,
            _ => {
                let avoid: Vec<Buf> = parts.iter().map(|&(b, _)| b).collect();
                let out = cx.alloc_avoiding(total, 1, &avoid)?;
                cx.emit(AudioOp::Gather {
                    out,
                    buses: parts.clone(),
                });
                out
            }
        };
        for (buf, _) in &parts {
            cx.consume(*buf);
        }
        if !parts.iter().any(|&(b, _)| b == input) {
            cx.consume(input);
        }

        // The same question on the way out (§14.2): a plugin's extra output
        // buses are handed over only as far as the graph reads them, so Surge
        // XT's `Scene B` costs nothing in a patch that ignores it.
        let out_wired = cx.outputs_read();
        if out_wired > self.ports.audio_out.len() {
            // A socket the node does not have. `connect` cannot make this link,
            // so the patch was hand-edited or written by a later version.
            return Err(CompileError::TypeMismatch {
                node: cx.node(),
                port: (out_wired - 1) as u8,
            });
        }
        if out_wired > 1 + MAX_AUX_BUSES {
            return Err(CompileError::TooLarge {
                what: "output buses read from one plugin",
                limit: 1 + MAX_AUX_BUSES,
            });
        }
        // At least the main bus, even when nothing reads it: a plugin still has
        // to be given somewhere to write.
        let out_buses: Vec<u16> = self.ports.audio_out[..out_wired.max(1)].to_vec();
        let out_total: u16 = out_buses.iter().sum();

        // One bus is the overwhelmingly common case and stays exactly as it
        // was: the plugin writes straight into the buffer the next node reads.
        // Only a patch that reads a second bus pays for the split.
        let single = out_buses.len() == 1;
        let readers = cx.readers();
        let output = cx.alloc_avoiding(out_total, if single { readers } else { 1 }, &[input])?;
        let notes = cx.note_source(self.ports.audio_in.len() as u8);
        cx.emit(AudioOp::Plugin {
            instance: self.instance as u32,
            input,
            input_buses: buses.clone(),
            output,
            output_buses: out_buses.clone(),
            notes,
        });
        cx.declare_instance(InstanceIo {
            instance: self.instance as u32,
            input_channels: buses.first().copied().unwrap_or(0),
            aux_inputs: buses.get(1..).unwrap_or(&[]).to_vec(),
            output_channels: out_buses[0],
            aux_outputs: out_buses.get(1..).unwrap_or(&[]).to_vec(),
        });

        let latency = in_latency + self.ports.latency;
        if single {
            cx.produce(0, output, latency);
            return Ok(());
        }
        let mut channel = 0u16;
        for (port, &width) in out_buses.iter().enumerate() {
            let readers = cx.readers_of(port as u8);
            // A bus in the middle that nobody reads still occupies its channels
            // in the plugin's output region; it just never gets copied out of
            // it.
            if readers > 0 {
                let buf = cx.alloc_avoiding(width, readers, &[input, output])?;
                cx.emit(AudioOp::Split {
                    from: output,
                    out: buf,
                    channel,
                    width,
                });
                cx.produce(port as u8, buf, latency);
            }
            channel += width;
        }
        cx.consume(output);
        Ok(())
    }
}
