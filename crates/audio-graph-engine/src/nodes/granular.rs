use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, ParamCx};
use crate::ir::{
    AudioOp, GranularBinding, GranularSpec, MAX_GRANULAR_BINDINGS, MAX_GRANULAR_SECONDS,
};
pub use crate::ir::{GranularAction, GranularMode};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback};
use crate::nodes::{Beats, KeyTrigger, Node};
use crate::port::{Port, PortType};
#[cfg(feature = "ui")]
mod ui;

#[derive(Clone, Copy)]
enum ControlZone {
    Input,
    Output,
}

impl ControlZone {
    fn actions(self) -> &'static [(&'static str, GranularAction)] {
        match self {
            Self::Input => &[
                ("Record (hold)", GranularAction::Record),
                ("Reset", GranularAction::Reset),
                ("Invert loop recording", GranularAction::InvertLoop),
                ("Update ratio", GranularAction::Update(1.0)),
                ("History (beats)", GranularAction::History(2.0)),
                ("Block (beats)", GranularAction::Block(0.25)),
            ],
            Self::Output => &[
                ("Play", GranularAction::Play),
                ("Stop", GranularAction::Stop),
                ("Size / block", GranularAction::Size(0.5)),
                ("Interval / block", GranularAction::Interval(0.25)),
                ("Slice position", GranularAction::Position(0.0)),
                ("Wet", GranularAction::Wet(1.0)),
            ],
        }
    }

    fn accepts(self, action: GranularAction) -> bool {
        self.actions()
            .iter()
            .any(|(_, value)| std::mem::discriminant(value) == std::mem::discriminant(&action))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GranularBand {
    pub end: u8,
    pub action: GranularAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GranularKey {
    pub key: u8,
    #[serde(default)]
    pub mode: GranularMode,
    #[serde(default)]
    pub velocity: bool,
    pub bands: Vec<GranularBand>,
}

impl GranularKey {
    pub fn new(key: u8, action: GranularAction) -> Self {
        Self {
            key,
            mode: GranularMode::Hold,
            velocity: false,
            bands: vec![GranularBand { end: 127, action }],
        }
    }

    fn compile(&self, out: &mut Vec<GranularBinding>) -> Result<(), CompileError> {
        let invalid = || CompileError::InvalidSetting {
            what: "granular key or velocity partition",
        };
        if self.key > 127 || self.bands.is_empty() {
            return Err(invalid());
        }
        let count = if self.velocity { self.bands.len() } else { 1 };
        let mut start = 1;
        for band in &self.bands[..count] {
            if !band.action.valid() || (self.velocity && (band.end < start || band.end > 127)) {
                return Err(invalid());
            }
            out.push(GranularBinding {
                trigger: if self.velocity {
                    KeyTrigger::Velocity {
                        key: self.key,
                        min: start,
                        max: band.end,
                    }
                } else {
                    KeyTrigger::Key(self.key)
                },
                action: band.action,
                mode: self.mode,
            });
            start = band.end.saturating_add(1);
        }
        if self.velocity && self.bands.last().is_none_or(|band| band.end != 127) {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Granular {
    pub channels: u16,
    pub capacity: f64,
    #[serde(deserialize_with = "read_beats")]
    pub history: f64,
    #[serde(deserialize_with = "read_beats")]
    pub block: f64,
    pub looping: bool,
    pub update: f64,
    pub input_keys: Vec<GranularKey>,
    pub output_keys: Vec<GranularKey>,
    pub size: f64,
    pub interval: f64,
    pub position: f64,
    pub wet: f64,
    /// Fresh edit identities let same-value UI edits supersede MIDI and reject reports from another document.
    #[serde(skip, default = "fresh_revisions")]
    pub revisions: [u64; 8],
}

impl Default for Granular {
    fn default() -> Self {
        Self {
            channels: 2,
            capacity: 4.0,
            history: 2.0,
            block: 0.25,
            looping: true,
            update: 1.0,
            input_keys: vec![
                GranularKey::new(24, GranularAction::Record),
                GranularKey::new(25, GranularAction::Reset),
            ],
            output_keys: vec![
                GranularKey::new(26, GranularAction::Play),
                GranularKey::new(27, GranularAction::Stop),
                GranularKey::new(28, GranularAction::Size(0.5)),
                GranularKey::new(29, GranularAction::Interval(0.25)),
                GranularKey::new(30, GranularAction::Position(0.0)),
            ],
            size: 0.5,
            interval: 0.25,
            position: 0.0,
            wet: 0.5,
            revisions: fresh_revisions(),
        }
    }
}

fn fresh_revisions() -> [u64; 8] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    [NEXT.fetch_add(1, Ordering::Relaxed); 8]
}

fn read_beats<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Number(f64),
        Fraction(Beats),
    }
    Ok(match Value::deserialize(deserializer)? {
        Value::Number(value) => value,
        Value::Fraction(value) => value.value(),
    })
}

impl Granular {
    pub fn set_value(&mut self, action: GranularAction) {
        if let Some((target, value)) = action.parameter() {
            self.write_value(target, value);
            self.revisions[target] = fresh_revisions()[0];
        }
    }

    fn write_value(&mut self, target: usize, value: f64) {
        match target {
            0 => self.size = value,
            1 => self.interval = value,
            2 => self.position = value,
            3 => self.wet = value,
            5 => self.update = value,
            6 => self.history = value,
            7 => self.block = value,
            _ => {}
        }
    }

    pub fn sync_selection(&mut self, status: &crate::ir::GranularStatus) {
        for (target, value) in status.selected.iter().enumerate() {
            if self.revisions[target] == status.revisions[target]
                && let Some(value) = value
            {
                self.write_value(target, *value);
            }
        }
    }

    fn spec(&self) -> Result<GranularSpec, CompileError> {
        let mut bindings = Vec::new();
        let mut seen = 0u128;
        for (zone, key) in self
            .input_keys
            .iter()
            .map(|key| (ControlZone::Input, key))
            .chain(
                self.output_keys
                    .iter()
                    .map(|key| (ControlZone::Output, key)),
            )
        {
            if key.key > 127 || seen & (1u128 << key.key) != 0 {
                return Err(CompileError::InvalidSetting {
                    what: "duplicate granular key; use velocity bands on one key",
                });
            }
            seen |= 1u128 << key.key;
            let active_bands = if key.velocity { key.bands.len() } else { 1 };
            if key
                .bands
                .iter()
                .take(active_bands)
                .any(|band| !zone.accepts(band.action))
            {
                return Err(CompileError::InvalidSetting {
                    what: "granular operation belongs to the other control zone",
                });
            }
            key.compile(&mut bindings)?;
            if bindings.len() > MAX_GRANULAR_BINDINGS {
                return Err(CompileError::TooLarge {
                    what: "granular velocity bands",
                    limit: MAX_GRANULAR_BINDINGS,
                });
            }
        }
        Ok(GranularSpec {
            block_beats: self.block,
            history_beats: self.history,
            looping: self.looping,
            update: self.update,
            bindings,
            revisions: self.revisions,
        })
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
                "Audio In",
                PortType::Audio {
                    channels: self.channels,
                },
            ),
            Port::new("Keys", PortType::Note),
            Port::param("Dry / Wet"),
        ]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new(
            "Audio Out",
            PortType::Audio {
                channels: self.channels,
            },
        )]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let spec = self.spec()?;
        if !(1..=2).contains(&self.channels)
            || !self.capacity.is_finite()
            || !(0.001..=MAX_GRANULAR_SECONDS).contains(&self.capacity)
            || !spec.history_beats.is_finite()
            || spec.history_beats <= 0.0
            || spec.history_beats > 64.0
            || !spec.block_beats.is_finite()
            || spec.block_beats <= 0.0
            || spec.block_beats > 64.0
            || !self.update.is_finite()
            || !(0.0..=1.0).contains(&self.update)
            || self.values().iter().any(|value| !value.is_finite())
        {
            return Err(CompileError::InvalidSetting {
                what: "granular capacity, history, block, or parameter",
            });
        }
        if let Some(reg) = cx.input(2) {
            cx.drive_audio(2, reg)?;
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
            spec: self.spec()?,
            params: values,
            wet_lane: cx.lane(2),
        });
        cx.produce(0, out, input.map_or(0, |(_, late)| late));
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn title_controls(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let button = ui.small_button("⚙").on_hover_text("Granular settings");
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .layout(egui::Layout::top_down(egui::Align::Min))
            .width(360.0)
            .show(|ui| self.settings(ui, cx))
            .is_some_and(|response| response.inner)
    }

    #[cfg(feature = "ui")]
    fn after_inputs(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        let mut changed = self.input_zone(ui, cx);
        changed |= ui::key_table(
            ui,
            ControlZone::Output,
            &mut self.output_keys,
            &self.input_keys,
        );
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
        if port != 2 {
            return false;
        }
        let mut wet = self.wet;
        let changed = fallback(ui, connected, cx.input(port), &mut wet, |ui, value| {
            ui.add(egui::DragValue::new(value).speed(0.01).range(0.0..=1.0))
                .changed()
        });
        if changed {
            self.set_value(GranularAction::Wet(wet));
        }
        changed
    }
}

