//! The wrapper's persisted state (ARCHITECTURE.md §8.3).
//!
//! The sub-plugin's own chunk is nested inside, opaque. The parts the wrapper
//! owns — slot table, bindings, and eventually the node graph — are serialised
//! separately and deliberately do *not* depend on the sub-plugin being present,
//! so swapping the sub-plugin leaves everything else standing.

use serde::{Deserialize, Serialize};

use crate::slots::Slot;

/// Identifies which sub-plugin to reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubPluginRef {
    /// `"vst3"` today; the field exists so a CLAP reference (M6) is additive.
    pub format: String,
    /// Authoritative identity: the VST3 class id in its platform-independent
    /// hex form, so a project moves between machines and operating systems.
    pub plugin_id: String,
    /// Where it was last found. A *hint* only — plugin folders differ between
    /// machines, so a missing path triggers a search by `plugin_id` rather than
    /// a failure.
    pub path_hint: String,
    /// For showing the user what is missing when it cannot be found.
    pub display_name: String,
}

/// Everything the wrapper writes into the DAW's project file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WrapperState {
    /// Bumped when the layout changes incompatibly. Present from the start so
    /// there is somewhere to branch when it does.
    pub version: u32,
    pub slots: Vec<Slot>,
    pub sub_plugin: Option<SubPluginRef>,
    /// The sub-plugin's own opaque chunk.
    ///
    /// Base64 rather than raw bytes because this lives inside a JSON document
    /// that nice-plug persists as a string field.
    pub sub_state: Option<String>,
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
    crate::schedule::DEFAULT_QUANTUM
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
}

/// Minimal standard base64. Written out rather than taken as a dependency:
/// it is thirty lines, and this is the only place the project needs it.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }

    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for &c in chunk {
            n = n << 6 | if c == b'=' { 0 } else { value(c)? };
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::Binding;

    #[test]
    fn state_survives_a_json_round_trip() {
        let mut state = WrapperState::new(vec![
            Slot {
                name: Some("Cutoff".into()),
                binding: Some(Binding {
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
    fn base64_handles_every_padding_case() {
        for len in 0..8 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let encoded = base64_encode(&bytes);
            assert_eq!(encoded.len() % 4, 0, "len {len} produced unpadded output");
            assert_eq!(base64_decode(&encoded), Some(bytes), "len {len}");
        }
    }

    #[test]
    fn base64_rejects_garbage_instead_of_returning_nonsense() {
        // A corrupted project should surface as "sub-plugin state unreadable",
        // not as a chunk of wrong bytes handed to a third-party plugin.
        assert_eq!(base64_decode("!!!!"), None);
        assert_eq!(base64_decode("abc"), None);
    }

    #[test]
    fn a_state_written_before_the_graph_existed_still_loads() {
        // Projects saved by M3 and M4 have no `graph` and no `sub_block`. They
        // must open with an empty graph and the default rate, not fail.
        let json = r#"{"version":1,"slots":[],"sub_plugin":null,"sub_state":null}"#;
        let state: WrapperState = serde_json::from_str(json).unwrap();
        assert!(state.graph.is_none());
        assert_eq!(state.sub_block, crate::schedule::DEFAULT_QUANTUM);
    }

    #[test]
    fn a_graph_saved_by_a_newer_version_survives_a_round_trip() {
        // The field is opaque here on purpose: this crate must not be the
        // reason a patch is lost when versions disagree.
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
}
