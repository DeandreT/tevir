use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{NodeId, Point, Rect};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScreenPlacement {
    pub node: NodeId,
    pub bounds: Rect,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transition<'a> {
    pub target: &'a NodeId,
    pub local_position: Point,
}

/// A connected, non-overlapping arrangement of node desktops.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Topology {
    screens: Vec<ScreenPlacement>,
}

impl Topology {
    pub fn new(screens: Vec<ScreenPlacement>) -> Result<Self, TopologyError> {
        if screens.is_empty() {
            return Err(TopologyError::Empty);
        }

        let mut nodes = HashSet::with_capacity(screens.len());
        for screen in &screens {
            if !nodes.insert(screen.node.clone()) {
                return Err(TopologyError::DuplicateNode(screen.node.clone()));
            }
        }

        for (index, first) in screens.iter().enumerate() {
            if u64::from(first.bounds.size.width.get()) > i32::MAX as u64 + 1
                || u64::from(first.bounds.size.height.get()) > i32::MAX as u64 + 1
                || first.bounds.right() > i64::from(i32::MAX) + 1
                || first.bounds.bottom() > i64::from(i32::MAX) + 1
            {
                return Err(TopologyError::CoordinateOverflow(first.node.clone()));
            }
            for second in &screens[index + 1..] {
                if first.bounds.overlaps(second.bounds) {
                    return Err(TopologyError::OverlappingScreens {
                        first: first.node.clone(),
                        second: second.node.clone(),
                    });
                }
            }
        }

        let connected = connected_screen_indexes(&screens);
        if connected.len() != screens.len()
            && let Some(node) = screens
                .iter()
                .enumerate()
                .find(|(index, _)| !connected.contains(index))
                .map(|(_, screen)| screen.node.clone())
        {
            return Err(TopologyError::DisconnectedNode(node));
        }

        Ok(Self { screens })
    }

    #[must_use]
    pub fn screens(&self) -> &[ScreenPlacement] {
        &self.screens
    }

    #[must_use]
    pub fn screen(&self, node: &NodeId) -> Option<&ScreenPlacement> {
        self.screens.iter().find(|screen| &screen.node == node)
    }

    #[must_use]
    pub fn screen_at(&self, point: Point) -> Option<&ScreenPlacement> {
        self.screens
            .iter()
            .find(|screen| screen.bounds.contains(point))
    }

    /// Finds the desktop entered through `edge` at an offset on the current edge.
    #[must_use]
    pub fn transition(&self, current: &NodeId, edge: Edge, offset: u32) -> Option<Transition<'_>> {
        let source = self.screen(current)?;
        let axis = match edge {
            Edge::Left | Edge::Right => {
                if offset >= source.bounds.size.height.get() {
                    return None;
                }
                source.bounds.top() + i64::from(offset)
            }
            Edge::Top | Edge::Bottom => {
                if offset >= source.bounds.size.width.get() {
                    return None;
                }
                source.bounds.left() + i64::from(offset)
            }
        };

        self.screens.iter().find_map(|candidate| {
            if candidate.node == source.node {
                return None;
            }

            let local_position = match edge {
                Edge::Left
                    if candidate.bounds.right() == source.bounds.left()
                        && contains_axis(
                            candidate.bounds.top(),
                            candidate.bounds.bottom(),
                            axis,
                        ) =>
                {
                    Point {
                        x: i32::try_from(candidate.bounds.size.width.get() - 1).ok()?,
                        y: i32::try_from(axis - candidate.bounds.top()).ok()?,
                    }
                }
                Edge::Right
                    if candidate.bounds.left() == source.bounds.right()
                        && contains_axis(
                            candidate.bounds.top(),
                            candidate.bounds.bottom(),
                            axis,
                        ) =>
                {
                    Point {
                        x: 0,
                        y: i32::try_from(axis - candidate.bounds.top()).ok()?,
                    }
                }
                Edge::Top
                    if candidate.bounds.bottom() == source.bounds.top()
                        && contains_axis(
                            candidate.bounds.left(),
                            candidate.bounds.right(),
                            axis,
                        ) =>
                {
                    Point {
                        x: i32::try_from(axis - candidate.bounds.left()).ok()?,
                        y: i32::try_from(candidate.bounds.size.height.get() - 1).ok()?,
                    }
                }
                Edge::Bottom
                    if candidate.bounds.top() == source.bounds.bottom()
                        && contains_axis(
                            candidate.bounds.left(),
                            candidate.bounds.right(),
                            axis,
                        ) =>
                {
                    Point {
                        x: i32::try_from(axis - candidate.bounds.left()).ok()?,
                        y: 0,
                    }
                }
                _ => return None,
            };

            Some(Transition {
                target: &candidate.node,
                local_position,
            })
        })
    }
}

