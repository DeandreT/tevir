use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{NodeId, Point, Size};

pub const TOPOLOGY_COLUMNS: u8 = 5;
pub const TOPOLOGY_ROWS: u8 = 5;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct GridSlot {
    pub column: u8,
    pub row: u8,
}

impl GridSlot {
    #[must_use]
    pub const fn new(column: u8, row: u8) -> Self {
        Self { column, row }
    }

    #[must_use]
    pub const fn neighbor(self, edge: Edge) -> Option<Self> {
        match edge {
            Edge::Left if self.column > 0 => Some(Self::new(self.column - 1, self.row)),
            Edge::Right if self.column + 1 < TOPOLOGY_COLUMNS => {
                Some(Self::new(self.column + 1, self.row))
            }
            Edge::Top if self.row > 0 => Some(Self::new(self.column, self.row - 1)),
            Edge::Bottom if self.row + 1 < TOPOLOGY_ROWS => {
                Some(Self::new(self.column, self.row + 1))
            }
            Edge::Left | Edge::Right | Edge::Top | Edge::Bottom => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScreenPlacement {
    pub node: NodeId,
    pub slot: GridSlot,
    pub size: Size,
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

/// A connected arrangement of node desktops on the fixed topology grid.
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
        let mut slots = HashSet::with_capacity(screens.len());
        for screen in &screens {
            if !nodes.insert(screen.node.clone()) {
                return Err(TopologyError::DuplicateNode(screen.node.clone()));
            }
            if screen.slot.column >= TOPOLOGY_COLUMNS || screen.slot.row >= TOPOLOGY_ROWS {
                return Err(TopologyError::SlotOutOfBounds {
                    node: screen.node.clone(),
                    slot: screen.slot,
                });
            }
            if !slots.insert(screen.slot) {
                return Err(TopologyError::OccupiedSlot(screen.slot));
            }
        }

        let connected = connected_screen_indexes(&screens);
        if connected.len() != screens.len() {
            let disconnected = screens
                .iter()
                .enumerate()
                .find(|(index, _)| !connected.contains(index))
                .map(|(_, screen)| screen.node.clone());
            if let Some(node) = disconnected {
                return Err(TopologyError::DisconnectedNode(node));
            }
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
    pub fn screen_in_slot(&self, slot: GridSlot) -> Option<&ScreenPlacement> {
        self.screens.iter().find(|screen| screen.slot == slot)
    }

    /// Finds the desktop entered through `edge` at an offset on the current edge.
    #[must_use]
    pub fn transition(&self, current: &NodeId, edge: Edge, offset: u32) -> Option<Transition<'_>> {
        let source = self.screen(current)?;
        let source_length = edge_length(source.size, edge);
        if offset >= source_length {
            return None;
        }
        let target_slot = source.slot.neighbor(edge)?;
        let target = self.screen_in_slot(target_slot)?;
        let mapped = map_edge_offset(offset, source_length, edge_length(target.size, edge));
        let maximum_x = i32::try_from(target.size.width.get() - 1).ok()?;
        let maximum_y = i32::try_from(target.size.height.get() - 1).ok()?;
        let mapped = i32::try_from(mapped).ok()?;
        let local_position = match edge {
            Edge::Left => Point {
                x: maximum_x,
                y: mapped,
            },
            Edge::Right => Point { x: 0, y: mapped },
            Edge::Top => Point {
                x: mapped,
                y: maximum_y,
            },
            Edge::Bottom => Point { x: mapped, y: 0 },
        };
        Some(Transition {
            target: &target.node,
            local_position,
        })
    }
}

const fn edge_length(size: Size, edge: Edge) -> u32 {
    match edge {
        Edge::Left | Edge::Right => size.height.get(),
        Edge::Top | Edge::Bottom => size.width.get(),
    }
}

fn map_edge_offset(offset: u32, source_length: u32, target_length: u32) -> u32 {
    let source_span = source_length.saturating_sub(1);
    let target_span = target_length.saturating_sub(1);
    if source_span == 0 {
        return 0;
    }
    let numerator = u64::from(offset)
        .saturating_mul(u64::from(target_span))
        .saturating_add(u64::from(source_span / 2));
    u32::try_from(numerator / u64::from(source_span))
        .unwrap_or(u32::MAX)
        .min(target_span)
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
                && slots_are_adjacent(screens[index].slot, candidate.slot)
            {
                pending.push_back(candidate_index);
            }
        }
    }

    connected
}

const fn slots_are_adjacent(first: GridSlot, second: GridSlot) -> bool {
    (first.column == second.column && first.row.abs_diff(second.row) == 1)
        || (first.row == second.row && first.column.abs_diff(second.column) == 1)
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TopologyError {
    #[error("topology must contain at least one screen")]
    Empty,
    #[error("node `{0}` appears more than once in the topology")]
    DuplicateNode(NodeId),
    #[error(
        "screen for `{node}` uses grid slot ({}, {}), outside the {}x{} topology",
        slot.column,
        slot.row,
        TOPOLOGY_COLUMNS,
        TOPOLOGY_ROWS
    )]
    SlotOutOfBounds { node: NodeId, slot: GridSlot },
    #[error("grid slot ({}, {}) contains more than one screen", .0.column, .0.row)]
    OccupiedSlot(GridSlot),
    #[error("screen for `{0}` is not adjacent to the connected topology")]
    DisconnectedNode(NodeId),
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{Edge, GridSlot, ScreenPlacement, TOPOLOGY_COLUMNS, Topology, TopologyError};
    use crate::{NodeId, Point, Size};

