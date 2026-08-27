//! The wrapper's own parameters, as the DAW sees them (ARCHITECTURE.md §8.1).
//!
//! The sub-plugin's parameters are deliberately not published. What the DAW
//! gets is a fixed bank of generic slots, each of which the user binds to one
//! sub-plugin parameter from inside the wrapper. That indirection is the whole
//! point: swapping the sub-plugin cannot invalidate the DAW's automation lanes,
//! because those lanes are attached to slots, not to the sub-plugin.

use std::sync::Arc;

use crate::config::SLOT_COUNT;
use nice_plug::params::persist::PersistentField;
use nice_plug::prelude::*;

/// Serialised wrapper state, carried through nice-plug's `#[persist]` support.
///
/// A string rather than a struct because the payload includes the sub-plugin's
/// opaque chunk, and nice-plug persists fields as JSON strings.
#[derive(Default)]
pub struct PersistedState(pub std::sync::RwLock<String>);

impl<'a> PersistentField<'a, String> for PersistedState {
    fn set(&self, new_value: String) {
        *self.0.write().unwrap() = new_value;
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&String) -> R,
    {
        f(&self.0.read().unwrap())
    }
}

#[derive(Params)]
pub struct WrapperParams {
    /// Everything the wrapper owns that is not a slot value: the slot table,
    /// the bindings, which sub-plugin is loaded, and its state chunk.
    ///
    /// Kept as one persisted blob rather than many fields so its layout can
    /// evolve without inventing a new parameter each time.
    #[persist = "wrapper-state"]
    pub state: PersistedState,

    /// The slot values themselves.
    ///
    /// Their ids are the stable strings `slot_1`..`slot_32` — nice-plug's array
    /// nesting appends `_{index + 1}` to the inner field's id. §8.1 asks for a
    /// fixed numeric ParamID block; nice-plug derives VST3 ParamIDs by hashing
    /// the string id, so the stable string is what actually delivers the
    /// guarantee that section is after — automation surviving a sub-plugin swap.
    ///
    /// No `group`. With one, nice-plug puts each element in a group of its own
    /// (`Slots 1`, `Slots 2`, …) and the DAW's parameter tree gains a level
    /// that contains exactly one parameter: `Audio Graph FX > Slots 1 > Slot`.
    /// Without it the tree is flat and reads `Audio Graph FX > Slot 1`, which is
    /// what a user picking an automation lane actually wants.
    #[nested(array)]
    pub slots: [SlotParam; SLOT_COUNT],
}

#[derive(Params)]
pub struct SlotParam {
    #[id = "slot"]
    pub value: FloatParam,
}

impl SlotParam {
    /// `index` is zero-based; the name is not.
    ///
    /// nice-plug numbers array elements from 1 in the parameter id, so a
    /// zero-based display name would disagree with the DAW about which slot is
    /// which — and the id is the thing automation is attached to.
    fn new(index: usize) -> SlotParam {
        SlotParam {
            // Plain 0..1: the slot itself has no units. The mapping onto the
            // bound parameter's real range happens in the adapter, where the
            // range is actually known.
            value: FloatParam::new(
                format!("Slot {}", index + 1),
                0.0,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            ),
        }
    }
}

impl Default for SlotParam {
    fn default() -> Self {
        SlotParam::new(0)
    }
}

impl Default for WrapperParams {
    fn default() -> Self {
        WrapperParams {
            state: PersistedState::default(),
            slots: std::array::from_fn(SlotParam::new),
        }
    }
}

impl WrapperParams {
    pub fn new() -> Arc<WrapperParams> {
        Arc::new(WrapperParams::default())
    }

    /// Current slot values, in the order the adapter expects.
    pub fn slot_values(&self, out: &mut Vec<f64>) {
        out.clear();
        out.extend(self.slots.iter().map(|s| s.value.value() as f64));
    }
}
