// ============================================================================
//
// HUMAN REVIEW REQUIRED: THIS FILE HAS NOT BEEN REVIEWED BY A HUMAN.
//
// ============================================================================

use plugin_host::{Event, EventSink};

use crate::InstanceId;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstanceEvent {
    pub source: InstanceId,
    pub event: Event,
}

/// Bounded output from every child and chunk in a parent block.
#[derive(Default)]
pub struct InstanceEventSink {
    events: Vec<InstanceEvent>,
    overflowed: bool,
    pub(crate) native: EventSink,
}

impl InstanceEventSink {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            overflowed: false,
            native: EventSink::with_capacity(capacity),
        }
    }
    pub fn events(&self) -> &[InstanceEvent] {
        &self.events
    }
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }
    pub fn clear(&mut self) {
        self.events.clear();
        self.overflowed = false;
        self.native.clear();
    }
    pub(crate) fn collect(&mut self, source: InstanceId, offset: u32) {
        self.overflowed |= self.native.overflowed();
        for event in self.native.events() {
            let Some(time) = event.sample_offset().checked_add(offset) else {
                self.overflowed = true;
                continue;
            };
            if self.events.len() == self.events.capacity() {
                self.overflowed = true;
                break;
            }
            self.events.push(InstanceEvent {
                source,
                event: event.at_offset(time),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host::{ParamEvent, ParamId, Target};

    /// Both native loss and merged-capacity loss survive subsequent successful child calls.
    #[test]
    fn collection_keeps_sources_offsets_and_overflow() {
        let first = InstanceId::new(0);
        let second = InstanceId::new(0);
        let event = Event::Param(ParamEvent::SetValue {
            id: ParamId(0),
            value: 0.5,
            target: Target::Global,
            sample_offset: 1,
        });
        let mut sink = InstanceEventSink::with_capacity(2);
        sink.native.push(event);
        sink.native.mark_overflow();
        sink.collect(first, 0);
        sink.native.clear();
        sink.native.push(event);
        sink.collect(second, 32);
        assert_eq!(sink.events()[0].source, first);
        assert_eq!(sink.events()[1].source, second);
        assert_eq!(sink.events()[1].event.sample_offset(), 33);
        assert!(sink.overflowed());
        sink.clear();
        assert!(!sink.overflowed());
        for _ in 0..3 {
            sink.native.clear();
            sink.native.push(event);
            sink.collect(first, 0);
        }
        assert_eq!(sink.events().len(), 2);
        assert!(sink.overflowed());
    }
}
