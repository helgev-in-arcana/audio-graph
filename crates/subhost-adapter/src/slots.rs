//! Slots: the wrapper's own parameters, and their bindings to sub-plugin ones.
//!
//! ARCHITECTURE.md §8. The sub-plugin's parameters are deliberately *not*
//! exposed to the DAW. The wrapper publishes a fixed set of slots instead, and
//! each slot may be bound to one sub-plugin parameter. That indirection is what
//! lets the sub-plugin be swapped without destroying the DAW's automation.

use plugin_host_api::{ParamId, ParamInfo};
use serde::{Deserialize, Serialize};

/// How many slots the wrapper publishes.
///
/// Fixed because VST3 cannot add parameters at runtime (§8.1). CLAP can, and
/// will, but the engine only ever sees an abstract slot table, so that stays a
/// question for the outer layer.
pub const SLOT_COUNT: usize = 32;

/// Which sub-plugin parameter a slot drives.
///
/// Identified by `(plugin_id, param_id)` rather than by index: parameter
/// *order* is not stable across plugin versions, and a binding that silently
/// re-points at a different control after an update is worse than one that
/// fails to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// The sub-plugin this binding was made against (VST3 CID as hex, §8.3).
    pub plugin_id: String,
    /// The parameter's stable id within that plugin.
    pub param_id: u32,
    /// Remembered so an unresolved binding can still be shown meaningfully.
    pub param_name: String,
}

/// One slot, as the engine sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// User-supplied name. Hosts are not expected to honour a rename (§8.1).
    pub name: Option<String>,
    pub binding: Option<Binding>,
}

/// The wrapper's slot table plus the resolution of each binding against the
/// currently loaded sub-plugin.
#[derive(Debug, Clone)]
pub struct SlotTable {
    slots: Vec<Slot>,
    /// Resolved target per slot, recomputed whenever the sub-plugin changes.
    ///
    /// Separate from `slots` because it is derived state: a binding survives
    /// even when it cannot currently be resolved (§8.3), and conflating the two
    /// is what would delete a user's work when a plugin fails to load.
    resolved: Vec<Option<ResolvedTarget>>,
}

/// A binding that currently points at a real parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTarget {
    pub id: ParamId,
    pub min: f64,
    pub max: f64,
}

impl ResolvedTarget {
    /// Map a slot's 0..1 automation value onto the parameter's plain range.
    pub fn to_plain(&self, normalized: f64) -> f64 {
        self.min + normalized.clamp(0.0, 1.0) * (self.max - self.min)
    }
}

impl Default for SlotTable {
    fn default() -> Self {
        SlotTable {
            slots: vec![Slot::default(); SLOT_COUNT],
            resolved: vec![None; SLOT_COUNT],
        }
    }
}

impl SlotTable {
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn slot(&self, index: usize) -> Option<&Slot> {
        self.slots.get(index)
    }

