use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use domain::NodeId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SETTINGS_FILE: &str = "desktop.toml";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopSettings {
    pub node: Option<NodeId>,
}

impl DesktopSettings {
    pub fn load(root: &Path) -> Result<Self, SettingsError> {
        let path = root.join(SETTINGS_FILE);
        match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).map_err(SettingsError::Parse),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SettingsError::Read { path, source }),
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), SettingsError> {
        fs::create_dir_all(root).map_err(|source| SettingsError::CreateDirectory {
            path: root.to_path_buf(),
            source,
        })?;
        let path = root.join(SETTINGS_FILE);
        let contents = toml::to_string_pretty(self).map_err(SettingsError::Encode)?;
        fs::write(&path, contents).map_err(|source| SettingsError::Write { path, source })
    }
}

pub fn default_data_directory() -> Result<PathBuf, SettingsError> {
    ProjectDirs::from("", "", "Tevir")
        .map(|directories| directories.data_local_dir().to_path_buf())
        .ok_or(SettingsError::DataDirectoryUnavailable)
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("the platform did not provide an application data directory")]
    DataDirectoryUnavailable,
    #[error("could not create settings directory `{}`: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read settings `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse desktop settings: {0}")]
    Parse(toml::de::Error),
    #[error("could not encode desktop settings: {0}")]
    Encode(toml::ser::Error),
    #[error("could not write settings `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use domain::NodeId;
    use tempfile::TempDir;

    use super::DesktopSettings;

    #[test]
    fn settings_round_trip_the_selected_node() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let expected = DesktopSettings {
            node: Some(
                NodeId::new("studio-left")
                    .unwrap_or_else(|error| panic!("test node should be valid: {error}")),
            ),
        };
        expected
            .save(directory.path())
            .unwrap_or_else(|error| panic!("settings save failed: {error}"));
        let loaded = DesktopSettings::load(directory.path())
            .unwrap_or_else(|error| panic!("settings load failed: {error}"));

        assert_eq!(loaded.node, expected.node);
    }
}
