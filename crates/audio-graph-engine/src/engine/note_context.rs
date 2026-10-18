use plugin_host::Event;

use crate::ir::{NoteOwnership, NoteParamBinding};

pub(super) struct NoteRow<'a> {
    lanes: &'a [f64],
    start: u32,
    frames: u32,
    sample_rate: f64,
    tempo: f64,
}

impl<'a> NoteRow<'a> {
    pub(super) fn new(
        lanes: &'a [f64],
        start: u32,
        frames: u32,
        sample_rate: f64,
        tempo: f64,
    ) -> Self {
        Self {
            lanes,
            start,
            frames,
            sample_rate,
            tempo,
        }
    }
}

pub(crate) struct NoteWriter<'a> {
    events: &'a mut Vec<Event>,
    dropped: &'a mut u64,
}

impl<'a> NoteWriter<'a> {
    pub(super) fn new(events: &'a mut Vec<Event>, dropped: &'a mut u64) -> Self {
        Self { events, dropped }
    }

    pub(crate) fn push(&mut self, event: Event) {
        super::notes::NoteState::push(self.events, self.dropped, event);
    }

    pub(crate) fn drop_event(&mut self) {
        *self.dropped += 1;
    }
}

pub(crate) struct NoteEvalContext<'a> {
    pub input: &'a [Event],
    pub output: NoteWriter<'a>,
    pub notes: NoteOwnership<'a>,
    bindings: &'a [NoteParamBinding],
    row: &'a NoteRow<'a>,
}

impl<'a> NoteEvalContext<'a> {
    pub(super) fn new(
        input: &'a [Event],
        output: NoteWriter<'a>,
        notes: NoteOwnership<'a>,
        bindings: &'a [NoteParamBinding],
        row: &'a NoteRow<'a>,
    ) -> Self {
        Self {
            input,
            output,
            notes,
            bindings,
            row,
        }
    }

    pub(crate) fn parameter(&self, index: usize) -> f64 {
        let binding = &self.bindings[index];
        binding
            .lane
            .and_then(|lane| self.row.lanes.get(usize::from(lane)).copied())
            .unwrap_or(binding.fallback)
    }

    pub(crate) fn start(&self) -> u32 {
        self.row.start
    }

    pub(crate) fn frames(&self) -> u32 {
        self.row.frames
    }

    pub(crate) fn sample_rate(&self) -> f64 {
        self.row.sample_rate
    }

    pub(crate) fn tempo(&self) -> f64 {
        self.row.tempo
    }
}
