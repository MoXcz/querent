//! User settings in `<config dir>/settings.toml`, loaded once at startup.

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use anyhow::Context as _;
use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};
use sql_db::DriverKind;
use sql_text::format::FormatStyle;

use crate::lsp_servers::LanguageServerSetting;

const SETTINGS_FILE: &str = "settings.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the theme to apply; `None` means the built-in default.
    pub theme: Option<String>,
    pub vim_mode: bool,
    /// Object explorer shows schema folders, or `schema.name` in one list.
    pub explorer_group_by_schema: bool,
    /// Optional language server per database kind, e.g.
    /// `[language_servers.postgres]` with `command = "sqls"`.
    pub language_servers: BTreeMap<DriverKind, LanguageServerSetting>,
    /// Built-in formatter style (`[formatter]`).
    pub formatter: FormatStyle,
    /// Format with the connection's language server instead of the built-in formatter.
    pub format_with_language_server: bool,
    /// Rows kept per result set; 0 keeps every row.
    pub row_limit: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: None,
            vim_mode: true,
            explorer_group_by_schema: true,
            language_servers: BTreeMap::new(),
            formatter: FormatStyle::default(),
            format_with_language_server: false,
            row_limit: 50_000,
        }
    }
}

impl Settings {
    /// A missing file gives defaults; an unreadable one is logged and gives defaults.
    fn load(config_dir: &Path) -> Self {
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

    fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let path = config_dir.join(SETTINGS_FILE);
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

pub struct SettingsStore {
    settings: Settings,
    config_dir: Option<PathBuf>,
}

impl Global for SettingsStore {}

pub fn init(cx: &mut App) {
    let config_dir = directories::ProjectDirs::from("", "", "sql-editor")
        .map(|directories| directories.config_dir().to_path_buf());
    let settings = config_dir
        .as_deref()
        .map(Settings::load)
        .unwrap_or_default();
    cx.set_global(SettingsStore {
        settings,
        config_dir,
    });
}

pub fn get(cx: &App) -> &Settings {
    &cx.global::<SettingsStore>().settings
}

/// `~/.config/sql-editor` on Linux; `None` when the OS reports no home directory.
pub fn config_dir(cx: &App) -> Option<&Path> {
    cx.global::<SettingsStore>().config_dir.as_deref()
}

/// Changes the settings in memory and saves them in the background.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    let store = cx.global_mut::<SettingsStore>();
    change(&mut store.settings);
    let Some(config_dir) = store.config_dir.clone() else {
        return;
    };
    let settings = store.settings.clone();
    cx.background_executor()
        .spawn(async move {
            if let Err(error) = settings.save(&config_dir) {
                tracing::warn!(%error, "could not save settings");
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use sql_text::format::KeywordCase;

    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let settings = Settings {
            theme: Some("Solitude".to_owned()),
            vim_mode: false,
            explorer_group_by_schema: false,
            language_servers: BTreeMap::from([(
                DriverKind::Postgres,
                LanguageServerSetting {
                    command: "sqls".to_owned(),
                    args: Vec::new(),
                    pass_connection: true,
                },
            )]),
            formatter: FormatStyle {
                indent: 2,
                keyword_case: KeywordCase::Lower,
                lines_between_statements: 2,
            },
            format_with_language_server: true,
            row_limit: 1_000,
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn missing_keys_use_defaults() {
        let settings = toml::from_str::<Settings>(r#"theme = "Solitude""#).unwrap();
        assert!(settings.vim_mode);
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }
}
