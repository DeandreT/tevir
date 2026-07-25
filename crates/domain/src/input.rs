use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

/// A USB HID usage, retained as a physical key across operating systems.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PhysicalKey {
    pub usage_page: u16,
    pub usage_id: u16,
}

impl PhysicalKey {
    #[must_use]
    pub const fn new(usage_page: u16, usage_id: u16) -> Self {
        Self {
            usage_page,
            usage_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Press,
    Repeat { count: NonZeroU16 },
    Release,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonAction {
    Press,
    Release,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Primary,
    Secondary,
    Middle,
    Back,
    Forward,
    Other(u16),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDelta {
    /// Wheel movement where 120 units represent one detent.
    Discrete {
        horizontal_120: i32,
        vertical_120: i32,
    },
    /// Micropixels from a high-resolution scrolling device.
    Continuous {
        horizontal_micropixels: i64,
        vertical_micropixels: i64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Key {
        key: PhysicalKey,
        action: KeyAction,
    },
    PointerButton {
        button: MouseButton,
        action: ButtonAction,
    },
    PointerRelative {
        dx_micropixels: i64,
        dy_micropixels: i64,
    },
    PointerAbsolute(Point),
    Scroll(ScrollDelta),
}

use crate::Point;

/// One input event timestamped relative to the start of its session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputEvent {
    pub elapsed_micros: u64,
    pub kind: InputKind,
}
