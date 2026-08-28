//! Hosted sub-plugin node and its port configuration.

use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, Buf, MAX_AUX_BUSES};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{CAUTION, NodeAction, NodeUi, shorten};
use crate::port::{Port, PortType};
use subhost_adapter::{InstanceIo, ParamTarget};

/// One sub-plugin parameter the graph is allowed to drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamPort {
    /// Unique parameter identifier within the hosted sub-plugin.
    pub id: u32,
    pub name: String,
}

/// Cached I/O port configuration and parameter layout of a hosted sub-plugin.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginPorts {
    /// Channel count for each input audio bus (bus 0 is main, subsequent buses are auxiliary/sidechain).
    #[serde(default)]
    pub audio_in: Vec<u16>,
    #[serde(default)]
    pub audio_out: Vec<u16>,
    /// List of output bus indices exposed as output sockets in socket order.
    /// If empty, defaults to exposing all available output buses.
    #[serde(default)]
    pub audio_out_shown: Vec<u16>,
    #[serde(default)]
    pub accepts_notes: bool,
    #[serde(default)]
    pub params: Vec<ParamPort>,
    /// Processing latency in samples reported by the hosted sub-plugin.
    #[serde(default)]
    pub latency: u32,
}

/// Graph node representing a hosted sub-plugin instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    pub instance: usize,
    pub ports: PluginPorts,
}

impl PluginPorts {
    /// Constructs port configuration from a loaded plugin's I/O layout and latency.
    pub fn from_layout(layout: &plugin_host::IoLayout, latency: u32) -> PluginPorts {
        let widths = |buses: &[plugin_host::BusInfo]| -> Vec<u16> {
            buses
                .iter()
                .map(|b| b.channels.min(crate::MAX_CHANNELS as u16))
                .filter(|&c| c > 0)
                .collect()
        };
        let audio_out = widths(&layout.outputs);
        PluginPorts {
            // Initialize with only the primary output bus (bus 0) exposed.
            audio_out_shown: if audio_out.is_empty() {
                Vec::new()
            } else {
                vec![0]
            },
            audio_in: widths(&layout.inputs),
            audio_out,
            accepts_notes: layout.accepts_notes,
            params: Vec::new(),
            latency,
        }
    }

    /// Returns the valid output bus indices for all exposed output ports.
    pub fn shown_outputs(&self) -> Vec<u16> {
        if self.audio_out_shown.is_empty() {
            return (0..self.audio_out.len() as u16).collect();
        }
        self.audio_out_shown
            .iter()
            .copied()
            .filter(|&bus| usize::from(bus) < self.audio_out.len())
            .collect()
    }

    /// `audio_out_shown`, written out first if it was standing for "all".
    #[cfg(feature = "ui")]
    fn shown_mut(&mut self) -> &mut Vec<u16> {
        if self.audio_out_shown.is_empty() {
            self.audio_out_shown = self.shown_outputs();
        }
        &mut self.audio_out_shown
    }
}

fn out_name(bus: u16) -> String {
    if bus == 0 {
        "out".to_string()
    } else {
        format!("out {}", bus + 1)
    }
}

impl Node for Plugin {
    fn title(&self) -> String {
        format!("Plugin {}", self.instance + 1)
    }

