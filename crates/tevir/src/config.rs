use std::{
    fs,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use domain::{
    DesktopLayout, Edge, NodeId, Point, Rect, ScreenPlacement, Size, Topology, TopologyError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Config {
    pub node: NodeId,
    pub role: Role,
}

#[derive(Clone, Debug)]
pub enum Role {
    Controller {
        listen: SocketAddr,
        topology: Topology,
        edge_behavior: EdgeBehavior,
    },
    Agent {
        controller_node: NodeId,
        controller: SocketAddr,
        display_layout: DesktopLayout,
    },
}

impl Config {
    pub fn new(node: NodeId, role: Role) -> Result<Self, ConfigError> {
        if let Role::Controller {
            topology,
            edge_behavior,
            ..
        } = &role
        {
            if topology.screen(&node).is_none() {
                return Err(ConfigError::MissingLocalScreen(node));
            }
            edge_behavior.validate()?;
        }
        if let Role::Agent {
            controller_node, ..
        } = &role
            && controller_node == &node
        {
            return Err(ConfigError::LocalController(node));
        }
        Ok(Self { node, role })
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let file: ConfigFile = toml::from_str(contents)?;
        file.validate()
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let contents = toml::to_string_pretty(&ConfigFile::from(self))?;
        fs::write(path, contents).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeRule {
    pub enabled: bool,
    pub active_start_percent: u8,
    pub active_end_percent: u8,
}

impl Default for EdgeRule {
    fn default() -> Self {
        Self {
            enabled: true,
            active_start_percent: 0,
            active_end_percent: 100,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeBehavior {
    pub switch_delay_ms: u32,
    pub corner_dead_zone_percent: u8,
    pub left: EdgeRule,
    pub right: EdgeRule,
    pub top: EdgeRule,
    pub bottom: EdgeRule,
}

impl EdgeBehavior {
    const MAX_SWITCH_DELAY_MS: u32 = 5_000;
    const MAX_CORNER_DEAD_ZONE_PERCENT: u8 = 49;

    #[must_use]
    pub const fn rule(&self, edge: Edge) -> EdgeRule {
        match edge {
            Edge::Left => self.left,
            Edge::Right => self.right,
            Edge::Top => self.top,
            Edge::Bottom => self.bottom,
        }
    }

    pub fn rule_mut(&mut self, edge: Edge) -> &mut EdgeRule {
        match edge {
            Edge::Left => &mut self.left,
            Edge::Right => &mut self.right,
            Edge::Top => &mut self.top,
            Edge::Bottom => &mut self.bottom,
        }
    }

    #[must_use]
    pub fn active_interval(&self, edge: Edge) -> Option<(f64, f64)> {
        let rule = self.rule(edge);
        if !rule.enabled {
            return None;
        }
        let start = rule.active_start_percent.max(self.corner_dead_zone_percent);
        let end = rule
            .active_end_percent
            .min(100u8.saturating_sub(self.corner_dead_zone_percent));
        (start < end).then(|| (f64::from(start) / 100.0, f64::from(end) / 100.0))
    }

    #[must_use]
    pub fn allows(&self, edge: Edge, position: f64) -> bool {
        self.active_interval(edge)
            .is_some_and(|(start, end)| position >= start && position <= end)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.switch_delay_ms > Self::MAX_SWITCH_DELAY_MS {
            return Err(ConfigError::InvalidEdgeBehavior(format!(
                "switch delay must not exceed {} ms",
                Self::MAX_SWITCH_DELAY_MS
            )));
        }
        if self.corner_dead_zone_percent > Self::MAX_CORNER_DEAD_ZONE_PERCENT {
            return Err(ConfigError::InvalidEdgeBehavior(format!(
                "corner dead zone must not exceed {}%",
                Self::MAX_CORNER_DEAD_ZONE_PERCENT
            )));
        }
        for (name, rule) in [
            ("left", self.left),
            ("right", self.right),
            ("top", self.top),
            ("bottom", self.bottom),
        ] {
            if rule.active_start_percent > 100
                || rule.active_end_percent > 100
                || rule.active_start_percent >= rule.active_end_percent
            {
                return Err(ConfigError::InvalidEdgeBehavior(format!(
                    "{name} edge active range must increase between 0% and 100%"
                )));
            }
        }
        Ok(())
    }
}

impl Default for EdgeBehavior {
    fn default() -> Self {
        Self {
            switch_delay_ms: 0,
            corner_dead_zone_percent: 2,
            left: EdgeRule::default(),
            right: EdgeRule::default(),
            top: EdgeRule::default(),
            bottom: EdgeRule::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    node: Node,
    role: RoleFile,
}

impl ConfigFile {
    fn validate(self) -> Result<Config, ConfigError> {
        let role = match self.role {
            RoleFile::Controller {
                listen,
                screens,
                edge_behavior,
            } => {
                let placements = screens.into_iter().map(Screen::into_placement).collect();
                Role::Controller {
                    listen,
                    topology: Topology::new(placements)?,
                    edge_behavior,
                }
            }
            RoleFile::Agent {
                controller_node,
                controller,
                display_width,
                display_height,
            } => Role::Agent {
                controller_node,
                controller,
                display_layout: DesktopLayout::single(Size::new(display_width, display_height)),
            },
        };

        Config::new(self.node.id, role)
    }
}

impl From<&Config> for ConfigFile {
    fn from(config: &Config) -> Self {
        let role = match &config.role {
            Role::Controller {
                listen,
                topology,
                edge_behavior,
            } => RoleFile::Controller {
                listen: *listen,
                screens: topology
                    .screens()
                    .iter()
                    .map(Screen::from_placement)
                    .collect(),
                edge_behavior: *edge_behavior,
            },
            Role::Agent {
                controller_node,
                controller,
                display_layout,
            } => RoleFile::Agent {
                controller_node: controller_node.clone(),
                controller: *controller,
                display_width: display_layout.size().width,
                display_height: display_layout.size().height,
            },
        };
        Self {
            node: Node {
                id: config.node.clone(),
            },
            role,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Node {
    id: NodeId,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
enum RoleFile {
    Controller {
        listen: SocketAddr,
        screens: Vec<Screen>,
        #[serde(default)]
        edge_behavior: EdgeBehavior,
    },
    Agent {
        controller_node: NodeId,
        controller: SocketAddr,
        display_width: NonZeroU32,
        display_height: NonZeroU32,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Screen {
    node: NodeId,
    x: i32,
    y: i32,
    width: NonZeroU32,
    height: NonZeroU32,
}

impl Screen {
    fn into_placement(self) -> ScreenPlacement {
        ScreenPlacement::single(
            self.node,
            Rect::new(
                Point {
                    x: self.x,
                    y: self.y,
                },
                Size::new(self.width, self.height),
            ),
        )
    }

    fn from_placement(placement: &ScreenPlacement) -> Self {
        Self {
            node: placement.node.clone(),
            x: placement.bounds.origin.x,
            y: placement.bounds.origin.y,
            width: placement.bounds.size.width,
            height: placement.bounds.size.height,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not create configuration directory `{}`: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not encode configuration: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("could not write configuration `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Topology(#[from] TopologyError),
    #[error("controller node `{0}` is missing from `role.screens`")]
    MissingLocalScreen(NodeId),
    #[error("agent node `{0}` cannot use itself as its controller")]
    LocalController(NodeId),
    #[error("edge behavior is invalid: {0}")]
    InvalidEdgeBehavior(String),
}

#[cfg(test)]
mod tests {
    use domain::Edge;
    use tempfile::TempDir;

    use super::{Config, ConfigError, EdgeBehavior, Role};

    #[test]
    fn edge_behavior_applies_ranges_and_corner_dead_zones() {
        let mut behavior = EdgeBehavior {
            corner_dead_zone_percent: 10,
            ..EdgeBehavior::default()
        };
        behavior.right.active_start_percent = 5;
        behavior.right.active_end_percent = 80;

        assert!(!behavior.allows(Edge::Right, 0.09));
        assert!(behavior.allows(Edge::Right, 0.10));
        assert!(behavior.allows(Edge::Right, 0.80));
        assert!(!behavior.allows(Edge::Right, 0.81));
        behavior.right.enabled = false;
        assert!(!behavior.allows(Edge::Right, 0.5));
    }

    #[test]
    fn parses_a_controller_topology() {
        let config = Config::parse(
            r#"
                [node]
                id = "left"

                [role]
                kind = "controller"
                listen = "0.0.0.0:24800"

                [[role.screens]]
                node = "left"
                x = 0
                y = 180
                width = 1920
                height = 1080

                [[role.screens]]
                node = "right"
                x = 1920
                y = 0
                width = 2560
                height = 1440
            "#,
        )
        .unwrap_or_else(|error| panic!("configuration should be valid: {error}"));

        assert_eq!(config.node.as_str(), "left");
        assert!(matches!(
            config.role,
            Role::Controller { topology, .. } if topology.screens().len() == 2
        ));
    }

    #[test]
    fn requires_the_controller_in_its_topology() {
        let result = Config::parse(
            r#"
                [node]
                id = "left"

                [role]
                kind = "controller"
                listen = "127.0.0.1:24800"

                [[role.screens]]
                node = "right"
                x = 1920
                y = 0
                width = 1920
                height = 1080
            "#,
        );

        assert!(matches!(result, Err(ConfigError::MissingLocalScreen(_))));
    }

    #[test]
    fn rejects_invalid_edge_behavior() {
        let result = Config::parse(
            r#"
                [node]
                id = "left"

                [role]
                kind = "controller"
                listen = "127.0.0.1:24800"

                [role.edge_behavior]
                corner_dead_zone_percent = 50

                [[role.screens]]
                node = "left"
                x = 0
                y = 0
                width = 1920
                height = 1080
            "#,
        );

        assert!(matches!(result, Err(ConfigError::InvalidEdgeBehavior(_))));
    }

    #[test]
    fn rejects_unknown_fields() {
        let result = Config::parse(
            r#"
                [node]
                id = "right"
                display_name = "Right"

                [role]
                kind = "agent"
                controller_node = "left"
                controller = "192.0.2.1:24800"
                display_width = 2560
                display_height = 1440
            "#,
        );

        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn parses_an_agent_controller_and_display() {
        let config = Config::parse(
            r#"
                [node]
                id = "right"

                [role]
                kind = "agent"
                controller_node = "left"
                controller = "192.0.2.1:24800"
                display_width = 2560
                display_height = 1440
            "#,
        )
        .unwrap_or_else(|error| panic!("configuration should be valid: {error}"));

        assert!(matches!(
            config.role,
            Role::Agent {
                controller_node,
                display_layout,
                ..
            } if controller_node.as_str() == "left"
                && display_layout.size().width.get() == 2560
                && display_layout.size().height.get() == 1440
        ));
    }

    #[test]
    fn saved_configuration_round_trips_through_validation() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let path = directory.path().join("nested").join("controller.toml");
        let config = Config::parse(
            r#"
                [node]
                id = "left"

                [role]
                kind = "controller"
                listen = "0.0.0.0:24800"

                [[role.screens]]
                node = "left"
                x = 0
                y = 0
                width = 1920
                height = 1080
            "#,
        )
        .unwrap_or_else(|error| panic!("configuration should be valid: {error}"));

        config
            .save(&path)
            .unwrap_or_else(|error| panic!("configuration save failed: {error}"));
        let loaded = Config::load(&path)
            .unwrap_or_else(|error| panic!("saved configuration should load: {error}"));

        assert_eq!(loaded.node, config.node);
        assert!(matches!(
            loaded.role,
            Role::Controller { topology, .. } if topology.screens().len() == 1
        ));
    }
}
