//! Platform-neutral domain types for Tevir.

mod geometry;
mod host;
mod identity;
mod input;
mod topology;

pub use geometry::{Point, Rect, Size};
pub use host::HostPlatform;
pub use identity::{NodeId, NodeIdError};
pub use input::{
    ButtonAction, InputEvent, InputKind, KeyAction, MouseButton, PhysicalKey, ScrollDelta,
};
pub use topology::{
    Edge, GridSlot, ScreenPlacement, TOPOLOGY_COLUMNS, TOPOLOGY_ROWS, Topology, TopologyError,
    Transition,
};
