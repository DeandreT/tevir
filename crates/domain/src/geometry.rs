use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Size {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl Size {
    #[must_use]
    pub const fn new(width: NonZeroU32, height: NonZeroU32) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    #[must_use]
    pub const fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    #[must_use]
    pub const fn left(self) -> i64 {
        self.origin.x as i64
    }

    #[must_use]
    pub const fn top(self) -> i64 {
        self.origin.y as i64
    }

    #[must_use]
    pub const fn right(self) -> i64 {
        self.left() + self.size.width.get() as i64
    }

    #[must_use]
    pub const fn bottom(self) -> i64 {
        self.top() + self.size.height.get() as i64
    }

    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        self.left() < other.right()
            && self.right() > other.left()
            && self.top() < other.bottom()
            && self.bottom() > other.top()
    }

    #[must_use]
    pub fn shares_edge(self, other: Self) -> bool {
        let vertical_overlap = self.top() < other.bottom() && self.bottom() > other.top();
        let horizontal_overlap = self.left() < other.right() && self.right() > other.left();

        ((self.left() == other.right() || self.right() == other.left()) && vertical_overlap)
            || ((self.top() == other.bottom() || self.bottom() == other.top())
                && horizontal_overlap)
    }
}
