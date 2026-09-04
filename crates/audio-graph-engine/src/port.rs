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

/// What the button that takes a socket away is doing on that socket's row.
///
/// Three states rather than two, because a group at its floor still has rows:
/// the button stays on them, greyed. It is drawn there rather than left out
/// because a row is laid out from the socket inwards, so a button that comes
/// and goes takes its width from everything beside it — and it would come and
/// go exactly while the user is adding and removing sockets, which is when
/// they are clicking along those rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remove {
    /// No button: this socket is not one of a group the user grows.
    None,
    /// A button that works.
    Offered,
    /// A button that does not, because the group has none to spare.
    Held,
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
    /// What the button that takes this socket away is doing on its row — a
    /// mix's inputs and a plugin's parameter sockets are the groups that have
    /// one. For how many sockets actually go, see
    /// [`NodeKind::remove_input`][crate::NodeKind::remove_input].
    pub remove: Remove,
}

impl Port {
    pub(crate) fn new(name: impl Into<Cow<'static, str>>, ty: PortType) -> Port {
        Port {
            name: name.into(),
            ty,
            aux: false,
            remove: Remove::None,
        }
    }

    pub(crate) fn param(name: impl Into<Cow<'static, str>>) -> Port {
        Port::new(name, PortType::Param)
    }

    pub(crate) fn aux(self) -> Port {
        Port { aux: true, ..self }
    }

    /// Mark this port as the first socket of a group the user grows and
    /// shrinks. `offered` says whether the button may be clicked now — a
    /// group down to what it has to keep holds its button rather than
    /// dropping it, see [`Remove::Held`].
    #[cfg(feature = "ui")]
    pub(crate) fn removable(self, offered: bool) -> Port {
        Port {
            remove: if offered {
                Remove::Offered
            } else {
                Remove::Held
            },
            ..self
        }
    }
}
