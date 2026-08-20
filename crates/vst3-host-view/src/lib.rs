//! Sub-plugin editor windows.
//!
//! Split out from `vst3-host` because it is the part that disappears when the
//! backend is used for anything else — an offline renderer or a scanner needs
//! none of it (ARCHITECTURE.md §2). It is also, per ADR-3, the piece that moves
//! wholesale into the child process if ADR-6 is ever triggered, since a child
//! process would create its own top-level window and no cross-process window
//! embedding would be needed at all.
//!
//! Three concerns, one per module: the container window, the `IPlugFrame` the
//! plugin talks back through, and the ordering rules that keep teardown from
//! crashing.

mod deferred;
mod editor;
mod frame;
mod keys;
mod window;

pub use deferred::{Deferred, new as deferred};
pub use editor::{EditorWindow, can_resize};
pub use frame::PlugFrame;
pub use keys::forward as forward_key;
pub use window::{ContainerWindow, PLATFORM_TYPE, Size, WindowState, pump_events, root_window};