    fn node(value: &str) -> NodeId {
        NodeId::new(value).unwrap_or_else(|error| panic!("invalid test node: {error}"))
    }

    fn screen(node_id: &str, column: u8, row: u8, width: u32, height: u32) -> ScreenPlacement {
        ScreenPlacement {
            node: node(node_id),
            slot: GridSlot::new(column, row),
            size: Size::new(
                NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
            ),
        }
    }

    #[test]
    fn maps_edge_positions_proportionally_between_desktops() {
        let topology = Topology::new(vec![
            screen("left", 1, 2, 1920, 1080),
            screen("right", 2, 2, 2560, 1440),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        let middle = topology
            .transition(&node("left"), Edge::Right, 540)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));
        let top = topology
            .transition(&node("left"), Edge::Right, 0)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));
        let bottom = topology
            .transition(&node("left"), Edge::Right, 1079)
            .unwrap_or_else(|| panic!("expected an adjacent screen"));

        assert_eq!(middle.target, &node("right"));
        assert_eq!(middle.local_position, Point { x: 0, y: 720 });
        assert_eq!(top.local_position, Point { x: 0, y: 0 });
        assert_eq!(bottom.local_position, Point { x: 0, y: 1439 });
    }

    #[test]
    fn routes_in_each_grid_direction() {
        let topology = Topology::new(vec![
            screen("center", 2, 2, 1920, 1080),
            screen("left", 1, 2, 1920, 1080),
            screen("right", 3, 2, 1920, 1080),
            screen("top", 2, 1, 1920, 1080),
            screen("bottom", 2, 3, 1920, 1080),
        ])
        .unwrap_or_else(|error| panic!("topology should be valid: {error}"));

        assert_eq!(
            topology
                .transition(&node("center"), Edge::Left, 10)
                .map(|transition| transition.target),
            Some(&node("left"))
        );
        assert_eq!(
            topology
                .transition(&node("center"), Edge::Right, 10)
                .map(|transition| transition.target),
            Some(&node("right"))
        );
        assert_eq!(
            topology
                .transition(&node("center"), Edge::Top, 10)
                .map(|transition| transition.target),
            Some(&node("top"))
        );
        assert_eq!(
            topology
                .transition(&node("center"), Edge::Bottom, 10)
                .map(|transition| transition.target),
            Some(&node("bottom"))
        );
    }

    #[test]
    fn rejects_duplicate_and_out_of_range_slots() {
        assert!(matches!(
            Topology::new(vec![
                screen("first", 2, 2, 1920, 1080),
                screen("second", 2, 2, 1920, 1080),
            ]),
            Err(TopologyError::OccupiedSlot(_))
        ));
        assert!(matches!(
            Topology::new(vec![screen("outside", TOPOLOGY_COLUMNS, 0, 1920, 1080)]),
            Err(TopologyError::SlotOutOfBounds { .. })
        ));
    }

    #[test]
    fn rejects_disconnected_nodes() {
        assert!(matches!(
            Topology::new(vec![
                screen("center", 2, 2, 1920, 1080),
                screen("island", 4, 4, 1920, 1080),
            ]),
            Err(TopologyError::DisconnectedNode(node_id)) if node_id == node("island")
        ));
    }
}
