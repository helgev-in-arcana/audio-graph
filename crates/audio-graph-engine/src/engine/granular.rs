use std::collections::VecDeque;

use plugin_host::NoteEvent;

use super::lines::Slot;
use crate::ir::{
    GranularSpec, KeyTrigger, MAX_GRAINS, MAX_GRANULAR_SLICES, MIN_GRANULAR_BLOCK_SECONDS,
};

/// Five milliseconds softens state changes without retaining a recording being replaced.
const FADE_SECONDS: f64 = 0.005;
/// One keyboard's worth of overlapping control presses bounds Hold bookkeeping.
const MAX_HELD: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Owner {
    id: Option<i32>,
    channel: i16,
    key: i16,
}

impl Owner {
    fn released_by(self, id: Option<i32>, channel: i16, key: i16) -> bool {
        self.channel == channel && self.key == key && (id.is_none() || self.id == id)
    }
}

#[derive(Debug, Clone, Copy)]
struct Slice {
    start: u64,
    len: usize,
    planned: usize,
}

#[derive(Debug, Clone, Copy)]
struct Grain {
    start: u64,
    age: usize,
    len: usize,
}

pub(super) struct GranularState {
    node: u32,
    pub(super) ring: Vec<f32>,
    pub(super) capacity: usize,
    history: usize,
    written: u64,
    valid: usize,
    slices: VecDeque<Slice>,
    record_left: usize,
    recording: Option<Owner>,
    looping: bool,
    playing: bool,
    started: bool,
    play_owners: Vec<Owner>,
    block_left: usize,
    block_len: usize,
    until_grain: f64,
    grains: [Option<Grain>; MAX_GRAINS],
    mix: f64,
    tail: [f32; 2],
    tail_left: usize,
    last_wet: [f32; 2],
    sample_rate: f64,
    bindings: Option<([KeyTrigger; 4], bool)>,
    pub(super) note_source: Option<u16>,
    pub(super) dropped: u64,
}

impl GranularState {
    pub(super) fn new() -> Self {
        Self {
            node: u32::MAX,
            ring: Vec::new(),
            capacity: 0,
            history: 0,
            written: 0,
            valid: 0,
            slices: VecDeque::with_capacity(MAX_GRANULAR_SLICES),
            record_left: 0,
            recording: None,
            looping: true,
            playing: false,
            started: false,
            play_owners: Vec::with_capacity(MAX_HELD),
            block_left: 0,
            block_len: 0,
            until_grain: 0.0,
            grains: [None; MAX_GRAINS],
            mix: 0.0,
            tail: [0.0; 2],
            tail_left: 0,
            last_wet: [0.0; 2],
            sample_rate: 0.0,
            bindings: None,
            note_source: None,
            dropped: 0,
        }
    }

    pub(super) fn configure(&mut self, spec: &GranularSpec, source: Option<u16>, sample_rate: f64) {
        if self.sample_rate != sample_rate {
            self.clear();
            self.sample_rate = sample_rate;
        }
        if self.bindings != Some((spec.keys, spec.latch)) || self.note_source != source {
            self.release_keys();
            self.bindings = Some((spec.keys, spec.latch));
            self.note_source = source;
        }
    }

    pub(super) fn release_keys(&mut self) {
        self.recording = None;
        if !self.play_owners.is_empty() {
            self.playing = false;
        }
        self.play_owners.clear();
    }

    pub(super) fn transport_reset(&mut self) {
        self.release_keys();
        self.grains.fill(None);
        self.block_left = 0;
        self.until_grain = 0.0;
        self.mix = 0.0;
        self.tail_left = 0;
        self.last_wet = [0.0; 2];
        self.started = false;
    }

    fn finish_grains(&mut self) {
        self.started = false;
        self.tail = self.last_wet;
        self.tail_left = self.fade_samples();
        self.grains.fill(None);
        self.block_left = 0;
        self.until_grain = 0.0;
    }

