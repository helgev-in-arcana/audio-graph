//! Automatable parameter slots and sub-plugin parameter bindings.
//!
//! The sub-plugin's parameters are deliberately *not* exposed to the DAW. The
//! wrapper publishes a fixed set of slots instead, and each slot may be bound
//! to one sub-plugin parameter. That indirection is what lets the sub-plugin be
//! swapped without destroying the DAW's automation.

use plugin_host::{ParamId, ParamInfo};
use serde::{Deserialize, Serialize};

/// Binding descriptor connecting a parameter slot to a specific sub-plugin parameter.
///
/// Identified by `(instance, plugin_id, param_id)` rather than by index:
/// parameter *order* is not stable across plugin versions, and a binding that
/// silently re-points at a different control after an update is worse than one
/// that fails to resolve.
///
/// `instance` is what makes two copies of the same plugin two different
/// targets. Without it, a binding made against the second copy would also
/// resolve against the first and both would move together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// Which graph node's plugin this drives. Defaulted so a saved state from
    /// before multiple instances existed, which had exactly one sub-plugin,
    /// loads as a binding against instance 0 — which is what it was.
    #[serde(default)]
    pub instance: u32,
    /// Sub-plugin identifier string (e.g. VST3 class ID or CLAP plugin ID).
    pub plugin_id: String,
    /// Sub-plugin parameter ID.
    pub param_id: u32,
    /// Display name of the parameter at the time of binding. Remembered so an
    /// unresolved binding can still be shown meaningfully.
    pub param_name: String,
}

/// A published parameter slot, including user label and sub-plugin binding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// User-assigned display name for the slot. Hosts are not expected to
    /// honour a rename.
    pub name: Option<String>,
    pub binding: Option<Binding>,
}

/// Table of published parameter slots and their resolved sub-plugin parameter targets.
#[derive(Debug, Clone)]
pub struct SlotTable {
    /// Number of slots published by the wrapper.
    ///
    /// Fixed for a given wrapper because VST3 cannot add parameters at
    /// runtime, but the number itself is that wrapper's business. Held
    /// separately from `slots.len()` because
    /// [`load_state`][Self::load_state] takes a table of whatever length the
    /// project was saved with and has to resize it back to this.
    count: usize,
    slots: Vec<Slot>,
    /// Cached resolved targets for currently loaded sub-plugins, recomputed
    /// whenever a sub-plugin changes.
    ///
    /// Separate from `slots` because it is derived state: a binding survives
    /// even when it cannot currently be resolved, and conflating the two is
    /// what would delete a user's work when a plugin fails to load.
    resolved: Vec<Option<ResolvedTarget>>,
}

/// Target parameter resolution details for audio-thread automation mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTarget {
    /// The instance the parameter belongs to. Carried through to the audio
    /// thread so an event is delivered to the plugin it was bound against.
    pub instance: u32,
    pub id: ParamId,
    pub min: f64,
    pub max: f64,
}

impl ResolvedTarget {
    /// Maps a normalized `0.0..=1.0` slot value to the parameter's native value range `[min, max]`.
    pub fn to_plain(&self, normalized: f64) -> f64 {
        self.min + normalized.clamp(0.0, 1.0) * (self.max - self.min)
    }
}

impl SlotTable {
    pub fn new(count: usize) -> SlotTable {
        SlotTable {
            count,
            slots: vec![Slot::default(); count],
            resolved: vec![None; count],
        }
    }

    /// Returns the number of published slots.
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn slot(&self, index: usize) -> Option<&Slot> {
        self.slots.get(index)
    }

