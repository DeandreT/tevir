//! Platform-neutral domain types for Tevir.

mod display;
mod geometry;
mod host;
mod identity;
mod input;
mod topology;

pub use display::{
    DesktopLayout, DesktopLayoutError, DisplayRotation, MAX_MONITOR_NAME_BYTES,
    MAX_MONITORS_PER_DESKTOP, Monitor,
};
pub use geometry::{Point, Rect, Size};
pub use host::HostPlatform;
pub use identity::{NodeId, NodeIdError};
pub use input::{
    ButtonAction, InputEvent, InputKind, KeyAction, MouseButton, PhysicalKey, ScrollDelta,
};
pub use topology::{Edge, ScreenPlacement, Topology, TopologyError, Transition};
