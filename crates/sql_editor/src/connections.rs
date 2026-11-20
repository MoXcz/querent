//! Saved connections in `<config dir>/connections.toml`.
//!
//! Passwords and service principal secrets are saved here, so the file is
//! owner-only (0600) until they can move to the OS keychain.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gpui_kit::{App, BorrowAppContext as _, Global, SharedString};
use serde::{Deserialize, Serialize};
use sql_db::{ConnectionConfig, ConnectionSettings, ConnectionStringError, Dialect};

use crate::settings;

const CONNECTIONS_FILE: &str = "connections.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    pub settings: ConnectionSettings,
}

impl SavedConnection {
    pub fn config(&self) -> Result<ConnectionConfig, ConnectionStringError> {
        self.settings.config()
    }

    pub fn dialect(&self) -> Dialect {
        self.settings.driver.dialect()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default, rename = "connection")]
    connections: Vec<StoredConnection>,
}

/// Versions before 0.0.11 saved a connection string instead of `settings`.
/// No `Debug`: `connection_string` may hold a password in plain text.
#[derive(Clone, Serialize, Deserialize)]
struct StoredConnection {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_string: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<ConnectionSettings>,
}

impl StoredConnection {
    /// An entry that cannot be converted comes back unchanged, to be written back as is.
    #[expect(
        clippy::result_large_err,
        reason = "called once per saved connection at startup"
    )]
    fn into_saved(self) -> Result<SavedConnection, Self> {
        let settings = match (&self.settings, &self.connection_string) {
            (Some(settings), _) => settings.clone(),
            (None, Some(text)) => match ConnectionSettings::from_connection_string(text) {
                Ok(settings) => settings,
                Err(error) => {
                    tracing::warn!(name = %self.name, %error, "keeping a saved connection whose string no longer parses");
                    return Err(self);
                }
            },
            (None, None) => return Err(self),
        };
        Ok(SavedConnection {
            name: self.name,
            settings,
        })
    }
}

impl From<SavedConnection> for StoredConnection {
    fn from(saved: SavedConnection) -> Self {
        Self {
            name: saved.name,
            connection_string: None,
            settings: Some(saved.settings),
        }
    }
}

pub struct ConnectionStore {
    connections: Vec<SavedConnection>,
    /// Entries this version cannot read, kept so saving does not delete them.
    unreadable: Vec<StoredConnection>,
    path: Option<PathBuf>,
}

impl Global for ConnectionStore {}

pub fn init(cx: &mut App) {
    let path = settings::config_dir(cx).map(|directory| directory.join(CONNECTIONS_FILE));
    let (connections, unreadable) = path.as_deref().map(load).unwrap_or_default();
    cx.set_global(ConnectionStore {
        connections,
        unreadable,
        path,
    });
}

pub fn all(cx: &App) -> &[SavedConnection] {
    &cx.global::<ConnectionStore>().connections
}

pub fn names(cx: &App) -> Vec<SharedString> {
    all(cx)
        .iter()
        .map(|connection| SharedString::from(connection.name.clone()))
        .collect()
}

/// Names of saved entries this version cannot read; a new connection must not reuse one.
pub fn is_unreadable_name(cx: &App, name: &str) -> bool {
    cx.global::<ConnectionStore>()
        .unreadable
        .iter()
        .any(|entry| entry.name == name)
}

pub fn find<'a>(cx: &'a App, name: &str) -> Option<&'a SavedConnection> {
    all(cx).iter().find(|connection| connection.name == name)
}

/// Adds the connection, or replaces the one named `replacing` (or with the same name).
pub fn save(cx: &mut App, connection: SavedConnection, replacing: Option<&str>) {
    cx.update_global::<ConnectionStore, _>(|store, _| {
        let existing = store.connections.iter().position(|saved| {
            Some(saved.name.as_str()) == replacing || saved.name == connection.name
        });
        match existing {
            Some(index) => {
                if let Some(slot) = store.connections.get_mut(index) {
                    *slot = connection;
                }
                // Renaming onto another connection's name leaves one of each name.
                let mut seen = std::collections::HashSet::new();
                store
                    .connections
                    .retain(|saved| seen.insert(saved.name.clone()));
            }
            None => store.connections.push(connection),
        }
    });
    persist(cx);
}

