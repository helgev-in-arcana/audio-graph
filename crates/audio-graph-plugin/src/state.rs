//! Serialized state representation stored in DAW project files and presets.
//!
//! Sub-plugin state chunks are nested opaquely. Wrapper-owned data structures
//! (slot table, parameter bindings, node graph) are serialized independently
//! so that sub-plugins can be reloaded or swapped without losing graph configuration.

use serde::{Deserialize, Serialize};
use subhost_adapter::{
    InstanceState, Slot, SubHostState, SubPluginRef, base64_decode, base64_encode,
};

/// Everything the wrapper writes into the DAW's project file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WrapperState {
    /// Incremented on breaking schema changes.
    pub version: u32,
    pub slots: Vec<Slot>,
    /// Legacy single-sub-plugin reference for backwards compatibility with earlier state formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_plugin: Option<SubPluginRef>,
    /// Legacy single-sub-plugin state chunk for backwards compatibility.
    ///
    /// Base64 encoded inside the JSON string persisted by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_state: Option<String>,
    /// Hosted sub-plugin instances.
    ///
    /// In legacy project files where `sub_plugin` is set, [`instances`][WrapperState::instances]
    /// transparently maps the single sub-plugin to instance 0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_plugins: Vec<InstanceState>,
    /// The node graph, stored as a generic JSON value.
    ///
    /// Preserved as an opaque value so forward and backward schema versions survive round trips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<serde_json::Value>,
    /// Sub-block modulation quantum in samples.
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

    /// The hosted sub-plugins across all instances, supporting both multi-instance and legacy formats.
    ///
    /// For legacy project files containing a single `sub_plugin`, maps the entry to instance 0.
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
        // When updating state, clear legacy single-plugin fields now represented in `sub_plugins`.
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
        // States saved without a `graph` or `sub_block` should deserialize successfully with defaults.
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
        // Parameter bindings and state remain valid even when no sub-plugin is loaded.
        let state = WrapperState::new(vec![Slot::default(); 2]);
        let json = serde_json::to_string(&state).unwrap();
        let back: WrapperState = serde_json::from_str(&json).unwrap();
        assert!(back.sub_plugin.is_none());
        assert!(back.sub_state_bytes().is_none());
    }

    #[test]
    fn a_pre_m8_project_comes_back_in_the_new_shape() {
        // Legacy single-sub-plugin layouts are migrated to multi-instance format upon loading.
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
