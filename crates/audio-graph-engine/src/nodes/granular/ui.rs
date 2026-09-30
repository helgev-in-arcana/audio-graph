use super::*;
use crate::nodes::widgets::key_control;

impl Granular {
    pub(super) fn input_zone(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        ui.separator();
        if let Some(status) = cx.granular_status {
            ui.label(format!(
                "Recorded {:.3} / {:.3} s",
                status.recorded_seconds, status.history_seconds
            ));
            ui.label(format!(
                "Record: {}  Play: {}",
                if status.recording { "on" } else { "off" },
                if status.playing { "on" } else { "off" }
            ));
        } else {
            ui.label("Recorded — / — s");
            ui.label("Record: —  Play: —");
        }
        ui.strong("Input control");
        let changed = key_table(
            ui,
            ControlZone::Input,
            &mut self.input_keys,
            &self.output_keys,
        );
        ui.separator();
        ui.strong("Output control");
        changed
    }

    pub(super) fn settings(&mut self, ui: &mut egui::Ui, cx: &mut NodeUi<'_>) -> bool {
        ui.strong("Granular settings");
        ui.weak("Base values (UI / MIDI Select)");
        let mut changed = false;
        egui::Grid::new("granular-settings")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Capacity (s)");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.capacity)
                            .speed(0.01)
                            .range(0.001..=MAX_GRANULAR_SECONDS),
                    )
                    .changed();
                ui.end_row();
                for (label, mut action) in [
                    ("History (beats)", GranularAction::History(self.history)),
                    ("Block size (beats)", GranularAction::Block(self.block)),
                    ("Update on wrap", GranularAction::Update(self.update)),
                    ("Size / block", GranularAction::Size(self.size)),
                    ("Interval / block", GranularAction::Interval(self.interval)),
                    ("Slice position", GranularAction::Position(self.position)),
                    ("Dry / Wet", GranularAction::Wet(self.wet)),
                ] {
                    ui.label(label);
                    if ui
                        .push_id(label, |ui| number_control(ui, &mut action))
                        .inner
                    {
                        self.set_value(action);
                        changed = true;
                    }
                    ui.end_row();
                }
                ui.label("Loop recording");
                changed |= ui.checkbox(&mut self.looping, "").changed();
                ui.end_row();
            });
        if let Some(status) = cx.granular_status {
            ui.label(format!(
                "Loop: {}{}",
                if self.looping ^ status.loop_inverted {
                    "on"
                } else {
                    "off"
                },
                if status.loop_inverted {
                    " (MIDI inverted)"
                } else {
                    ""
                }
            ));
        }
        if let Some(value) = cx.input(2) {
            ui.label(format!("Dry / Wet input: {value:.3}"));
        }
        ui.weak("Hold temporarily overrides the base value.");
        ui.weak("Flag keys invert the base; Select keeps inversion until Reset.");
        ui.weak("History is fixed on Record, up to capacity.");
        ui.weak("Changing capacity clears recorded audio.");
        ui.weak("Recorded audio is not saved in the project.");
        changed
    }
}

fn action_label(action: GranularAction) -> String {
    let fraction = |v: f64| {
        for den in [1, 2, 4, 8, 16, 32, 64] {
            let num = (v * f64::from(den)).round();
            if (v * f64::from(den) - num).abs() < 1e-9 {
                return if den == 1 {
                    format!("{num:.0}")
                } else {
                    format!("{num:.0}/{den}")
                };
            }
        }
        format!("{v:.2}")
    };
    match action {
        GranularAction::Record => "Record".into(),
        GranularAction::Reset => "Reset".into(),
        GranularAction::Play => "Play".into(),
        GranularAction::Stop => "Stop".into(),
        GranularAction::Size(v) => format!("Size {}", fraction(v)),
        GranularAction::Interval(v) => format!("Interval {}", fraction(v)),
        GranularAction::Position(v) => format!("Position {v:.2}"),
        GranularAction::Wet(v) => format!("Wet {v:.2}"),
        GranularAction::InvertLoop => "Invert loop".into(),
        GranularAction::Update(v) => format!("Update {v:.2}"),
        GranularAction::History(v) => format!("History {}", fraction(v)),
        GranularAction::Block(v) => format!("Block {}", fraction(v)),
    }
}

