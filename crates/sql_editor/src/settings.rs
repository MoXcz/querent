//! User settings in `<config dir>/settings.toml`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "settings.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the theme to apply; `None` means the built-in default.
    pub theme: Option<String>,
}

impl Settings {
    /// A missing file gives defaults; an unreadable one is logged and gives defaults.
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join(SETTINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|error| {
                tracing::warn!(path = %path.display(), %error, "ignoring invalid settings file");
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot read settings file");
                Self::default()
            }
        }
    }

    pub fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let path = config_dir.join(SETTINGS_FILE);
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// `~/.config/sql-editor` on Linux; `None` when the OS reports no home directory.
pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "sql-editor")
        .map(|directories| directories.config_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let settings = Settings {
            theme: Some("Solitude".to_owned()),
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(text.trim(), r#"theme = "Solitude""#);
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }
}