    /// Bind a slot to a sub-plugin parameter, replacing any previous binding.
    pub fn bind(&mut self, index: usize, plugin_id: &str, param: &ParamInfo) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        slot.binding = Some(Binding {
            plugin_id: plugin_id.to_string(),
            param_id: param.id.0,
            param_name: param.name.clone(),
        });
        self.resolved[index] = Some(ResolvedTarget {
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

    /// Every slot that currently drives something, as `(slot index, target)`.
    ///
    /// The audio thread takes this once per activate rather than walking the
    /// table per block.
    pub fn active_targets(&self) -> Vec<(usize, ResolvedTarget)> {
        self.resolved
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.map(|t| (i, t)))
            .collect()
    }

    /// Re-resolve every binding against a newly loaded sub-plugin.
    ///
    /// Bindings that do not match are *kept* and simply left unresolved, so
    /// reloading the original plugin brings them back (§8.3). Deleting them
    /// would turn a missing file into lost work.
    pub fn resolve_against(&mut self, plugin_id: &str, params: &[ParamInfo]) {
        for (slot, resolved) in self.slots.iter().zip(self.resolved.iter_mut()) {
            *resolved = slot.binding.as_ref().and_then(|binding| {
                if binding.plugin_id != plugin_id {
                    return None;
                }
                params
                    .iter()
                    .find(|p| p.id.0 == binding.param_id)
                    .map(|p| ResolvedTarget {
                        id: p.id,
                        min: p.min,
                        max: p.max,
                    })
            });
        }
    }

    /// Drop every resolution without touching the bindings, for when the
    /// sub-plugin is unloaded.
    pub fn unresolve_all(&mut self) {
        self.resolved.iter_mut().for_each(|r| *r = None);
    }

    /// Bindings that are held but do not currently point anywhere.
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

    /// Restore slots from saved state.
    ///
    /// A saved table from a different build may be shorter or longer; it is
    /// resized rather than rejected, because refusing to load costs the user
    /// their whole project.
    pub fn load_state(&mut self, slots: Vec<Slot>) {
        self.slots = slots;
        self.slots.resize(SLOT_COUNT, Slot::default());
        self.resolved = vec![None; SLOT_COUNT];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host_api::ParamFlags;

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
        let mut table = SlotTable::default();
        table.bind(0, "AAAA", &param(7, "Cutoff", 20.0, 20_000.0));
        let target = table.resolved(0).expect("resolved");
        assert_eq!(target.to_plain(0.0), 20.0);
        assert_eq!(target.to_plain(1.0), 20_000.0);
        assert_eq!(target.to_plain(0.5), 10_010.0);
    }

    #[test]
    fn a_binding_survives_a_plugin_that_does_not_resolve_it() {
        // The whole point of §8.3: a missing plugin must not delete the user's
        // work, because reloading it has to bring the mapping back.
        let mut table = SlotTable::default();
        table.bind(3, "AAAA", &param(7, "Cutoff", 0.0, 1.0));

        table.resolve_against("BBBB", &[]);
        assert!(
            table.resolved(3).is_none(),
            "should not resolve against another plugin"
        );
        assert!(
            table.slot(3).unwrap().binding.is_some(),
            "binding must be kept"
        );
        assert_eq!(table.unresolved().len(), 1);

        table.resolve_against("AAAA", &[param(7, "Cutoff", 0.0, 1.0)]);
        assert!(table.resolved(3).is_some(), "binding should come back");
        assert!(table.unresolved().is_empty());
    }

    #[test]
    fn bindings_follow_the_parameter_id_not_its_position() {
        let mut table = SlotTable::default();
        table.bind(0, "AAAA", &param(42, "Drive", 0.0, 10.0));

        // A plugin update reorders its parameter list. Resolving by index would
        // silently re-point the slot at whatever now sits in that position.
        let reordered = [param(1, "Mix", 0.0, 1.0), param(42, "Drive", 0.0, 10.0)];
        table.resolve_against("AAAA", &reordered);

        let target = table.resolved(0).expect("resolved");
        assert_eq!(target.id, ParamId(42));
        assert_eq!(target.max, 10.0);
    }

    #[test]
    fn unloading_keeps_bindings_but_drops_resolutions() {
        let mut table = SlotTable::default();
        table.bind(1, "AAAA", &param(7, "Cutoff", 0.0, 1.0));
        table.unresolve_all();
        assert!(table.resolved(1).is_none());
        assert!(table.slot(1).unwrap().binding.is_some());
    }

    #[test]
    fn state_from_a_different_slot_count_is_resized_not_rejected() {
        let mut table = SlotTable::default();
        table.load_state(vec![Slot::default(); 4]);
        assert_eq!(table.slots().len(), SLOT_COUNT);

        let mut table = SlotTable::default();
        table.load_state(vec![Slot::default(); SLOT_COUNT + 16]);
        assert_eq!(table.slots().len(), SLOT_COUNT);
    }
}
