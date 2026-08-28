//! Node port definitions and types.
//!
//! Defines the data types that sockets can carry and the metadata associated
//! with input and output ports on nodes.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// The data type carried across a connection between ports.
///
/// Connections are only valid between ports of matching types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortType {
    /// A scalar parameter value, evaluated per sub-block.
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
    /// Whether this is an auxiliary input (e.g. sidechain) rather than the main signal path.
    ///
    /// Used by the UI to visually distinguish auxiliary inputs from main signal inputs.
    pub aux: bool,
    /// Indicates whether this socket represents a dynamically removable input group
    /// (e.g. mix bus inputs or plugin parameter ports).
    ///
    /// Used by the UI to render removal controls for dynamic port sets.
    pub removable: bool,
}

impl Port {
    pub(crate) fn new(name: impl Into<Cow<'static, str>>, ty: PortType) -> Port {
        Port {
            name: name.into(),
            ty,
            aux: false,
            removable: false,
        }
    }

    pub(crate) fn param(name: impl Into<Cow<'static, str>>) -> Port {
        Port::new(name, PortType::Param)
    }

    pub(crate) fn aux(self) -> Port {
        Port { aux: true, ..self }
    }

    /// Mark this port as the first socket of a group the user may remove.
    #[cfg(feature = "ui")]
    pub(crate) fn removable(self) -> Port {
        Port {
            removable: true,
            ..self
        }
    }
}
