//! Host-side containers passed into `IAudioProcessor::process`.
//!
//! Buffers are pre-sized during activation and reused during processing to ensure
//! the audio thread never allocates. Capacity limits are strict; points exceeding
//! capacity are dropped rather than triggering reallocations on the real-time thread.

use std::cell::{Cell, RefCell};

use vst3::Steinberg::Vst::{
    Event, IEventList, IEventListTrait, IParamValueQueue, IParamValueQueueTrait, IParameterChanges,
    IParameterChangesTrait, ParamID, ParamValue,
};
use vst3::Steinberg::{int32, kInvalidArgument, kResultFalse, kResultOk, tresult};
use vst3::{Class, ComWrapper};

/// Points for one parameter within one block.
pub struct ValueQueue {
    id: Cell<ParamID>,
    /// `(sample_offset, normalized_value)`, kept sorted by offset.
    points: RefCell<Vec<(int32, ParamValue)>>,
}

impl ValueQueue {
    fn new(capacity: usize) -> ComWrapper<ValueQueue> {
        ComWrapper::new(ValueQueue {
            id: Cell::new(0),
            points: RefCell::new(Vec::with_capacity(capacity)),
        })
    }

    fn reset(&self, id: ParamID) {
        self.id.set(id);
        self.points.borrow_mut().clear();
    }

    /// Append a point. Returns false when the pre-allocated capacity is full.
    fn push(&self, sample_offset: int32, value: ParamValue) -> bool {
        let mut points = self.points.borrow_mut();
        if points.len() == points.capacity() {
            return false;
        }
        points.push((sample_offset, value));
        true
    }
}

impl Class for ValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.borrow().len() as int32
    }

    unsafe fn getPoint(
        &self,
        index: int32,
        sample_offset: *mut int32,
        value: *mut ParamValue,
    ) -> tresult {
        if sample_offset.is_null() || value.is_null() || index < 0 {
            return kInvalidArgument;
        }
        let points = self.points.borrow();
        let Some(&(offset, v)) = points.get(index as usize) else {
            return kResultFalse;
        };
        unsafe {
            *sample_offset = offset;
            *value = v;
        }
        kResultOk
    }

    unsafe fn addPoint(
        &self,
        sample_offset: int32,
        value: ParamValue,
        index: *mut int32,
    ) -> tresult {
        // Plugins call this on *output* queues. We accept it so a plugin that
        // reports its own automation does not see a failure, but the values are
        // only read back if the caller asks.
        if !self.push(sample_offset, value) {
            return kResultFalse;
        }
        if !index.is_null() {
            unsafe { *index = (self.points.borrow().len() - 1) as int32 };
        }
        kResultOk
    }
}

/// The `IParameterChanges` handed to `process`, backed by a fixed queue pool.
pub struct ParameterChanges {
    /// Pre-built queues, reused every block. `used` is how many are live now.
    pool: Vec<ComWrapper<ValueQueue>>,
    used: Cell<usize>,
}

impl ParameterChanges {
    /// `max_params` distinct parameters, each with up to `max_points` changes
    /// per block. Both are hard limits from here on.
    pub fn new(max_params: usize, max_points: usize) -> ComWrapper<ParameterChanges> {
        ComWrapper::new(ParameterChanges {
            pool: (0..max_params)
                .map(|_| ValueQueue::new(max_points))
                .collect(),
            used: Cell::new(0),
        })
    }

    pub fn clear(&self) {
        self.used.set(0);
    }

    /// Record `value` (normalized) for `id` at `sample_offset`.
    ///
    /// Consecutive calls for the same parameter reuse its existing queue.
    /// Returns false if a limit was hit and the point was dropped.
    pub fn add_point(&self, id: ParamID, sample_offset: int32, value: ParamValue) -> bool {
        let used = self.used.get();
        for queue in &self.pool[..used] {
            if queue.id.get() == id {
                return queue.push(sample_offset, value);
            }
        }
        let Some(queue) = self.pool.get(used) else {
            return false;
        };
        queue.reset(id);
        self.used.set(used + 1);
        queue.push(sample_offset, value)
    }

