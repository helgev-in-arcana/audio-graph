//! What this wrapper writes into the DAW's project file (ARCHITECTURE.md §8.3).
//!
//! The sub-plugin's own chunk is nested inside, opaque. The parts the wrapper
//! owns — slot table, bindings, the node graph — are serialised separately and
//! deliberately do *not* depend on the sub-plugin being present, so swapping
//! the sub-plugin leaves everything else standing.
//!
//! The layout lives here rather than in `subhost-adapter` because it is this
//! product's document: `graph` and `sub_block` mean nothing to a crate whose
//! job is nesting one plugin inside another. That crate fills in the two parts
//! it does own — the slots and the instances — through
//! [`SubHostState`][subhost_adapter::SubHostState].

use serde::{Deserialize, Serialize};
use subhost_adapter::{
    InstanceState, Slot, SubHostState, SubPluginRef, base64_decode, base64_encode,
};

/// Everything the wrapper writes into the DAW's project file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WrapperState {
    /// Bumped when the layout changes incompatibly. Present from the start so
    /// there is somewhere to branch when it does.
    pub version: u32,
    pub slots: Vec<Slot>,
    /// Pre-M8 projects hold one sub-plugin here. Read on load and folded into
    /// `instances`; never written any more (see [`WrapperState::instances`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_plugin: Option<SubPluginRef>,
    /// The pre-M8 sub-plugin's own opaque chunk.
    ///
    /// Base64 rather than raw bytes because this lives inside a JSON document
    /// that nice-plug persists as a string field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_state: Option<String>,
    /// The hosted sub-plugins, from M8 on (§14.1).
    ///
    /// Empty in a pre-M8 project, where `sub_plugin` carries the single one
    /// instead; [`instances`][WrapperState::instances] hides that difference
    /// from every caller so there is one shape to handle rather than two.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_plugins: Vec<InstanceState>,
    /// The node graph (§9), as its own JSON value.
    ///
    /// Held opaquely rather than as a `Graph` so that this crate — which is
    /// about nesting one plugin inside another and knows nothing about node
    /// graphs — does not grow a dependency on the engine to describe a field it
    /// only ever passes through. It also means a project saved by a newer
    /// version survives a round trip through an older one instead of losing the
    /// patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<serde_json::Value>,
    /// Sub-block size for modulation, in samples (§9.2).
    #[serde(default = "default_sub_block")]
    pub sub_block: u32,
}

fn default_sub_block() -> u32 {
    subhost_adapter::DEFAULT_QUANTUM
}

/// Current layout version.
pub const STATE_VERSION: u32 = 1;

impl WrapperState {
    pub fn new(slots: Vec<Slot>) -> WrapperState {
        WrapperState {
            version: STATE_VERSION,
            slots,
            sub_plugin: None,
            sub_state: None,
            sub_plugins: Vec::new(),
            graph: None,
            sub_block: default_sub_block(),
        }
    }

    pub fn set_sub_state(&mut self, bytes: &[u8]) {
        self.sub_state = Some(base64_encode(bytes));
    }

    pub fn sub_state_bytes(&self) -> Option<Vec<u8>> {
        base64_decode(self.sub_state.as_deref()?)
    }

    /// Record one instance's plugin and settings.
    pub fn set_instance(&mut self, instance: usize, reference: SubPluginRef, state: Option<&[u8]>) {
        let entry = InstanceState {
            instance,
            reference,
            state: state.map(base64_encode),
        };
        match self.sub_plugins.iter_mut().find(|e| e.instance == instance) {
            Some(existing) => *existing = entry,
            None => self.sub_plugins.push(entry),
        }
    }

    /// The hosted sub-plugins, whichever way the project spelled them.
    ///
    /// A project saved before M8 has one in `sub_plugin`; one saved after has
    /// any number in `sub_plugins`. Callers should not have to know which, so
    /// the old shape is presented as instance 0 of the new one.
    /// The parts `SubHost` owns, in the shape it wants them.
    pub fn sub_host_state(&self) -> SubHostState {
        SubHostState {
            slots: self.slots.clone(),
            instances: self.instances(),
        }
    }