fn number_control(ui: &mut egui::Ui, action: &mut GranularAction) -> bool {
    let (value, min, max) = match action {
        GranularAction::Size(v) | GranularAction::Interval(v) => (v, 0.05, 1.0),
        GranularAction::Position(v) | GranularAction::Wet(v) | GranularAction::Update(v) => {
            (v, 0.0, 1.0)
        }
        GranularAction::History(v) | GranularAction::Block(v) => (v, 1.0 / 96.0, 64.0),
        _ => return false,
    };
    ui.horizontal(|ui| {
        let mut changed = ui
            .add(
                egui::DragValue::new(value)
                    .speed(0.01)
                    .range(min..=max)
                    .custom_formatter(|v, _| {
                        for den in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96] {
                            let num = (v * f64::from(den)).round();
                            if (v * f64::from(den) - num).abs() < 1e-9 {
                                return if den == 1 {
                                    format!("{num:.0}")
                                } else {
                                    format!("{num:.0}/{den}")
                                };
                            }
                        }
                        format!("{v:.4}")
                    })
                    .custom_parser(|text| {
                        if let Some((num, den)) = text.split_once('/') {
                            let den = den.trim().parse::<f64>().ok()?;
                            (den > 0.0).then_some(num.trim().parse::<f64>().ok()? / den)
                        } else {
                            text.trim().parse().ok()
                        }
                    }),
            )
            .changed();
        ui.menu_button("Presets", |ui| {
            for (label, v) in [
                ("1/16", 0.0625),
                ("1/8", 0.125),
                ("1/4", 0.25),
                ("1/3", 1.0 / 3.0),
                ("1/2", 0.5),
                ("1", 1.0),
            ] {
                if ui.button(label).clicked() {
                    *value = v;
                    changed = true;
                }
            }
        });
        changed
    })
    .inner
}

fn action_menu(
    ui: &mut egui::Ui,
    action: &mut GranularAction,
    mode: &mut GranularMode,
    zone: ControlZone,
) -> bool {
    let mut changed = false;
    ui.menu_button(action_label(*action), |ui| {
        for &(label, value) in zone.actions() {
            let selected = std::mem::discriminant(action) == std::mem::discriminant(&value);
            if ui.selectable_label(selected, label).clicked() && !selected {
                *action = value;
                changed = true;
            }
        }
        if action.parameter().is_some() {
            ui.separator();
            changed |= number_control(ui, action);
        }
        if !matches!(
            action,
            GranularAction::Record | GranularAction::Reset | GranularAction::Stop
        ) {
            ui.separator();
            ui.label("Mode for this key's value / Play actions");
            changed |= ui
                .selectable_value(mode, GranularMode::Hold, "Hold")
                .changed();
            changed |= ui
                .selectable_value(mode, GranularMode::Select, "Select")
                .changed();
        }
    });
    changed
}