    /// Binds a slot to a sub-plugin parameter and resolves its target range.
    pub fn bind(&mut self, index: usize, instance: u32, plugin_id: &str, param: &ParamInfo) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        slot.binding = Some(Binding {
            instance,
            plugin_id: plugin_id.to_string(),
            param_id: param.id.0,
            param_name: param.name.clone(),
        });
        self.resolved[index] = Some(ResolvedTarget {
            instance,
            id: param.id,
            min: param.min,
            max: param.max,
        });
    }

    pub fn clear(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.binding = None;
            self.resolved[index] = None;
        }
    }

    pub fn rename(&mut self, index: usize, name: Option<String>) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.name = name;
        }
    }

    pub fn resolved(&self, index: usize) -> Option<ResolvedTarget> {
        self.resolved.get(index).copied().flatten()
    }

    /// Returns all active `(slot_index, ResolvedTarget)` pairs bound to
    /// `instance`.
    ///
    /// The audio thread takes this once per activate rather than walking the
    /// table per block. Filtered per instance so each sub-plugin is handed
    /// only the slots that were bound against it.
    pub fn active_targets(&self, instance: u32) -> Vec<(usize, ResolvedTarget)> {
        self.resolved
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.filter(|t| t.instance == instance).map(|t| (i, t)))
            .collect()
    }

    /// Re-resolves bindings targeting `instance` against a newly loaded
    /// sub-plugin's parameters.
    ///
    /// Only bindings that name this instance are touched: loading a plugin
    /// into one node must not disturb the parameters driving another.
    ///
    /// Bindings that do not match the new plugin ID or parameter list are
    /// *kept* and left unresolved, so reloading the original plugin brings
    /// them back. Deleting them would turn a missing file into lost work.
    pub fn resolve_against(&mut self, instance: u32, plugin_id: &str, params: &[ParamInfo]) {
        for (slot, resolved) in self.slots.iter().zip(self.resolved.iter_mut()) {
            let Some(binding) = slot.binding.as_ref().filter(|b| b.instance == instance) else {
                continue;
            };
            *resolved = if binding.plugin_id != plugin_id {
                None
            } else {
                params
                    .iter()
                    .find(|p| p.id.0 == binding.param_id)
                    .map(|p| ResolvedTarget {
                        instance,
                        id: p.id,
                        min: p.min,
                        max: p.max,
                    })
            };
        }
    }

    /// Clears resolved targets for `instance` while keeping configured bindings.
    pub fn unresolve(&mut self, instance: u32) {
        for resolved in self.resolved.iter_mut() {
            if resolved.is_some_and(|r| r.instance == instance) {
                *resolved = None;
            }
        }
    }

    /// Clears all resolved targets while keeping configured bindings.
    pub fn unresolve_all(&mut self) {
        self.resolved.iter_mut().for_each(|r| *r = None);
    }

    /// Returns a list of `(slot_index, &Binding)` pairs that are currently unresolved.
    pub fn unresolved(&self) -> Vec<(usize, &Binding)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(i, s)| s.binding.is_some() && self.resolved[*i].is_none())
            .filter_map(|(i, s)| s.binding.as_ref().map(|b| (i, b)))
            .collect()
    }

    pub fn to_state(&self) -> Vec<Slot> {
        self.slots.clone()
    }

    /// Restores slot definitions from state, resizing to the configured slot
    /// count.
    ///
    /// A saved table from a different build may be shorter or longer; it is
    /// resized rather than rejected, because refusing to load would cost the
    /// user their whole project.
    pub fn load_state(&mut self, slots: Vec<Slot>) {
        self.slots = slots;
        self.slots.resize(self.count, Slot::default());
        self.resolved = vec![None; self.count];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host::ParamFlags;

    /// Test slot count constant.
    const SLOTS: usize = 32;

    fn param(id: u32, name: &str, min: f64, max: f64) -> ParamInfo {
        ParamInfo {
            id: ParamId(id),
            name: name.into(),
            module: String::new(),
            min,
            max,
            default: min,
            flags: ParamFlags::AUTOMATABLE,
        }
    }

    #[test]
    fn a_bound_slot_maps_automation_onto_the_plain_range() {
        let mut table = SlotTable::new(SLOTS);
        table.bind(0, 0, "AAAA", &param(7, "Cutoff", 20.0, 20_000.0));
        let target = table.resolved(0).expect("resolved");
        assert_eq!(target.to_plain(0.0), 20.0);
        assert_eq!(target.to_plain(1.0), 20_000.0);
        assert_eq!(target.to_plain(0.5), 10_010.0);
    }

    #[test]
    fn a_binding_survives_a_plugin_that_does_not_resolve_it() {
        // A missing plugin must not delete the user's work: reloading it has
        // to bring the mapping back.
        let mut table = SlotTable::new(SLOTS);
        table.bind(3, 0, "AAAA", &param(7, "Cutoff", 0.0, 1.0));

        table.resolve_against(0, "BBBB", &[]);
        assert!(
            table.resolved(3).is_none(),
            "should not resolve against another plugin"
        );
        assert!(
            table.slot(3).unwrap().binding.is_some(),
            "binding must be kept"
        );
        assert_eq!(table.unresolved().len(), 1);

        table.resolve_against(0, "AAAA", &[param(7, "Cutoff", 0.0, 1.0)]);
        assert!(table.resolved(3).is_some(), "binding should come back");
        assert!(table.unresolved().is_empty());
    }

    #[test]
    fn bindings_follow_the_parameter_id_not_its_position() {
        let mut table = SlotTable::new(SLOTS);
        table.bind(0, 0, "AAAA", &param(42, "Drive", 0.0, 10.0));

        // A plugin update reorders its parameter list. Resolving by index
        // would silently re-point the slot at whatever now sits there.
        let reordered = [param(1, "Mix", 0.0, 1.0), param(42, "Drive", 0.0, 10.0)];
        table.resolve_against(0, "AAAA", &reordered);

        let target = table.resolved(0).expect("resolved");
        assert_eq!(target.id, ParamId(42));
        assert_eq!(target.max, 10.0);
    }

    #[test]
    fn two_copies_of_one_plugin_are_two_different_targets() {
        // Keying a binding on the plugin id alone made two copies
        // indistinguishable: a slot bound to the second one also resolved
        // against the first, so both moved together.
        let mut table = SlotTable::new(SLOTS);
        table.bind(0, 0, "AAAA", &param(7, "Cutoff", 0.0, 1.0));
        table.bind(1, 1, "AAAA", &param(7, "Cutoff", 0.0, 1.0));

        let params = [param(7, "Cutoff", 0.0, 1.0)];
        table.resolve_against(0, "AAAA", &params);
        table.resolve_against(1, "AAAA", &params);

        assert_eq!(
            table.active_targets(0).len(),
            1,
            "instance 0 should be driven by its own slot only"
        );
        assert_eq!(table.active_targets(0)[0].0, 0);
        assert_eq!(table.active_targets(1)[0].0, 1);
    }

    #[test]
    fn loading_one_instance_leaves_the_others_resolved() {
        // Loading a plugin into one node re-resolves that node's bindings. If
        // it rewrote the whole table, the other nodes' parameters would stop
        // being driven the moment anything else was loaded.
        let mut table = SlotTable::new(SLOTS);
        table.bind(0, 0, "AAAA", &param(7, "Cutoff", 0.0, 1.0));
        table.bind(1, 1, "BBBB", &param(9, "Drive", 0.0, 1.0));

        table.resolve_against(1, "BBBB", &[param(9, "Drive", 0.0, 1.0)]);

        assert!(table.resolved(0).is_some(), "instance 0 must be untouched");
        assert!(table.resolved(1).is_some());
    }

    #[test]
    fn unloading_one_instance_leaves_the_others_alone() {
        let mut table = SlotTable::new(SLOTS);
        table.bind(0, 0, "AAAA", &param(7, "Cutoff", 0.0, 1.0));
        table.bind(1, 1, "BBBB", &param(9, "Drive", 0.0, 1.0));

        table.unresolve(1);

        assert!(table.resolved(0).is_some());
        assert!(table.resolved(1).is_none());
        assert!(
            table.slot(1).unwrap().binding.is_some(),
            "binding must be kept"
        );
    }

    #[test]
    fn unloading_keeps_bindings_but_drops_resolutions() {
        let mut table = SlotTable::new(SLOTS);
        table.bind(1, 0, "AAAA", &param(7, "Cutoff", 0.0, 1.0));
        table.unresolve_all();
        assert!(table.resolved(1).is_none());
        assert!(table.slot(1).unwrap().binding.is_some());
    }

    #[test]
    fn state_from_a_different_slot_count_is_resized_not_rejected() {
        let mut table = SlotTable::new(SLOTS);
        table.load_state(vec![Slot::default(); 4]);
        assert_eq!(table.slots().len(), SLOTS);

        let mut table = SlotTable::new(SLOTS);
        table.load_state(vec![Slot::default(); SLOTS + 16]);
        assert_eq!(table.slots().len(), SLOTS);
    }
}