    fn fade_samples(&self) -> usize {
        (self.sample_rate * FADE_SECONDS).ceil().max(1.0) as usize
    }

    fn empty_recording(&mut self) {
        self.written = 0;
        self.valid = 0;
        self.slices.clear();
        self.record_left = 0;
    }

    pub(super) fn event(&mut self, event: &NoteEvent, spec: &GranularSpec) {
        match *event {
            NoteEvent::NoteOn {
                note_id,
                channel,
                key,
                velocity,
                ..
            } => {
                let owner = Owner {
                    id: note_id,
                    channel,
                    key,
                };
                let Some(action) = spec
                    .keys
                    .iter()
                    .position(|trigger| trigger.matches(key, velocity))
                else {
                    return;
                };
                match action {
                    0 => {
                        self.finish_grains();
                        self.empty_recording();
                        self.history = ((spec.history * self.sample_rate).round() as usize)
                            .clamp(1, self.capacity.max(1));
                        self.looping = spec.looping;
                        self.recording = Some(owner);
                    }
                    1 => {
                        self.finish_grains();
                        self.empty_recording();
                        self.recording = None;
                        self.playing = false;
                        self.play_owners.clear();
                    }
                    2 => {
                        if !spec.latch {
                            if self.play_owners.len() == MAX_HELD {
                                self.dropped = self.dropped.saturating_add(1);
                                return;
                            }
                            self.play_owners.push(owner);
                        }
                        self.playing = true;
                        self.block_left = 0;
                        self.until_grain = 0.0;
                    }
                    _ => {
                        self.playing = false;
                        self.play_owners.clear();
                    }
                }
            }
            NoteEvent::NoteOff {
                note_id,
                channel,
                key,
                ..
            } => {
                if self
                    .recording
                    .is_some_and(|owner| owner.released_by(note_id, channel, key))
                {
                    self.recording = None;
                }
                if let Some(index) = self
                    .play_owners
                    .iter()
                    .position(|owner| owner.released_by(note_id, channel, key))
                {
                    self.play_owners.remove(index);
                    if self.play_owners.is_empty() {
                        self.playing = false;
                    }
                }
            }
            _ => {}
        }
    }

    fn block_samples(&self, spec: &GranularSpec, tempo: f64) -> usize {
        let tempo = if tempo.is_finite() && tempo > 0.0 {
            tempo
        } else {
            120.0
        };
        let seconds = (spec.block_beats * 60.0 / tempo).max(MIN_GRANULAR_BLOCK_SECONDS);
        (seconds * self.sample_rate)
            .round()
            .max((MIN_GRANULAR_BLOCK_SECONDS * self.sample_rate).ceil())
            .clamp(4.0, u32::MAX as f64) as usize
    }

    fn record(&mut self, input: [f32; 2], spec: &GranularSpec, tempo: f64) {
        if self.recording.is_none()
            || self.capacity == 0
            || (!self.looping && self.valid == self.history)
        {
            return;
        }
        if self.record_left == 0 {
            self.record_left = self.block_samples(spec, tempo);
            if self.slices.len() == MAX_GRANULAR_SLICES {
                self.slices.pop_front();
            }
            self.slices.push_back(Slice {
                start: self.written,
                len: 0,
                planned: self.record_left,
            });
        }
        let slot = self.written as usize % self.history;
        for (ch, value) in input.into_iter().enumerate() {
            let at = ch * self.capacity + slot;
            self.ring[at] = if self.valid < self.history {
                value
            } else {
                (f64::from(self.ring[at]) * (1.0 - spec.update) + f64::from(value) * spec.update)
                    as f32
            };
        }
        self.written += 1;
        self.valid = (self.valid + 1).min(self.history);
        self.record_left -= 1;
        if let Some(slice) = self.slices.back_mut() {
            slice.len += 1;
        }
        let oldest = self.written - self.valid as u64;
        while self
            .slices
            .front()
            .is_some_and(|slice| slice.start + slice.len as u64 <= oldest)
        {
            self.slices.pop_front();
        }
    }

