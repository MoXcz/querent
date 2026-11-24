//! Open tabs and their text in `<config dir>/session.json`, restored at start.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui_kit::App;
use serde::{Deserialize, Serialize};

use crate::connections::replace_owner_only;
use crate::settings;

const SESSION_FILE: &str = "session.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
    /// Folders open in the sidebar, in the order they were opened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<SavedFolder>,
    #[serde(default)]
    pub sidebar: SidebarView,
    /// The Querent version that wrote the session; a newer one shows its release notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_version: Option<String>,
    /// Per dbt project, the database previews read instead of the manifest's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dbt_databases: BTreeMap<PathBuf, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iceberg_catalogs: Vec<SavedCatalog>,
}

/// An Iceberg REST catalog in the sidebar; its credential is in the keychain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCatalog {
    pub name: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub warehouse: String,
}

/// What the sidebar shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarView {
    Files,
    Dbt,
    Iceberg,
    #[default]
    Database,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFolder {
    pub path: PathBuf,
    /// The connection last used for a file in this folder; new tabs from it start there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub connection: Option<String>,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub sql: String,
    /// The `.sql` file the tab was opened from or saved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// `sql` differs from what `file` held when the tab last read or wrote it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub modified: bool,
}

fn path(cx: &App) -> Option<PathBuf> {
    settings::config_dir(cx).map(|directory| directory.join(SESSION_FILE))
}

/// The last session, or `None` for a first start or an unreadable file.
pub fn load(cx: &App) -> Option<Session> {
    let path = path(cx)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read session file");
            return None;
        }
    };
    match serde_json::from_str::<Session>(&text) {
        Ok(session) if !session.tabs.is_empty() => Some(session),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring invalid session file");
            None
        }
    }
}

/// Writes in the background; owner-only, since queries can hold sensitive values.
pub fn save(session: &Session, cx: &App) {
    let Some(path) = path(cx) else {
        return;
    };
    let Ok(text) = serde_json::to_string_pretty(session) else {
        return;
    };
    cx.background_executor()
        .spawn(async move {
            if let Err(error) = write(&path, &text) {
                tracing::warn!(%error, "could not save the session");
            }
        })
        .detach();
}

/// Writes now, for app quit, when a background write might not finish.
pub fn save_now(session: &Session, cx: &App) {
    let (Some(path), Ok(text)) = (path(cx), serde_json::to_string_pretty(session)) else {
        return;
    };
    if let Err(error) = write(&path, &text) {
        tracing::warn!(%error, "could not save the session");
    }
}

fn write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    replace_owner_only(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let session = Session {
            tabs: vec![SavedTab {
                connection: Some("Fabric".to_owned()),
                database: "analytics".to_owned(),
                sql: "select 1;\n".to_owned(),
                file: Some(PathBuf::from("/tmp/report.sql")),
                modified: true,
            }],
            active: 0,
            folders: vec![SavedFolder {
                path: PathBuf::from("/tmp/reports"),
                connection: Some("Fabric".to_owned()),
                database: "analytics".to_owned(),
            }],
            sidebar: SidebarView::Iceberg,
            last_version: Some("0.3.0".to_owned()),
            dbt_databases: BTreeMap::from([(
                PathBuf::from("/tmp/reports/dbt"),
                "analytics_dev".to_owned(),
            )]),
            iceberg_catalogs: vec![SavedCatalog {
                name: "lake".to_owned(),
                uri: "http://localhost:8181".to_owned(),
                warehouse: String::new(),
            }],
        };
        let text = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&text).unwrap(), session);
        let older = r#"{"tabs":[{"connection":null}],"active":0}"#;
        assert_eq!(
            serde_json::from_str::<Session>(older).unwrap().tabs[0].sql,
            ""
        );
        assert_eq!(
            serde_json::from_str::<Session>(older).unwrap().sidebar,
            SidebarView::Database
        );
    }
}