    /// Read back what the plugin wrote into an output change list.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn points(&self) -> Vec<(ParamID, int32, ParamValue)> {
        let mut out = Vec::new();
        for queue in &self.pool[..self.used.get()] {
            for &(offset, value) in queue.points.borrow().iter() {
                out.push((queue.id.get(), offset, value));
            }
        }
        out
    }
}

impl Class for ParameterChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParameterChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.used.get() {
            return std::ptr::null_mut();
        }
        // Borrowed for the duration of the call, as VST3 expects — the pool
        // owns these and outlives the block.
        self.pool[index as usize]
            .as_com_ref::<IParamValueQueue>()
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let id = unsafe { *id };
        let used = self.used.get();

        for (i, queue) in self.pool[..used].iter().enumerate() {
            if queue.id.get() == id {
                if !index.is_null() {
                    unsafe { *index = i as int32 };
                }
                return queue
                    .as_com_ref::<IParamValueQueue>()
                    .map_or(std::ptr::null_mut(), |r| r.as_ptr());
            }
        }

        let Some(queue) = self.pool.get(used) else {
            return std::ptr::null_mut();
        };
        queue.reset(id);
        self.used.set(used + 1);
        if !index.is_null() {
            unsafe { *index = used as int32 };
        }
        queue
            .as_com_ref::<IParamValueQueue>()
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

/// The `IEventList` handed to `process`, in both directions.
pub struct EventList {
    events: RefCell<Vec<Event>>,
}

impl EventList {
    pub fn new(capacity: usize) -> ComWrapper<EventList> {
        ComWrapper::new(EventList {
            events: RefCell::new(Vec::with_capacity(capacity)),
        })
    }

    pub fn clear(&self) {
        self.events.borrow_mut().clear();
    }

    /// Returns false if the pre-allocated capacity is full.
    pub fn push(&self, event: Event) -> bool {
        let mut events = self.events.borrow_mut();
        if events.len() == events.capacity() {
            return false;
        }
        events.push(event);
        true
    }

    pub fn len(&self) -> usize {
        self.events.borrow().len()
    }

    pub fn get(&self, index: usize) -> Option<Event> {
        self.events.borrow().get(index).copied()
    }
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.events.borrow().len() as int32
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        if e.is_null() || index < 0 {
            return kInvalidArgument;
        }
        let events = self.events.borrow();
        let Some(&event) = events.get(index as usize) else {
            return kResultFalse;
        };
        unsafe { *e = event };
        kResultOk
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        if self.push(unsafe { *e }) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_group_points_by_parameter() {
        let changes = ParameterChanges::new(4, 8);
        assert!(changes.add_point(7, 0, 0.25));
        assert!(changes.add_point(9, 0, 0.5));
        assert!(changes.add_point(7, 32, 0.75));

        unsafe {
            assert_eq!(changes.getParameterCount(), 2);
        }
        let points = changes.points();
        assert_eq!(points.len(), 3);
        assert_eq!(points.iter().filter(|(id, _, _)| *id == 7).count(), 2);
    }

    #[test]
    fn clear_makes_queues_reusable_without_allocating() {
        let changes = ParameterChanges::new(2, 2);
        changes.add_point(1, 0, 0.0);
        changes.add_point(2, 0, 0.0);
        changes.clear();
        assert!(changes.add_point(3, 0, 1.0));
        unsafe { assert_eq!(changes.getParameterCount(), 1) };
    }

    #[test]
    fn exceeding_the_pool_drops_rather_than_allocating() {
        let changes = ParameterChanges::new(1, 1);
        assert!(changes.add_point(1, 0, 0.0));
        // Second point for the same parameter: point capacity exhausted.
        assert!(!changes.add_point(1, 1, 0.0));
        // Second parameter: queue pool exhausted.
        assert!(!changes.add_point(2, 0, 0.0));
    }

    #[test]
    fn event_list_respects_its_capacity() {
        let list = EventList::new(1);
        let event: Event = unsafe { std::mem::zeroed() };
        assert!(list.push(event));
        assert!(!list.push(event));
        list.clear();
        assert!(list.push(event));
    }
}
