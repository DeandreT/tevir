use std::{
    fs,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use domain::{NodeId, Point, Rect, ScreenPlacement, Size, Topology, TopologyError};
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
    },
    Agent {
        controller: SocketAddr,
    },
}

impl Config {
    pub fn new(node: NodeId, role: Role) -> Result<Self, ConfigError> {
        if let Role::Controller { topology, .. } = &role
            && topology.screen(&node).is_none()
        {
            return Err(ConfigError::MissingLocalScreen(node));
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    node: Node,
    role: RoleFile,
}

impl ConfigFile {
    fn validate(self) -> Result<Config, ConfigError> {
        let role = match self.role {
            RoleFile::Controller { listen, screens } => {
                let placements = screens.into_iter().map(Screen::into_placement).collect();
                Role::Controller {
                    listen,
                    topology: Topology::new(placements)?,
                }
            }
            RoleFile::Agent { controller } => Role::Agent { controller },
        };

        Config::new(self.node.id, role)
    }
}

impl From<&Config> for ConfigFile {
    fn from(config: &Config) -> Self {
        let role = match &config.role {
            Role::Controller { listen, topology } => RoleFile::Controller {
                listen: *listen,
                screens: topology
                    .screens()
                    .iter()
                    .map(Screen::from_placement)
                    .collect(),
            },
            Role::Agent { controller } => RoleFile::Agent {
                controller: *controller,
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
    },
    Agent {
        controller: SocketAddr,
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
        ScreenPlacement {
            node: self.node,
            bounds: Rect::new(
                Point {
                    x: self.x,
                    y: self.y,
                },
                Size::new(self.width, self.height),
            ),
        }
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
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{Config, ConfigError, Role};

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
                y = 0
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
                x = 0
                y = 0
                width = 1920
                height = 1080
            "#,
        );

        assert!(matches!(result, Err(ConfigError::MissingLocalScreen(_))));
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
                controller = "192.0.2.1:24800"
            "#,
        );

        assert!(matches!(result, Err(ConfigError::Parse(_))));
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
