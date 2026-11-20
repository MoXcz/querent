//! Saved connections in `<config dir>/connections.toml`.
//!
//! Only Entra ID connection strings are accepted, so the file holds no secrets.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gpui_kit::{App, BorrowAppContext as _, Global, SharedString};
use serde::{Deserialize, Serialize};
use sql_db::{ConnectionStringError, FabricConfig};

use crate::settings;

const CONNECTIONS_FILE: &str = "connections.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    pub connection_string: String,
}

impl SavedConnection {
    pub fn config(&self) -> Result<FabricConfig, ConnectionStringError> {
        sql_db::parse_connection_string(&self.connection_string)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default, rename = "connection")]
    connections: Vec<SavedConnection>,
}

pub struct ConnectionStore {
    connections: Vec<SavedConnection>,
    path: Option<PathBuf>,
}

impl Global for ConnectionStore {}

pub fn init(cx: &mut App) {
    let path = settings::config_dir(cx).map(|directory| directory.join(CONNECTIONS_FILE));
    let connections = path.as_deref().map(load).unwrap_or_default();
    cx.set_global(ConnectionStore { connections, path });
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
        connections: store.connections.clone(),
    };
    cx.background_executor()
        .spawn(async move {
            if let Err(error) = write(&path, &file) {
                tracing::warn!(%error, "could not save connections");
            }
        })
        .detach();
}

fn load(path: &Path) -> Vec<SavedConnection> {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<ConnectionsFile>(&text) {
            Ok(file) => file.connections,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring invalid connections file");
                Vec::new()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read connections file");
            Vec::new()
        }
    }
}

fn write(path: &Path, file: &ConnectionsFile) -> anyhow::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    std::fs::write(path, toml::to_string_pretty(file)?)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_round_trips() {
        let file = ConnectionsFile {
            connections: vec![SavedConnection {
                name: "BI warehouse".to_owned(),
                connection_string: "Server=h;Database='a;b'".to_owned(),
            }],
        };
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(text.contains("[[connection]]"));
        let parsed: ConnectionsFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.connections, file.connections);
    }
}