    fn spawn(&mut self, size: f64, position: f64) {
        if self.slices.is_empty() {
            return;
        }
        let index = (position * (self.slices.len() - 1) as f64).round() as usize;
        let slice = self.slices[index];
        let start = slice.start.max(self.written - self.valid as u64);
        let available = (slice.start + slice.len as u64).saturating_sub(start) as usize;
        let wanted = ((self.block_len as f64 * size).round() as usize).max(4);
        let still_writing = self.recording.is_some()
            && (self.looping || self.valid < self.history)
            && index + 1 == self.slices.len()
            && slice.len < slice.planned;
        if available < 4 || (still_writing && available < wanted.min(self.history)) {
            return;
        }
        let Some(slot) = self.grains.iter_mut().find(|grain| grain.is_none()) else {
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        *slot = Some(Grain {
            start,
            age: 0,
            len: wanted.min(available),
        });
        self.started = true;
    }

    pub(super) fn tick(
        &mut self,
        input: [f32; 2],
        spec: &GranularSpec,
        params: [f64; 4],
        tempo: f64,
    ) -> [f32; 2] {
        let [size, interval, position, wet] = params;
        if self.playing {
            if self.block_left == 0 {
                self.block_len = self.block_samples(spec, tempo);
                self.block_left = self.block_len;
                self.until_grain = 0.0;
            }
            if self.until_grain <= 0.0 {
                self.spawn(size, position);
                self.until_grain += (self.block_len as f64 * interval).max(1.0);
            }
            self.until_grain -= 1.0;
            self.block_left -= 1;
        }
        let mut sum = [0.0f64; 2];
        let mut weight = 0.0;
        let mut sounding = false;
        for slot in &mut self.grains {
            let Some(grain) = slot else { continue };
            sounding = true;
            let w = 0.5
                - 0.5 * (std::f64::consts::TAU * grain.age as f64 / (grain.len - 1) as f64).cos();
            let at = (grain.start + grain.age as u64) as usize % self.history.max(1);
            for (ch, sum) in sum.iter_mut().enumerate() {
                *sum += f64::from(self.ring[ch * self.capacity + at]) * w;
            }
            weight += w;
            grain.age += 1;
            if grain.age == grain.len {
                *slot = None;
            }
        }
        let tail_gain = self.tail_left as f64 / self.fade_samples() as f64;
        let active = (self.playing && self.started) || sounding || self.tail_left > 0;
        for (ch, sum) in sum.iter_mut().enumerate() {
            *sum = *sum / weight.max(1.0) + f64::from(self.tail[ch]) * tail_gain;
            self.last_wet[ch] = *sum as f32;
        }
        self.tail_left = self.tail_left.saturating_sub(1);
        let target = if active { wet } else { 0.0 };
        let step = 1.0 / self.fade_samples() as f64;
        self.mix += (target - self.mix).clamp(-step, step);
        self.record(input, spec, tempo);
        std::array::from_fn(|ch| {
            ((1.0 - self.mix) * f64::from(input[ch]) + self.mix * sum[ch]) as f32
        })
    }
}

impl Slot for GranularState {
    fn node(&self) -> u32 {
        self.node
    }
    fn set_node(&mut self, node: u32) {
        self.node = node;
    }
    fn clear(&mut self) {
        self.empty_recording();
        self.recording = None;
        self.playing = false;
        self.play_owners.clear();
        self.transport_reset();
        self.bindings = None;
        self.note_source = None;
        self.dropped = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(looping: bool) -> (GranularState, GranularSpec) {
        let spec = GranularSpec {
            block_beats: 0.016,
            history: 0.016,
            looping,
            update: 1.0,
            latch: true,
            keys: [24.into(), 25.into(), 26.into(), 27.into()],
        };
        let mut state = GranularState::new();
        state.capacity = 32;
        state.ring = vec![0.0; 64];
        state.configure(&spec, Some(0), 1000.0);
        (state, spec)
    }

    fn key(on: bool, key: i16, id: i32) -> NoteEvent {
        if on {
            NoteEvent::NoteOn {
                note_id: Some(id),
                port: 0,
                channel: 0,
                key,
                velocity: 1.0,
                sample_offset: 0,
            }
        } else {
            NoteEvent::NoteOff {
                note_id: Some(id),
                port: 0,
                channel: 0,
                key,
                velocity: 0.0,
                sample_offset: 0,
            }
        }
    }

    fn samples(
        state: &mut GranularState,
        spec: &GranularSpec,
        input: &[f32],
        tempo: f64,
    ) -> Vec<[f32; 2]> {
        input
            .iter()
            .map(|&x| state.tick([x, -x], spec, [1.0, 1.0, 0.0, 1.0], tempo))
            .collect()
    }

    fn history(state: &GranularState) -> Vec<f32> {
        (state.written - state.valid as u64..state.written)
            .map(|i| state.ring[i as usize % state.history])
            .collect()
    }

    #[test]
    fn loop_keeps_the_latest_audio_and_full_stop_keeps_the_first_audio() {
        for looping in [false, true] {
            let (mut state, spec) = setup(looping);
            state.event(&key(true, 24, 1), &spec);
            let input: Vec<_> = (0..40).map(|i| i as f32).collect();
            samples(&mut state, &spec, &input, 120.0);
            assert_eq!(
                history(&state),
                if looping {
                    input[24..].to_vec()
                } else {
                    input[..16].to_vec()
                }
            );
            state.event(&key(false, 24, 1), &spec);
            let kept = history(&state);
            samples(&mut state, &spec, &[100.0; 32], 120.0);
            assert_eq!(history(&state), kept);
        }
    }

    #[test]
    fn updates_mix_only_written_samples_and_a_new_recording_discards_the_old_one() {
        let (mut state, mut spec) = setup(true);
        spec.update = 0.25;
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[2.0; 16], 120.0);
        assert_eq!(history(&state), vec![2.0; 16]);
        samples(&mut state, &spec, &[6.0; 16], 120.0);
        assert_eq!(history(&state), vec![3.0; 16]);
        state.event(&key(true, 24, 2), &spec);
        state.event(&key(false, 24, 1), &spec);
        samples(&mut state, &spec, &[9.0; 8], 120.0);
        assert_eq!(history(&state), vec![9.0; 8]);
        state.event(&key(false, 24, 2), &spec);
        samples(&mut state, &spec, &[0.0; 8], 120.0);
        assert_eq!(history(&state), vec![9.0; 8]);
    }

