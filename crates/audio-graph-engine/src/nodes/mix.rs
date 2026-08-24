use serde::{Deserialize, Serialize};

/// Sum several audio inputs of the same width into one, each at its own
/// gain.
///
/// The only way two audio sources reach one destination. An input takes one
/// link everywhere in this graph, so mixing is a node rather than a rule —
/// and being a node is what lets the compiler see the merge and line the
/// paths up (§14.6).
///
/// The gains are here rather than in a node of their own because a mix with
/// one input *is* a gain, and the audio half needed a multiply anyway: a
/// feedback delay whose loop gain is one never decays, and `Math` is the
/// param half's multiply and cannot touch a buffer. Two nodes that share
/// every line of their implementation are one node.
///
/// Each gain has a socket of its own, after the audio inputs; the number
/// here is what is used while that socket is unconnected, the same rule
/// `Math`'s `b` follows. Missing entries are 1.0, which is what makes a
/// patch saved before the gains existed still mix the way it did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mix {
    pub channels: u16,
    pub inputs: u8,
    #[serde(default)]
    pub gains: Vec<f64>,
}
