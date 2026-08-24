//! What a socket carries, and what a socket is.
//!
//! Its own module because both sides need it and neither owns it: a node
//! declares its ports, and the graph decides from them what may be joined to
//! what.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// What a port carries (§14.3).
///
/// Ports only connect to ports of the same type. Mono-to-stereo is deliberately
/// not implicit: a hidden widening rule is the same kind of thing as a hidden
/// mixing rule, and the graph already says no to those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortType {
    /// A scalar. One value per sub-block (§9.2).
    Param,
    /// Audio, `channels` wide.
    Audio { channels: u16 },
    /// Note events.
    Note,
}

impl PortType {
    pub const STEREO: PortType = PortType::Audio { channels: 2 };

    pub fn label(self) -> String {
        match self {
            PortType::Param => "param".into(),
            PortType::Audio { channels: 1 } => "mono".into(),
            PortType::Audio { channels: 2 } => "stereo".into(),
            PortType::Audio { channels } => format!("{channels} ch"),
            PortType::Note => "notes".into(),
        }
    }
}

/// One socket on a node.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: Cow<'static, str>,
    pub ty: PortType,
    /// Whether this is a *side* input rather than the signal path proper: a
    /// sidechain or another aux bus (§14.11).
    ///
    /// The compiler does not care — an aux bus is the bus at that index and
    /// nothing else. It is here because the editor does: a sidechain socket
    /// that looks exactly like the main input is one a user wires into by
    /// mistake, and the mistake is silent.
    pub aux: bool,
}

impl Port {
    pub(crate) fn new(name: impl Into<Cow<'static, str>>, ty: PortType) -> Port {
        Port {
            name: name.into(),
            ty,
            aux: false,
        }
    }

    pub(crate) fn param(name: impl Into<Cow<'static, str>>) -> Port {
        Port::new(name, PortType::Param)
    }

    pub(crate) fn aux(self) -> Port {
        Port { aux: true, ..self }
    }
}
