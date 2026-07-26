use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Point, Rect, Size};

pub const MAX_MONITORS_PER_DESKTOP: usize = 32;
pub const MAX_MONITOR_NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayRotation {
    Normal,
    Clockwise90,
    Clockwise180,
    Clockwise270,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Monitor {
    pub name: Option<String>,
    /// Bounds relative to the aggregate desktop origin.
    pub bounds: Rect,
    /// Desktop scale in thousandths, where 1000 is 100%.
    pub scale_milli: NonZeroU32,
    pub rotation: DisplayRotation,
}

impl Monitor {
    #[must_use]
    pub fn new(name: Option<String>, bounds: Rect) -> Self {
        Self {
            name,
            bounds,
            scale_milli: NonZeroU32::new(1000).unwrap_or(NonZeroU32::MIN),
            rotation: DisplayRotation::Normal,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DesktopLayout {
    size: Size,
    monitors: Vec<Monitor>,
}

impl DesktopLayout {
    pub fn new(size: Size, monitors: Vec<Monitor>) -> Result<Self, DesktopLayoutError> {
        let layout = Self { size, monitors };
        layout.validate()?;
        Ok(layout)
    }

    #[must_use]
    pub fn single(size: Size) -> Self {
        Self {
            size,
            monitors: vec![Monitor::new(None, Rect::new(Point { x: 0, y: 0 }, size))],
        }
    }

    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    #[must_use]
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    #[must_use]
    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    pub fn validate(&self) -> Result<(), DesktopLayoutError> {
        if self.monitors.is_empty() {
            return Err(DesktopLayoutError::Empty);
        }
        if self.monitors.len() > MAX_MONITORS_PER_DESKTOP {
            return Err(DesktopLayoutError::TooManyMonitors {
                actual: self.monitors.len(),
                maximum: MAX_MONITORS_PER_DESKTOP,
            });
        }
        let mut reaches_left = false;
        let mut reaches_right = false;
        let mut reaches_top = false;
        let mut reaches_bottom = false;
        for (index, monitor) in self.monitors.iter().enumerate() {
            if monitor
                .name
                .as_ref()
                .is_some_and(|name| name.len() > MAX_MONITOR_NAME_BYTES)
            {
                return Err(DesktopLayoutError::MonitorNameTooLong {
                    index,
                    maximum: MAX_MONITOR_NAME_BYTES,
                });
            }
            if monitor.bounds.origin.x < 0
                || monitor.bounds.origin.y < 0
                || monitor.bounds.right() > i64::from(self.size.width.get())
                || monitor.bounds.bottom() > i64::from(self.size.height.get())
            {
                return Err(DesktopLayoutError::MonitorOutsideDesktop { index });
            }
            reaches_left |= monitor.bounds.left() == 0;
            reaches_right |= monitor.bounds.right() == i64::from(self.size.width.get());
            reaches_top |= monitor.bounds.top() == 0;
            reaches_bottom |= monitor.bounds.bottom() == i64::from(self.size.height.get());
        }
        if !(reaches_left && reaches_right && reaches_top && reaches_bottom) {
            return Err(DesktopLayoutError::DoesNotSpanDesktop);
        }
        Ok(())
    }

    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        self.monitors
            .iter()
            .any(|monitor| monitor.bounds.contains(point))
    }

    #[must_use]
    pub fn contains_edge_offset(&self, edge: crate::Edge, offset: u32) -> bool {
        self.monitors.iter().any(|monitor| match edge {
            crate::Edge::Left => {
                monitor.bounds.left() == 0
                    && contains_axis(
                        monitor.bounds.top(),
                        monitor.bounds.bottom(),
                        i64::from(offset),
                    )
            }
            crate::Edge::Right => {
                monitor.bounds.right() == i64::from(self.size.width.get())
                    && contains_axis(
                        monitor.bounds.top(),
                        monitor.bounds.bottom(),
                        i64::from(offset),
                    )
            }
            crate::Edge::Top => {
                monitor.bounds.top() == 0
                    && contains_axis(
                        monitor.bounds.left(),
                        monitor.bounds.right(),
                        i64::from(offset),
                    )
            }
            crate::Edge::Bottom => {
                monitor.bounds.bottom() == i64::from(self.size.height.get())
                    && contains_axis(
                        monitor.bounds.left(),
                        monitor.bounds.right(),
                        i64::from(offset),
                    )
            }
        })
    }

    #[must_use]
    pub fn edge_segments(&self, edge: crate::Edge) -> Vec<(u32, u32)> {
        self.monitors
            .iter()
            .filter_map(|monitor| match edge {
                crate::Edge::Left if monitor.bounds.left() == 0 => Some((
                    u32::try_from(monitor.bounds.top()).ok()?,
                    u32::try_from(monitor.bounds.bottom()).ok()?,
                )),
                crate::Edge::Right
                    if monitor.bounds.right() == i64::from(self.size.width.get()) =>
                {
                    Some((
                        u32::try_from(monitor.bounds.top()).ok()?,
                        u32::try_from(monitor.bounds.bottom()).ok()?,
                    ))
                }
                crate::Edge::Top if monitor.bounds.top() == 0 => Some((
                    u32::try_from(monitor.bounds.left()).ok()?,
                    u32::try_from(monitor.bounds.right()).ok()?,
                )),
                crate::Edge::Bottom
                    if monitor.bounds.bottom() == i64::from(self.size.height.get()) =>
                {
                    Some((
                        u32::try_from(monitor.bounds.left()).ok()?,
                        u32::try_from(monitor.bounds.right()).ok()?,
                    ))
                }
                _ => None,
            })
            .collect()
    }
}

const fn contains_axis(start: i64, end: i64, value: i64) -> bool {
    value >= start && value < end
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DesktopLayoutError {
    #[error("desktop layout must contain at least one monitor")]
    Empty,
    #[error("desktop layout contains {actual} monitors; the maximum is {maximum}")]
    TooManyMonitors { actual: usize, maximum: usize },
    #[error("monitor {index} name exceeds {maximum} bytes")]
    MonitorNameTooLong { index: usize, maximum: usize },
    #[error("monitor {index} extends outside the aggregate desktop")]
    MonitorOutsideDesktop { index: usize },
    #[error("monitor bounds do not span the aggregate desktop")]
    DoesNotSpanDesktop,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{DesktopLayout, DesktopLayoutError, Monitor};
    use crate::{Edge, Point, Rect, Size};

    fn size(width: u32, height: u32) -> Size {
        Size::new(
            NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
        )
    }

    #[test]
    fn exposed_monitor_edges_follow_the_real_layout() {
        let layout = DesktopLayout::new(
            size(3840, 2160),
            vec![
                Monitor::new(
                    Some(String::from("left")),
                    Rect::new(Point { x: 0, y: 1080 }, size(1920, 1080)),
                ),
                Monitor::new(
                    Some(String::from("top-right")),
                    Rect::new(Point { x: 1920, y: 0 }, size(1920, 1080)),
                ),
            ],
        )
        .unwrap_or_else(|error| panic!("layout should be valid: {error}"));

        assert!(!layout.contains_edge_offset(Edge::Left, 100));
        assert!(layout.contains_edge_offset(Edge::Left, 1200));
        assert!(layout.contains_edge_offset(Edge::Top, 2000));
        assert!(!layout.contains(Point { x: 100, y: 100 }));
    }

    #[test]
    fn rejects_monitors_outside_the_aggregate_desktop() {
        let result = DesktopLayout::new(
            size(1920, 1080),
            vec![Monitor::new(
                None,
                Rect::new(Point { x: 1, y: 0 }, size(1920, 1080)),
            )],
        );

        assert!(matches!(
            result,
            Err(DesktopLayoutError::MonitorOutsideDesktop { index: 0 })
        ));
    }
}
