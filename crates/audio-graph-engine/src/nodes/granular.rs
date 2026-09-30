use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{AudioOp, GranularParam, GranularSpec, MAX_GRANULAR_SECONDS};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, beats_control, fallback, key_trigger_control};
use crate::nodes::{Beats, KeyTrigger, Node};
use crate::port::{Port, PortType};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Granular {
    pub channels: u16,
    pub capacity: f64,
    pub history: f64,
    pub block: Beats,
    pub looping: bool,
    pub update: f64,
    pub latch: bool,
    pub record_key: KeyTrigger,
    pub reset_key: KeyTrigger,
    pub play_key: KeyTrigger,
    pub stop_key: KeyTrigger,
    pub size: f64,
    pub interval: f64,
    pub position: f64,
    pub wet: f64,
}

impl Default for Granular {
    fn default() -> Self {
        Self {
            channels: 2,
            capacity: 4.0,
            history: 1.0,
            block: Beats::new(1, 4),
            looping: true,
            update: 1.0,
            latch: false,
            record_key: 24.into(),
            reset_key: 25.into(),
            play_key: 26.into(),
            stop_key: 27.into(),
            size: 0.5,
            interval: 0.25,
            position: 0.0,
            wet: 0.5,
        }
    }
}

impl Granular {
    fn spec(&self) -> GranularSpec {
        GranularSpec {
            block_beats: self.block.value(),
            history: self.history,
            looping: self.looping,
            update: self.update,
            latch: self.latch,
            keys: [
                self.record_key,
                self.reset_key,
                self.play_key,
                self.stop_key,
            ],
        }
    }

    fn values(&self) -> [f64; 4] {
        [self.size, self.interval, self.position, self.wet]
    }
}

impl Node for Granular {
    fn title(&self) -> String {
        "Granular".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![
            Port::new(
                "in",
                PortType::Audio {
                    channels: self.channels,
                },
            ),
            Port::new("keys", PortType::Note),
            Port::param("size / block"),
            Port::param("interval / block"),
            Port::param("slice position"),
            Port::param("wet"),
        ]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new(
            "out",
            PortType::Audio {
                channels: self.channels,
            },
        )]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let spec = self.spec();
        if !(1..=2).contains(&self.channels)
            || !self.capacity.is_finite()
            || !(0.001..=MAX_GRANULAR_SECONDS).contains(&self.capacity)
            || !self.history.is_finite()
            || !(0.001..=self.capacity).contains(&self.history)
            || !spec.block_beats.is_finite()
            || spec.block_beats <= 0.0
            || !self.update.is_finite()
            || !(0.0..=1.0).contains(&self.update)
            || self.values().iter().any(|v| !v.is_finite())
            || spec.keys.iter().any(|key| !key.valid())
            || (0..4).any(|i| (i + 1..4).any(|j| spec.keys[i].overlaps(spec.keys[j])))
        {
            return Err(CompileError::InvalidSetting {
                what: "granular capacity, history, block, or overlapping keys",
            });
        }
        for port in 2..6 {
            if let Some(reg) = cx.input(port) {
                cx.drive_audio(port, reg)?;
            }
        }
        Ok(())
    }

    fn compile_audio(&self, cx: &mut AudioCx) -> Result<(), CompileError> {
        let state = cx.granular_state(self.capacity)?;
        let input = cx.source_at_socket_width(0)?;
        if let Some((buf, _)) = input {
            cx.consume(buf);
        }
        let out = cx.alloc(self.channels, cx.readers())?;
        let values = self.values();
        cx.emit(AudioOp::Granular {
            out,
            a: input.map(|(buf, _)| buf),
            notes: cx.note_source_of(1),
            state,
            spec: self.spec(),
            params: std::array::from_fn(|i| GranularParam {
                value: values[i],
                lane: cx.lane(i as u8 + 2),
            }),
        });
        cx.produce(0, out, input.map_or(0, |(_, late)| late));
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("block (beats)");
            changed |= beats_control(ui, "block", &mut self.block);
        });
        for (label, value, max) in [
            ("capacity (s)", &mut self.capacity, MAX_GRANULAR_SECONDS),
            ("history (s)", &mut self.history, MAX_GRANULAR_SECONDS),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                changed |= ui
                    .add(egui::DragValue::new(value).speed(0.01).range(0.001..=max))
                    .changed();
            });
        }
        if self.history > self.capacity {
            self.history = self.capacity;
            changed = true;
        }
        changed |= ui.checkbox(&mut self.looping, "loop recording").changed();
        ui.horizontal(|ui| {
            ui.label("update on wrap");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.update)
                        .speed(0.01)
                        .range(0.0..=1.0),
                )
                .changed();
        });
        changed |= ui.checkbox(&mut self.latch, "latch playback").changed();
        for (label, key) in [
            ("record", &mut self.record_key),
            ("reset", &mut self.reset_key),
            ("play", &mut self.play_key),
            ("stop", &mut self.stop_key),
        ] {
            ui.push_id(label, |ui| {
                changed |= key_trigger_control(ui, label, key);
            });
        }
        ui.weak("Capacity edits clear audio. History applies on record.");
        ui.weak("Recorded audio is not saved with the patch.");
        changed
    }

    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        cx: &mut NodeUi<'_>,
    ) -> bool {
        let (value, min, max) = match port {
            2 => (&mut self.size, 0.05, 1.0),
            3 => (&mut self.interval, 0.05, 1.0),
            4 => (&mut self.position, 0.0, 1.0),
            5 => (&mut self.wet, 0.0, 1.0),
            _ => return false,
        };
        fallback(ui, connected, cx.input(port), value, |ui, value| {
            ui.add(egui::DragValue::new(value).speed(0.01).range(min..=max))
                .changed()
        })
    }
}

#[cfg(feature = "ui")]
impl Granular {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Self)> {
        vec![("Granular", Self::default())]
    }
}