pub(super) fn key_table(
    ui: &mut egui::Ui,
    zone: ControlZone,
    keys: &mut Vec<GranularKey>,
    other: &[GranularKey],
) -> bool {
    ui.push_id(
        match zone {
            ControlZone::Input => "input keys",
            ControlZone::Output => "output keys",
        },
        |ui| {
            ui.spacing_mut().item_spacing.x *= 0.5;
            let mut changed = false;
            let mut remove = None;
            let bands_used: usize = keys
                .iter()
                .chain(other)
                .map(|key| if key.velocity { key.bands.len() } else { 1 })
                .sum();
            for (index, key) in keys.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    if binding_row(ui, true, |ui| {
                        changed |= key_control(ui, "", &mut key.key);
                        if ui
                            .add(egui::Button::new("vel").small().selected(key.velocity))
                            .on_hover_text(if key.velocity {
                                "velocity bands enabled"
                            } else {
                                "enable velocity bands"
                            })
                            .clicked()
                        {
                            key.velocity = !key.velocity;
                            if key.velocity
                                && key.bands.len() == 1
                                && bands_used < MAX_GRANULAR_BINDINGS
                            {
                                let action = key.bands[0].action;
                                key.bands[0].end = 63;
                                key.bands.push(GranularBand { end: 127, action });
                            }
                            changed = true;
                        }
                        if !key.velocity
                            && let Some(band) = key.bands.first_mut()
                        {
                            changed |= action_menu(ui, &mut band.action, &mut key.mode, zone);
                        }
                    }) {
                        remove = Some(index);
                    }
                    if key.velocity {
                        ui.indent("bands", |ui| {
                            let mut start = 1;
                            let mut drop_band = None;
                            let count = key.bands.len();
                            for band_index in 0..count {
                                let next = key
                                    .bands
                                    .get(band_index + 1)
                                    .map_or(127, |band| band.end.saturating_sub(1));
                                let band = &mut key.bands[band_index];
                                ui.push_id(band_index, |ui| {
                                    if binding_row(ui, count > 1, |ui| {
                                        changed |= velocity_range(
                                            ui,
                                            start,
                                            &mut band.end,
                                            next,
                                            band_index + 1 == count,
                                        );
                                        changed |=
                                            action_menu(ui, &mut band.action, &mut key.mode, zone);
                                    }) {
                                        drop_band = Some(band_index);
                                    }
                                });
                                start = band.end.saturating_add(1);
                            }
                            if let Some(index) = drop_band {
                                key.bands.remove(index);
                                if let Some(last) = key.bands.last_mut() {
                                    last.end = 127;
                                }
                                changed = true;
                            }
                            let start = key
                                .bands
                                .iter()
                                .rev()
                                .nth(1)
                                .map_or(1, |band| band.end.saturating_add(1));
                            if bands_used < MAX_GRANULAR_BINDINGS
                                && start < 127
                                && ui.small_button("+ band").clicked()
                            {
                                let action =
                                    key.bands.last_mut().map_or(zone.actions()[0].1, |last| {
                                        last.end = start + (127 - start) / 2;
                                        last.action
                                    });
                                key.bands.push(GranularBand { end: 127, action });
                                changed = true;
                            }
                        });
                    }
                });
            }
            if let Some(index) = remove {
                keys.remove(index);
                changed = true;
            }
            if bands_used < MAX_GRANULAR_BINDINGS
                && ui.small_button("+ key").clicked()
                && let Some(key) = (24..128)
                    .chain(0..24)
                    .find(|&value| !keys.iter().chain(other).any(|key| key.key == value))
            {
                keys.push(GranularKey::new(key, zone.actions()[0].1));
                changed = true;
            }
            changed
        },
    )
    .inner
}

fn binding_row(ui: &mut egui::Ui, removable: bool, contents: impl FnOnce(&mut egui::Ui)) -> bool {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let removed = ui
                .add_enabled(removable, egui::Button::new("x").small())
                .clicked();
            let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
            ui.allocate_ui_with_layout(
                size,
                egui::Layout::left_to_right(egui::Align::Center),
                contents,
            );
            removed
        })
        .inner
    })
    .inner
}

fn velocity_range(ui: &mut egui::Ui, start: u8, end: &mut u8, next: u8, last: bool) -> bool {
    let width = [egui::TextStyle::Body, egui::TextStyle::Monospace]
        .into_iter()
        .map(|style| {
            let font = style.resolve(ui.style());
            ui.fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap("127".into(), font, ui.visuals().text_color())
                    .size()
                    .x
            })
        })
        .fold(0.0f32, f32::max)
        + 2.0 * ui.spacing().button_padding.x;
    let cell = egui::vec2(width, ui.spacing().interact_size.y);
    ui.spacing_mut().interact_size.x = width;
    ui.add_sized(cell, egui::Label::new(start.to_string()));
    ui.label("–");
    if last {
        ui.add_sized(cell, egui::Label::new("127"));
        false
    } else {
        ui.add_sized(cell, egui::DragValue::new(end).range(start..=next))
            .changed()
    }
}