const fn contains_axis(start: i64, end: i64, value: i64) -> bool {
    value >= start && value < end
}

fn connected_screen_indexes(screens: &[ScreenPlacement]) -> HashSet<usize> {
    let mut connected = HashSet::with_capacity(screens.len());
    let mut pending = VecDeque::from([0]);

    while let Some(index) = pending.pop_front() {
        if !connected.insert(index) {
            continue;
        }

        for (candidate_index, candidate) in screens.iter().enumerate() {
            if !connected.contains(&candidate_index)
                && screens[index].bounds.shares_edge(candidate.bounds)
            {
                pending.push_back(candidate_index);
            }
        }
    }

    connected
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TopologyError {
    #[error("topology must contain at least one screen")]
    Empty,
    #[error("node `{0}` appears more than once in the topology")]
    DuplicateNode(NodeId),
    #[error("screens for `{first}` and `{second}` overlap")]
    OverlappingScreens { first: NodeId, second: NodeId },
    #[error("screen for `{0}` extends beyond the supported coordinate range")]
    CoordinateOverflow(NodeId),
    #[error("screen for `{0}` does not share an edge with the connected topology")]
    DisconnectedNode(NodeId),
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{Edge, ScreenPlacement, Topology, TopologyError};
    use crate::{NodeId, Point, Rect, Size};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn screen(node_id: &str, x: i32, y: i32, width: u32, height: u32) -> ScreenPlacement {
        ScreenPlacement {
            node: node(node_id),
            bounds: Rect::new(
                Point { x, y },
                Size::new(
                    NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
                    NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
                ),
            ),
        }
    }

    #[test]
    fn preserves_physical_position_across_centered_desktops() {
        let topology = Topology::new(vec![
            screen("controller", 0, 180, 5760, 1080),
            screen("agent", 5760, 0, 2560, 1440),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        let top = topology
            .transition(&node("controller"), Edge::Right, 0)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));
        let middle = topology
            .transition(&node("controller"), Edge::Right, 540)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));
        let bottom = topology
            .transition(&node("controller"), Edge::Right, 1079)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));

        assert_eq!(top.target, &node("agent"));
        assert_eq!(top.local_position, Point { x: 0, y: 180 });
        assert_eq!(middle.local_position, Point { x: 0, y: 720 });
        assert_eq!(bottom.local_position, Point { x: 0, y: 1259 });
    }

    #[test]
    fn routes_between_mixed_screens_at_negative_coordinates() {
        let topology = Topology::new(vec![
            screen("left", -2560, -360, 2560, 1440),
            screen("right", 0, 0, 1920, 1080),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        let transition = topology
            .transition(&node("left"), Edge::Right, 900)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));

        assert_eq!(transition.target, &node("right"));
        assert_eq!(transition.local_position, Point { x: 0, y: 540 });
        assert_eq!(
            topology
                .screen_at(Point { x: -1, y: 719 })
                .map(|screen| &screen.node),
            Some(&node("left"))
        );
    }

    #[test]
    fn does_not_route_outside_a_partial_shared_edge() {
        let topology = Topology::new(vec![
            screen("controller", 0, 180, 5760, 1080),
            screen("agent", 5760, 0, 2560, 1440),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        assert!(
            topology
                .transition(&node("agent"), Edge::Left, 100)
                .is_none()
        );
        assert!(
            topology
                .transition(&node("agent"), Edge::Left, 180)
                .is_some()
        );
    }

    #[test]
    fn rejects_overlapping_screens() {
        let result = Topology::new(vec![
            screen("left", 0, 0, 1920, 1080),
            screen("right", 1919, 0, 1920, 1080),
        ]);

        assert!(matches!(
            result,
            Err(TopologyError::OverlappingScreens { .. })
        ));
    }

    #[test]
    fn rejects_disconnected_screens() {
        let result = Topology::new(vec![
            screen("left", 0, 0, 1920, 1080),
            screen("island", 3000, 0, 1920, 1080),
        ]);

        assert!(matches!(
            result,
            Err(TopologyError::DisconnectedNode(node_id)) if node_id == node("island")
        ));
    }

    #[test]
    fn rejects_screens_larger_than_the_coordinate_domain() {
        let result = Topology::new(vec![screen("wide", i32::MIN, 0, u32::MAX, 1080)]);

        assert!(matches!(
            result,
            Err(TopologyError::CoordinateOverflow(node_id)) if node_id == node("wide")
        ));
    }
}