#[cfg(feature = "ui")]
impl Granular {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Self)> {
        vec![("Granular", Self::default())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_partitions_are_total_and_keys_are_unique_while_actions_may_repeat() {
        let mut node = Granular::default();
        node.input_keys
            .push(GranularKey::new(31, GranularAction::Record));
        assert!(node.spec().is_ok());
        node.input_keys[0].velocity = true;
        node.input_keys[0].bands = vec![
            GranularBand {
                end: 63,
                action: GranularAction::Record,
            },
            GranularBand {
                end: 127,
                action: GranularAction::Reset,
            },
        ];
        let spec = node.spec().unwrap();
        for velocity in 1..=127 {
            assert_eq!(
                spec.bindings
                    .iter()
                    .filter(|b| b.trigger.matches(24, f64::from(velocity) / 127.0))
                    .count(),
                1
            );
        }
        node.input_keys[0].bands[1].end = 62;
        assert!(node.spec().is_err());
        node.input_keys[0].bands[1].end = 127;
        node.output_keys[0].key = 24;
        assert!(node.spec().is_err());
    }

    #[test]
    fn the_seconds_based_prototype_is_not_silently_read_as_a_different_history() {
        let old = serde_json::json!({"history": 2.0, "record_key": 24, "play_key": 26});
        assert!(serde_json::from_value::<Granular>(old).is_err());
    }

    #[test]
    fn input_and_output_operations_are_disjoint_and_only_wet_has_a_parameter_socket() {
        for &(_, action) in ControlZone::Input.actions() {
            assert!(ControlZone::Input.accepts(action));
            assert!(!ControlZone::Output.accepts(action));
        }
        for &(_, action) in ControlZone::Output.actions() {
            assert!(ControlZone::Output.accepts(action));
            assert!(!ControlZone::Input.accepts(action));
        }
        let mut node = Granular::default();
        let ports = node.input_ports();
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[1].name.as_ref(), "Keys");
        assert_eq!(ports[2].name.as_ref(), "Dry / Wet");
        assert_eq!(ports[2].ty, PortType::Param);
        assert!(ports[..2].iter().all(|port| port.ty != PortType::Param));
        node.input_keys[0].bands[0].action = GranularAction::Play;
        assert!(node.spec().is_err());
        node.input_keys[0].bands[0].action = GranularAction::Record;
        node.output_keys[0].bands[0].action = GranularAction::History(1.0);
        assert!(node.spec().is_err());
    }
    #[test]
    fn saved_fractions_keep_their_beat_length_and_new_documents_reject_old_reports() {
        let node: Granular = serde_json::from_value(serde_json::json!({
            "history": { "num": 3, "den": 4, "triplet": true },
            "block": { "num": 1, "den": 64, "triplet": true }
        }))
        .unwrap();
        assert_eq!(node.history, 0.5);
        assert_eq!(node.block, 1.0 / 96.0);
        let restored: Granular =
            serde_json::from_value(serde_json::to_value(&node).unwrap()).unwrap();
        assert_ne!(restored.revisions, node.revisions);
        assert_eq!(restored.history, node.history);
        assert_eq!(restored.block, node.block);
    }
}