    #[test]
    fn tempo_changes_apply_at_the_next_recorded_block_boundary() {
        let (mut state, mut spec) = setup(true);
        spec.history = 0.032;
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[1.0; 3], 120.0);
        samples(&mut state, &spec, &[1.0; 6], 60.0);
        assert_eq!(state.slices[0].len, 8);
        assert_eq!(state.slices[0].planned, 8);
        assert_eq!(state.slices[1].start, 8);
        assert_eq!(state.slices[1].planned, 16);
        assert_eq!(state.slices[1].len, 1);
        state.event(&key(false, 24, 1), &spec);
        samples(&mut state, &spec, &[0.0; 10], 240.0);
        assert_eq!(state.slices[0].planned, 8);
    }

    #[test]
    fn grain_windows_read_the_recorded_slice_and_preserve_stereo() {
        let (mut state, spec) = setup(true);
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[1.0; 8], 120.0);
        state.event(&key(false, 24, 1), &spec);
        state.event(&key(true, 26, 2), &spec);
        state.mix = 1.0;
        let output = samples(&mut state, &spec, &[0.0; 8], 120.0);
        for (i, frame) in output.iter().enumerate() {
            let expected = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / 7.0).cos();
            assert!((f64::from(frame[0]) - expected).abs() < 1e-6);
            assert_eq!(frame[1], -frame[0]);
        }
    }

    #[test]
    fn recording_reads_the_oldest_sample_before_overwriting_it() {
        let (mut state, mut spec) = setup(true);
        spec.history = 0.008;
        state.event(&key(true, 24, 1), &spec);
        samples(
            &mut state,
            &spec,
            &(1..=8).map(|i| i as f32).collect::<Vec<_>>(),
            120.0,
        );
        state.event(&key(true, 26, 2), &spec);
        state.mix = 1.0;
        let output = samples(&mut state, &spec, &[99.0; 8], 120.0);
        for (i, frame) in output.iter().enumerate() {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / 7.0).cos();
            assert!((f64::from(frame[0]) - (i + 1) as f64 * w).abs() < 1e-5);
        }
        assert_eq!(history(&state), vec![99.0; 8]);
    }

    #[test]
    fn slice_selection_uses_recorded_boundaries_after_a_tempo_change() {
        let (mut state, spec) = setup(true);
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[1.0; 8], 120.0);
        samples(&mut state, &spec, &[-1.0; 8], 120.0);
        state.event(&key(false, 24, 1), &spec);
        for (position, sign) in [(0.0, 1.0), (1.0, -1.0)] {
            state.transport_reset();
            state.event(&key(true, 26, 2), &spec);
            let output: Vec<_> = (0..16)
                .map(|_| state.tick([0.0; 2], &spec, [1.0, 1.0, position, 1.0], 60.0))
                .collect();
            assert!(output.iter().all(|frame| frame[0] * sign >= 0.0));
            assert!(output.iter().any(|frame| frame[0] * sign > 0.1));
        }
    }

    #[test]
    fn stop_leaves_grain_tails_and_reset_clears_audio_without_restarting_a_held_key() {
        let (mut state, mut spec) = setup(true);
        spec.latch = false;
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[1.0; 16], 120.0);
        state.event(&key(false, 24, 1), &spec);
        state.event(&key(true, 26, 2), &spec);
        samples(&mut state, &spec, &[0.0; 4], 120.0);
        state.event(&key(true, 27, 3), &spec);
        assert!(!state.playing);
        assert!(
            samples(&mut state, &spec, &[0.0; 4], 120.0)
                .iter()
                .any(|frame| frame[0] > 0.0)
        );
        assert!(
            samples(&mut state, &spec, &[0.0; 16], 120.0)
                .iter()
                .all(|frame| frame[0] == 0.0)
        );
        state.event(&key(true, 25, 4), &spec);
        samples(&mut state, &spec, &[2.0; 16], 120.0);
        assert_eq!(state.valid, 0);
        assert!(!state.playing);
        assert!(state.recording.is_none());
    }

    #[test]
    fn dense_grains_and_tempo_jumps_remain_bounded_and_finite() {
        let (mut state, spec) = setup(true);
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[1.0; 16], 120.0);
        state.event(&key(false, 24, 1), &spec);
        state.event(&key(true, 26, 2), &spec);
        for i in 0..4000 {
            let tempo = if i % 100 < 50 { 1.0 } else { 100_000.0 };
            let output = state.tick([0.0; 2], &spec, [1.0, 0.05, 0.0, 1.0], tempo);
            assert!(output.iter().all(|v| v.is_finite() && v.abs() <= 1.00001));
        }
    }

    #[test]
    fn gaps_between_grains_do_not_reintroduce_dry_audio() {
        let (mut state, spec) = setup(true);
        state.event(&key(true, 24, 1), &spec);
        samples(&mut state, &spec, &[1.0; 16], 120.0);
        state.event(&key(false, 24, 1), &spec);
        state.event(&key(true, 26, 2), &spec);
        state.mix = 1.0;
        let output: Vec<_> = (0..8)
            .map(|_| state.tick([99.0; 2], &spec, [0.5, 1.0, 0.0, 1.0], 120.0))
            .collect();
        assert!(output[4..].iter().all(|frame| *frame == [0.0; 2]));
    }
}