pub fn remove(cx: &mut App, name: &str) {
    cx.update_global::<ConnectionStore, _>(|store, _| {
        store.connections.retain(|saved| saved.name != name);
    });
    persist(cx);
}

fn persist(cx: &mut App) {
    let store = cx.global::<ConnectionStore>();
    let Some(path) = store.path.clone() else {
        return;
    };
    let file = ConnectionsFile {
        connections: store
            .connections
            .iter()
            .cloned()
            .map(StoredConnection::from)
            .chain(store.unreadable.iter().cloned())
            .collect(),
    };
    cx.background_executor()
        .spawn(async move {
            if let Err(error) = write(&path, &file) {
                tracing::warn!(%error, "could not save connections");
            }
        })
        .detach();
}

fn load(path: &Path) -> (Vec<SavedConnection>, Vec<StoredConnection>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<ConnectionsFile>(&text) {
            Ok(file) => split(file.connections),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring invalid connections file");
                (Vec::new(), Vec::new())
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read connections file");
            (Vec::new(), Vec::new())
        }
    }
}

fn split(stored: Vec<StoredConnection>) -> (Vec<SavedConnection>, Vec<StoredConnection>) {
    let mut saved = Vec::new();
    let mut unreadable = Vec::new();
    for entry in stored {
        match entry.into_saved() {
            Ok(connection) => saved.push(connection),
            Err(entry) => unreadable.push(entry),
        }
    }
    (saved, unreadable)
}

fn write(path: &Path, file: &ConnectionsFile) -> anyhow::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    let text = toml::to_string_pretty(file)?;
    write_owner_only(path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies when creating; tighten a file made by an older version.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use sql_db::{Authentication, DriverKind};

    use super::*;

    #[test]
    fn file_round_trips() {
        let saved = SavedConnection {
            name: "BI warehouse".to_owned(),
            settings: ConnectionSettings {
                server: "h".to_owned(),
                database: "a;b".to_owned(),
                authentication: Authentication::Password {
                    user: "sa".to_owned(),
                    password: "pw".to_owned(),
                },
                ..ConnectionSettings::new(DriverKind::SqlServer)
            },
        };
        let file = ConnectionsFile {
            connections: vec![StoredConnection::from(saved.clone())],
        };
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(text.contains("[[connection]]"));
        let parsed: ConnectionsFile = toml::from_str(&text).unwrap();
        let (loaded, unreadable) = split(parsed.connections);
        assert_eq!(loaded, vec![saved]);
        assert!(unreadable.is_empty());
    }

    #[test]
    fn reads_connection_strings_from_older_versions() {
        let text = r#"
[[connection]]
name = "Fabric"
connection_string = "Server=x.datawarehouse.fabric.microsoft.com;Database=analytics"
"#;
        let parsed: ConnectionsFile = toml::from_str(text).unwrap();
        let (saved, _) = split(parsed.connections);
        assert_eq!(saved[0].settings.driver, DriverKind::Fabric);
        assert_eq!(saved[0].settings.database, "analytics");
    }

    #[test]
    fn keeps_strings_it_cannot_read() {
        let text = r#"
[[connection]]
name = "Old"
connection_string = "Server=h;Integrated Security=SSPI"
"#;
        let parsed: ConnectionsFile = toml::from_str(text).unwrap();
        let (saved, unreadable) = split(parsed.connections);
        assert_eq!(saved, Vec::new());
        let rewritten = toml::to_string(&ConnectionsFile {
            connections: unreadable,
        })
        .unwrap();
        assert!(rewritten.contains("Integrated Security=SSPI"));
    }
}
