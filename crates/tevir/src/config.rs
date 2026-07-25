use std::{
    fs,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use domain::{NodeId, Point, Rect, ScreenPlacement, Size, Topology, TopologyError};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug)]
pub struct Config {
    pub node: NodeId,
    pub role: Role,
}

#[derive(Debug)]
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    node: Node,
    role: RoleFile,
}

impl ConfigFile {
    fn validate(self) -> Result<Config, ConfigError> {
        let role = match self.role {
            RoleFile::Controller { listen, screens } => {
                if !screens.iter().any(|screen| screen.node == self.node.id) {
                    return Err(ConfigError::MissingLocalScreen(self.node.id));
                }

                let placements = screens.into_iter().map(Screen::into_placement).collect();
                Role::Controller {
                    listen,
                    topology: Topology::new(placements)?,
                }
            }
            RoleFile::Agent { controller } => Role::Agent { controller },
        };

        Ok(Config {
            node: self.node.id,
            role,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    id: NodeId,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error(transparent)]
    Topology(#[from] TopologyError),
    #[error("controller node `{0}` is missing from `role.screens`")]
    MissingLocalScreen(NodeId),
}

#[cfg(test)]
mod tests {
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
}