    /// Take back what `SubHost` saved, leaving everything else as it is.
    pub fn set_sub_host_state(&mut self, state: SubHostState) {
        self.slots = state.slots;
        self.sub_plugins = state.instances;
        // A project that came in with the pre-M8 single sub-plugin has been
        // folded into `sub_plugins` by now, so the old fields would only be a
        // second, staler copy of the same thing.
        self.sub_plugin = None;
        self.sub_state = None;
    }

    pub fn instances(&self) -> Vec<InstanceState> {
        if !self.sub_plugins.is_empty() {
            return self.sub_plugins.clone();
        }
        match &self.sub_plugin {
            Some(reference) => vec![InstanceState {
                instance: 0,
                reference: reference.clone(),
                state: self.sub_state.clone(),
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_adapter::Binding;

    #[test]
    fn state_survives_a_json_round_trip() {
        let mut state = WrapperState::new(vec![
            Slot {
                name: Some("Cutoff".into()),
                binding: Some(Binding {
                    instance: 0,
                    plugin_id: "ABCD".repeat(8),
                    param_id: 7,
                    param_name: "Filter Cutoff".into(),
                }),
            },
            Slot::default(),
        ]);
        state.set_sub_state(&[0, 1, 2, 253, 254, 255]);

        let json = serde_json::to_string(&state).unwrap();
        let back: WrapperState = serde_json::from_str(&json).unwrap();

        assert_eq!(back, state);
        assert_eq!(
            back.sub_state_bytes().unwrap(),
            vec![0, 1, 2, 253, 254, 255]
        );
    }

    #[test]
    fn a_state_written_before_the_graph_existed_still_loads() {
        // Projects saved by M3 and M4 have no `graph` and no `sub_block`. They
        // must open with an empty graph and the default rate, not fail.
        let json = r#"{"version":1,"slots":[],"sub_plugin":null,"sub_state":null}"#;
        let state: WrapperState = serde_json::from_str(json).unwrap();
        assert!(state.graph.is_none());
        assert_eq!(state.sub_block, subhost_adapter::DEFAULT_QUANTUM);
    }

    #[test]
    fn a_graph_saved_by_a_newer_version_survives_a_round_trip() {
        // The field is opaque to `subhost-adapter` on purpose: nesting a plugin
        // must not be the reason a patch is lost when versions disagree.
        let json = r#"{"version":1,"slots":[],"sub_plugin":null,"sub_state":null,
                       "graph":{"nodes":[{"kind":"SomethingFromTheFuture"}]},"sub_block":64}"#;
        let state: WrapperState = serde_json::from_str(json).unwrap();
        let back: WrapperState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(back.graph, state.graph);
        assert_eq!(back.sub_block, 64);
    }

    #[test]
    fn a_state_with_no_sub_plugin_is_still_valid() {
        // Bindings outlive the plugin they point at (§8.3), so this has to
        // serialise cleanly.
        let state = WrapperState::new(vec![Slot::default(); 2]);
        let json = serde_json::to_string(&state).unwrap();
        let back: WrapperState = serde_json::from_str(&json).unwrap();
        assert!(back.sub_plugin.is_none());
        assert!(back.sub_state_bytes().is_none());
    }

    #[test]
    fn a_pre_m8_project_comes_back_in_the_new_shape() {
        // `instances()` hides the old single-sub-plugin layout from callers;
        // taking a round trip through `SubHost` must write the new one.
        let json = r#"{"version":1,"slots":[],
                       "sub_plugin":{"format":"vst3","plugin_id":"AAAA",
                                     "path_hint":"","display_name":"Thing"},
                       "sub_state":"AAEC"}"#;
        let mut state: WrapperState = serde_json::from_str(json).unwrap();
        let carried = state.sub_host_state();
        assert_eq!(carried.instances.len(), 1);
        assert_eq!(carried.instances[0].instance, 0);

        state.set_sub_host_state(carried);
        assert!(state.sub_plugin.is_none(), "the old field must not linger");
        assert_eq!(state.sub_plugins.len(), 1);
        assert_eq!(state.instances()[0].state_bytes().unwrap(), vec![0, 1, 2]);
    }
}
