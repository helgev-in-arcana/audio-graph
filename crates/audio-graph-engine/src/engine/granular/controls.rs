use plugin_host::NoteEvent;

use crate::ir::{GranularAction, GranularBinding, GranularMode, MAX_GRANULAR_BINDINGS};

/// A channel/key address has one active press, so the sixteen MIDI channels bound storage.
const MAX_PRESSES: usize = 16 * 128;

#[derive(Clone, Copy)]
struct Press {
    channel: i16,
    key: i16,
    action: GranularAction,
}

pub(super) struct Controls {
    bindings: Vec<GranularBinding>,
    presses: Vec<Press>,
    selected: [Option<f64>; 8],
    held: [Option<f64>; 8],
    revisions: [u64; 8],
    loop_selected: bool,
    loop_held: bool,
    record: bool,
    play: bool,
    play_selected: bool,
}

impl Controls {
    pub(super) fn new() -> Self {
        Self {
            bindings: Vec::with_capacity(MAX_GRANULAR_BINDINGS),
            presses: Vec::with_capacity(MAX_PRESSES),
            selected: [None; 8],
            held: [None; 8],
            revisions: [0; 8],
            loop_selected: false,
            loop_held: false,
            record: false,
            play: false,
            play_selected: false,
        }
    }

    pub(super) fn configure(&mut self, bindings: &[GranularBinding]) {
        if self.bindings != bindings {
            self.release();
            self.play_selected = false;
            self.loop_selected = false;
            self.refresh();
            self.bindings.clear();
            self.bindings.extend_from_slice(bindings);
        }
    }

    pub(super) fn configure_values(&mut self, revisions: [u64; 8]) {
        for (target, revision) in revisions.into_iter().enumerate() {
            if self.revisions[target] != revision {
                self.selected[target] = None;
                self.revisions[target] = revision;
            }
        }
    }

    pub(super) fn selection(&self) -> ([Option<f64>; 8], [u64; 8]) {
        (self.selected, self.revisions)
    }

    pub(super) fn loop_inverted(&self) -> bool {
        self.loop_selected || self.loop_held
    }

    pub(super) fn clear(&mut self) {
        self.reset_performance();
        self.selected = [None; 8];
    }

    pub(super) fn reset_performance(&mut self) {
        self.release();
        self.play_selected = false;
        self.loop_selected = false;
        self.refresh();
    }

    pub(super) fn release(&mut self) {
        self.presses.clear();
        self.refresh();
    }

    pub(super) fn value(&self, target: usize, fallback: f64) -> f64 {
        self.held[target]
            .or(self.selected[target])
            .unwrap_or(fallback)
    }

    pub(super) fn recording(&self) -> bool {
        self.record
    }
    pub(super) fn playing(&self) -> bool {
        self.play
    }

    fn refresh(&mut self) {
        self.held = [None; 8];
        self.record = false;
        self.loop_held = false;
        self.play = self.play_selected;
        for press in &self.presses {
            match press.action {
                GranularAction::Record => self.record = true,
                GranularAction::Play => self.play = true,
                GranularAction::InvertLoop => self.loop_held = true,
                action => {
                    if let Some((target, value)) = action.parameter() {
                        self.held[target] = Some(value);
                    }
                }
            }
        }
    }

