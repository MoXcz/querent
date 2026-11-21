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
    /// The settings, and whether they may be saved. A missing file gives
    /// defaults. A file that does not parse is set aside as `settings.toml.bad`;
    /// one that cannot be read or set aside is never written over.
    fn load(config_dir: &Path) -> (Self, bool) {
        let path = config_dir.join(SETTINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(settings) => (settings, true),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "invalid settings file");
                    (Self::default(), crate::connections::set_aside(&path))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Self::default(), true),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot read settings file; settings will not be saved");
                (Self::default(), false)
            }
        }
    }

    fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let path = config_dir.join(SETTINGS_FILE);
        crate::connections::replace_owner_only(&path, toml::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("writing {}", path.display()))
    }
}

pub struct SettingsStore {
    settings: Settings,
    config_dir: Option<PathBuf>,
    /// Saves go to one thread in order, so an older save never lands last.
    writer: Option<std::sync::mpsc::Sender<Settings>>,
}

impl Global for SettingsStore {}

pub fn init(cx: &mut App) {
    let config_dir = directories::ProjectDirs::from("", "", "sql-editor")
        .map(|directories| directories.config_dir().to_path_buf());
    let (settings, may_save) = config_dir
        .as_deref()
        .map_or_else(|| (Settings::default(), false), Settings::load);
    let writer = config_dir
        .clone()
        .filter(|_| may_save)
        .and_then(spawn_writer);
    cx.set_global(SettingsStore {
        settings,
        config_dir,
        writer,
    });
}

fn spawn_writer(config_dir: PathBuf) -> Option<std::sync::mpsc::Sender<Settings>> {
    let (sender, jobs) = std::sync::mpsc::channel::<Settings>();
    let spawned = std::thread::Builder::new()
        .name("settings".to_owned())
        .spawn(move || {
            for settings in jobs {
                if let Err(error) = settings.save(&config_dir) {
                    tracing::warn!(%error, "could not save settings");
                }
            }
        });
    match spawned {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::warn!(%error, "settings will not be saved");
            None
        }
    }
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
    if let Some(writer) = &store.writer
        && writer.send(store.settings.clone()).is_err()
    {
        tracing::warn!("settings writer stopped");
    }
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
    fn a_file_that_does_not_parse_is_set_aside() {
        let directory =
            std::env::temp_dir().join(format!("sql-editor-settings-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(SETTINGS_FILE), "row_limit = 7\n[").unwrap();
        let (settings, may_save) = Settings::load(&directory);
        assert_eq!(settings, Settings::default());
        assert!(may_save);
        settings.save(&directory).unwrap();
        let kept = std::fs::read_to_string(directory.join("settings.toml.bad")).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(kept.contains("row_limit = 7"));
    }

    #[test]
    fn missing_keys_use_defaults() {
        let settings = toml::from_str::<Settings>(r#"theme = "Solitude""#).unwrap();
        assert!(settings.vim_mode);
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }
}