    /// Main audio, then aux (sidechain), then notes if it takes them, then one
    /// socket per exposed parameter.
    ///
    /// The order matters more than it looks: it is what link indices mean, so
    /// inserting a category in the middle would re-point every saved link.
    /// Grow it only at the end.
    fn input_ports(&self) -> Vec<Port> {
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
            let port = Port::param(param.name.clone());
            // Parameter ports can be dynamically removed.
            #[cfg(feature = "ui")]
            let port = port.removable();
            out.push(port);
        }
        out
    }

    fn output_ports(&self) -> Vec<Port> {
        let shown = self.ports.shown_outputs();
        let many = shown.len() > 1;
        shown
            .iter()
            .map(|&bus| {
                let channels = self.ports.audio_out[usize::from(bus)];
                let port = Port::new(out_name(bus), PortType::Audio { channels });
                // Allow output removal only when multiple outputs are shown.
                #[cfg(feature = "ui")]
                let port = if many { port.removable() } else { port };
                let _ = many;
                port
            })
            .collect()
    }

    // Emits parameter drive commands for connected parameter input ports.
    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        // Parameter ports begin after audio inputs and the optional note port.
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

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let out_width = cx.out_width();
        if out_width == 0 {
            // Skip compilation if the plugin has no output channels configured.
            return Ok(());
        }

        // Determine the highest actively connected input bus index.
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

        // Collect input buffers for each active input bus, inserting silence for unconnected buses.
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

        // Use buffer directly if single bus matches width, otherwise gather into a contiguous buffer.
        let total: u16 = buses.iter().sum();
        let input = match parts.as_slice() {
            [] => {
                // Synthesizers/instruments with no inputs receive a silent buffer matching output width.
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

        // Determine the highest output bus that has active readers.
        let shown = self.ports.shown_outputs();
        if cx.outputs_read() > shown.len() {
            // Error if graph tries to read a port beyond shown output ports.
            return Err(CompileError::TypeMismatch {
                node: cx.node(),
                port: (cx.outputs_read() - 1) as u8,
            });
        }
        let out_wired = shown
            .iter()
            .enumerate()
            .filter(|&(port, _)| cx.readers_of(port as u8) > 0)
            .map(|(_, &bus)| usize::from(bus) + 1)
            .max()
            .unwrap_or(0);
        if out_wired > 1 + MAX_AUX_BUSES {
            return Err(CompileError::TooLarge {
                what: "output buses read from one plugin",
                limit: 1 + MAX_AUX_BUSES,
            });
        }
        // Always allocate at least the primary output bus.
        let out_buses: Vec<u16> = self.ports.audio_out[..out_wired.max(1)].to_vec();
        let out_total: u16 = out_buses.iter().sum();

        // Single output bus writes directly to the destination buffer.
        let single = out_buses.len() == 1;
        let readers = cx.readers();
        let output = cx.alloc_avoiding(out_total, if single { readers } else { 1 }, &[input])?;
        let notes = cx.note_route(self.ports.audio_in.len() as u8);
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
            // Produce single output directly on port reading bus 0.
            let port = shown.iter().position(|&bus| bus == 0).unwrap_or(0);
            cx.produce(port as u8, output, latency);
            return Ok(());
        }
        // Where each bus starts in the plugin's output region.
        let mut offset = Vec::with_capacity(out_buses.len());
        let mut channel = 0u16;
        for &width in &out_buses {
            offset.push(channel);
            channel += width;
        }
        for (port, &bus) in shown.iter().enumerate() {
            let readers = cx.readers_of(port as u8);
            // Skip splitting buses that have no downstream readers.
            if readers == 0 || usize::from(bus) >= out_buses.len() {
                continue;
            }
            let width = out_buses[usize::from(bus)];
            let buf = cx.alloc_avoiding(width, readers, &[input, output])?;
            cx.emit(AudioOp::Split {
                from: output,
                out: buf,
                channel: offset[usize::from(bus)],
                width,
            });
            cx.produce(port as u8, buf, latency);
        }
        cx.consume(output);
        Ok(())
    }

    /// Formats the UI title showing instance index and loaded plugin name.
    #[cfg(feature = "ui")]
    fn ui_title(&self, cx: &NodeUi<'_>) -> String {
        match cx.instances.get(self.instance) {
            Some(view) if view.loaded && !view.name.is_empty() => {
                format!("{}: {}", self.instance + 1, view.name)
            }
            _ => self.title(),
        }
    }

    /// Title bar button to toggle opening/closing the plugin's custom GUI window.
    #[cfg(feature = "ui")]
    fn title_controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let Some(view) = cx.instances.get(self.instance).cloned() else {
            return false;
        };
        if !view.loaded {
            return false;
        }
        // Toggle button for plugin GUI window.
        let hint = if view.editor_open {
            "close the plugin's window"
        } else {
            "open the plugin's window"
        };
        // Keep button framed in both active and inactive states.
        if ui
            .add(
                egui::Button::new("GUI")
                    .small()
                    .selected(view.editor_open)
                    .frame(true)
                    .frame_when_inactive(true),
            )
            .on_hover_text(hint)
            .clicked()
        {
            cx.act(if view.editor_open {
                NodeAction::CloseSubEditor(self.instance)
            } else {
                NodeAction::OpenSubEditor(self.instance)
            });
        }
        false
    }

    /// Renders the node body controls, displaying a warning if the plugin is not loaded.
    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let loaded = cx.instances.get(self.instance).is_some_and(|v| v.loaded);
        if !loaded {
            ui.colored_label(CAUTION, "not loaded").on_hover_text(
                "the plugin could not be found, or is still loading. Its links and \
                 parameter sockets are preserved.",
            );
        }
        false
    }

    /// Renders parameter selection dropdown on a parameter input socket row.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        _connected: bool,
        cx: &mut NodeUi<'_>,
    ) -> bool {
        let first = self.ports.audio_in.len() + usize::from(self.ports.accepts_notes);
        let Some(index) = (port as usize).checked_sub(first) else {
            return false;
        };
        let Some(param) = self.ports.params.get_mut(index) else {
            return false;
        };
        // Clone parameter list to avoid borrow conflicts with `self`.
        let available = cx
            .instances
            .get(self.instance)
            .map(|view| view.params.clone())
            .unwrap_or_default();
        let mut changed = false;
        let label = if param.name.is_empty() {
            format!("#{}", param.id)
        } else {
            param.name.clone()
        };
        // Fill remaining row width to scale appropriately with canvas zoom.
        egui::ComboBox::from_id_salt(("param", index))
            .selected_text(shorten(&label))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for (id, name) in &available {
                    if ui.selectable_label(*id == param.id, name).clicked() && *id != param.id {
                        param.id = *id;
                        param.name = name.clone();
                        changed = true;
                    }
                }
                if available.is_empty() {
                    ui.weak("load a plugin to choose");
                }
            });
        changed
    }

    /// Renders output bus selector when multiple output buses are available.
    #[cfg(feature = "ui")]
    fn output_control(&mut self, ui: &mut egui::Ui, port: u8, _cx: &mut NodeUi<'_>) -> bool {
        if self.ports.audio_out.len() < 2 {
            return false;
        }
        let shown = self.ports.shown_outputs();
        let Some(&bus) = shown.get(usize::from(port)) else {
            return false;
        };
        let mut picked = None;
        // Exclude buses that are already assigned to another output socket.
        egui::ComboBox::from_id_salt(("out-bus", port))
            .selected_text(out_name(bus))
            .width(ui.available_width().min(72.0))
            .show_ui(ui, |ui| {
                for candidate in 0..self.ports.audio_out.len() as u16 {
                    if candidate != bus && shown.contains(&candidate) {
                        continue;
                    }
                    if ui
                        .selectable_label(candidate == bus, out_name(candidate))
                        .clicked()
                        && candidate != bus
                    {
                        picked = Some(candidate);
                    }
                }
            });
        let Some(candidate) = picked else {
            return false;
        };
        self.ports.shown_mut()[usize::from(port)] = candidate;
        true
    }

    /// Tooltip label for adding another output bus socket.
    #[cfg(feature = "ui")]
    fn add_output_label(&self) -> Option<&'static str> {
        (self.ports.shown_outputs().len() < self.ports.audio_out.len())
            .then_some("a socket for another output bus")
    }

    #[cfg(feature = "ui")]
    fn add_output(&mut self) {
        let shown = self.ports.shown_outputs();
        // Pick the lowest unassigned output bus index.
        let Some(next) = (0..self.ports.audio_out.len() as u16).find(|b| !shown.contains(b)) else {
            return;
        };
        self.ports.shown_mut().push(next);
    }

    #[cfg(feature = "ui")]
    fn remove_output(&mut self, port: u8) -> u8 {
        let index = usize::from(port);
        if self.ports.shown_outputs().len() <= 1 || index >= self.ports.shown_outputs().len() {
            return 0;
        }
        self.ports.shown_mut().remove(index);
        1
    }

    /// Tooltip label for adding an additional parameter input socket.
    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        Some("a socket for another parameter")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        // Append a new unassigned parameter socket with default ID 0.
        self.ports.params.push(ParamPort {
            id: 0,
            name: String::new(),
        });
    }

    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let first = self.ports.audio_in.len() + usize::from(self.ports.accepts_notes);
        let Some(index) = (port as usize).checked_sub(first) else {
            return 0;
        };
        if index >= self.ports.params.len() {
            return 0;
        }
        self.ports.params.remove(index);
        1
    }
}

#[cfg(feature = "ui")]
impl Plugin {
    /// Plugin nodes are instantiated via the plugin browser rather than default catalog templates.
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Plugin)> {
        Vec::new()
    }
}