    pub(super) fn event(&mut self, event: &NoteEvent) -> Option<GranularAction> {
        let (channel, key, velocity) = match *event {
            NoteEvent::NoteOn {
                channel,
                key,
                velocity,
                ..
            } => (channel, key, Some(velocity)),
            NoteEvent::NoteOff { channel, key, .. } => (channel, key, None),
            _ => return None,
        };
        if !(0..16).contains(&channel) || !(0..128).contains(&key) {
            return None;
        }
        // Re-striking an address replaces its press; note IDs cannot restore an older velocity band.
        self.presses
            .retain(|press| press.channel != channel || press.key != key);
        let binding = velocity
            .and_then(|velocity| {
                self.bindings
                    .iter()
                    .find(|binding| binding.trigger.matches(key, velocity))
            })
            .copied();
        if let Some(binding) = binding {
            match binding.action {
                GranularAction::Reset => {
                    self.release();
                    self.play_selected = false;
                    self.loop_selected = false;
                }
                GranularAction::Stop => {
                    self.play_selected = false;
                    self.presses
                        .retain(|press| press.action != GranularAction::Play);
                }
                GranularAction::Play if binding.mode == GranularMode::Select => {
                    self.play_selected = true
                }
                GranularAction::InvertLoop if binding.mode == GranularMode::Select => {
                    self.loop_selected = true;
                }
                action if action.parameter().is_some() && binding.mode == GranularMode::Select => {
                    let (target, value) = action.parameter().unwrap();
                    self.selected[target] = Some(value);
                }
                action => self.presses.push(Press {
                    channel,
                    key,
                    action,
                }),
            }
        }
        self.refresh();
        binding.map(|binding| binding.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::KeyTrigger;

    fn setup(actions: &[GranularAction]) -> Controls {
        let mut controls = Controls::new();
        controls.configure(
            &actions
                .iter()
                .enumerate()
                .map(|(i, &action)| GranularBinding {
                    trigger: KeyTrigger::Key(24 + i as u8),
                    action,
                    mode: GranularMode::Hold,
                })
                .collect::<Vec<_>>(),
        );
        controls
    }

    fn key(on: bool, key: i16, velocity: f64, id: i32) -> NoteEvent {
        if on {
            NoteEvent::NoteOn {
                port: 0,
                channel: 0,
                key,
                velocity,
                note_id: Some(id),
                sample_offset: 0,
            }
        } else {
            NoteEvent::NoteOff {
                port: 0,
                channel: 0,
                key,
                velocity,
                note_id: Some(id),
                sample_offset: 0,
            }
        }
    }

    #[test]
    fn priorities_are_per_target_and_restore_the_most_recent_remaining_press() {
        let mut c = setup(&[
            GranularAction::Interval(0.5),
            GranularAction::Interval(0.25),
            GranularAction::Size(0.75),
            GranularAction::Interval(0.125),
        ]);
        c.event(&key(true, 24, 1.0, 1));
        c.event(&key(true, 25, 1.0, 2));
        c.event(&key(true, 26, 1.0, 3));
        assert_eq!(c.value(1, 1.0), 0.25);
        assert_eq!(c.value(0, 1.0), 0.75);
        c.event(&key(true, 27, 1.0, 4));
        assert_eq!(c.value(1, 1.0), 0.125);
        assert_eq!(c.event(&key(false, 27, 1.0, 4)), None);
        assert_eq!(c.value(1, 1.0), 0.25);
        c.event(&key(false, 25, 1.0, 2));
        assert_eq!(c.value(1, 1.0), 0.5);
        c.event(&key(false, 24, 1.0, 1));
        assert_eq!(c.value(1, 1.0), 1.0);
        assert_eq!(c.value(0, 1.0), 0.75);
    }

    #[test]
    fn record_and_play_are_independent_or_flags_and_restoration_is_not_a_trigger() {
        let mut c = setup(&[
            GranularAction::Record,
            GranularAction::Record,
            GranularAction::Play,
            GranularAction::Play,
            GranularAction::Stop,
        ]);
        assert_eq!(
            c.event(&key(true, 24, 1.0, 1)),
            Some(GranularAction::Record)
        );
        c.event(&key(true, 25, 1.0, 2));
        c.event(&key(true, 26, 1.0, 3));
        c.event(&key(true, 27, 1.0, 4));
        assert!(c.recording() && c.playing());
        assert_eq!(c.event(&key(false, 25, 1.0, 2)), None);
        c.event(&key(false, 27, 1.0, 4));
        assert!(c.recording() && c.playing());
        c.event(&key(true, 28, 1.0, 5));
        assert!(c.recording() && !c.playing());
        c.event(&key(false, 28, 1.0, 5));
        assert!(!c.playing());
        c.event(&key(false, 24, 1.0, 1));
        assert!(!c.recording());
    }

    #[test]
    fn a_restrike_discards_the_previous_band_even_when_note_ids_are_available() {
        let mut c = Controls::new();
        c.configure(&[
            GranularBinding {
                trigger: KeyTrigger::Velocity {
                    key: 24,
                    min: 1,
                    max: 63,
                },
                action: GranularAction::Interval(0.5),
                mode: GranularMode::Hold,
            },
            GranularBinding {
                trigger: KeyTrigger::Velocity {
                    key: 24,
                    min: 64,
                    max: 127,
                },
                action: GranularAction::Size(0.25),
                mode: GranularMode::Hold,
            },
        ]);
        c.event(&key(true, 24, 0.2, 10));
        assert_eq!(c.value(1, 1.0), 0.5);
        c.event(&key(true, 24, 0.9, 11));
        assert_eq!(c.value(1, 1.0), 1.0);
        assert_eq!(c.value(0, 1.0), 0.25);
        c.event(&key(false, 24, 0.1, 10));
        assert_eq!(c.value(0, 1.0), 1.0);
        c.event(&key(false, 24, 0.9, 11));
        assert_eq!(c.value(1, 1.0), 1.0);
    }

    #[test]
    fn channels_have_independent_presses_and_selection_survives_release() {
        let mut c = setup(&[GranularAction::Play]);
        c.event(&key(true, 24, 1.0, 1));
        let mut second = key(true, 24, 1.0, 2);
        if let NoteEvent::NoteOn { channel, .. } = &mut second {
            *channel = 1;
        }
        c.event(&second);
        c.event(&key(false, 24, 0.0, 1));
        assert!(c.playing());
        c.release();
        assert!(!c.playing());
        c.configure(&[GranularBinding {
            trigger: 24.into(),
            action: GranularAction::Interval(0.25),
            mode: GranularMode::Select,
        }]);
        c.event(&key(true, 24, 1.0, 3));
        c.event(&key(false, 24, 0.0, 3));
        assert_eq!(c.value(1, 1.0), 0.25);
        c.configure(&c.bindings.clone());
        assert_eq!(c.value(1, 1.0), 0.25);
    }
    #[test]
    fn selection_edits_the_base_and_hold_restores_the_latest_ui_or_midi_value() {
        let mut c = setup(&[GranularAction::Wet(0.25), GranularAction::Wet(0.75)]);
        c.bindings[0].mode = GranularMode::Select;
        c.configure_values([1; 8]);
        c.event(&key(true, 25, 1.0, 1));
        c.event(&key(true, 24, 1.0, 2));
        assert_eq!(c.value(3, 0.5), 0.75);
        assert_eq!(c.selection().0[3], Some(0.25));
        c.event(&key(false, 25, 0.0, 1));
        assert_eq!(c.value(3, 0.5), 0.25);
        c.configure_values([1; 8]);
        assert_eq!(c.value(3, 0.5), 0.25);
        c.event(&key(true, 25, 1.0, 3));
        let mut revisions = [1; 8];
        revisions[3] = 2;
        c.configure_values(revisions);
        assert_eq!(c.value(3, 0.5), 0.75);
        c.event(&key(false, 25, 0.0, 3));
        assert_eq!(c.value(3, 0.5), 0.5);
        c.event(&key(true, 24, 1.0, 4));
        assert_eq!(c.value(3, 0.5), 0.25);
        c.configure(&[]);
        assert_eq!(c.value(3, 0.5), 0.25);
    }

    #[test]
    fn inversion_holds_use_or_and_select_is_idempotent_until_reset() {
        let mut c = setup(&[
            GranularAction::InvertLoop,
            GranularAction::InvertLoop,
            GranularAction::InvertLoop,
            GranularAction::Reset,
        ]);
        c.bindings[2].mode = GranularMode::Select;
        c.event(&key(true, 24, 1.0, 1));
        c.event(&key(true, 25, 1.0, 2));
        assert!(c.loop_inverted());
        c.event(&key(false, 25, 0.0, 2));
        assert!(c.loop_inverted());
        c.event(&key(false, 24, 0.0, 1));
        assert!(!c.loop_inverted());
        for id in 3..5 {
            c.event(&key(true, 26, 1.0, id));
            c.event(&key(false, 26, 0.0, id));
            assert!(c.loop_inverted());
        }
        c.event(&key(true, 24, 1.0, 5));
        c.release();
        assert!(c.loop_inverted());
        c.event(&key(true, 27, 1.0, 6));
        assert!(!c.loop_inverted());
    }
}
