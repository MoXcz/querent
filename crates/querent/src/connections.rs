//! Saved connections in `<config dir>/connections.toml`; secrets go to [`crate::secrets`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gpui_kit::{App, BorrowAppContext as _, Global, SharedString};
use serde::{Deserialize, Serialize};
use sql_db::{ConnectionConfig, ConnectionSettings, ConnectionStringError, Dialect};

use crate::{secrets, settings};

const CONNECTIONS_FILE: &str = "connections.toml";

/// How careful to be on a connection; tabs on a production one say so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    #[default]
    Normal,
    Production,
}

impl Environment {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    fn is_normal(&self) -> bool {
        *self == Self::Normal
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    #[serde(default, skip_serializing_if = "Environment::is_normal")]
    pub environment: Environment,
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
    #[serde(default, skip_serializing_if = "Environment::is_normal")]
    environment: Environment,
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
            environment: self.environment,
            settings,
        })
    }
}

impl From<SavedConnection> for StoredConnection {
    fn from(saved: SavedConnection) -> Self {
        Self {
            name: saved.name,
            environment: saved.environment,
            connection_string: None,
            settings: Some(saved.settings),
        }
    }
}

pub struct ConnectionStore {
    connections: Vec<SavedConnection>,
    /// Entries this version cannot read, kept so saving does not delete them.
    unreadable: Vec<StoredConnection>,
    /// Each connection's secret as last sent to the secret store, so a save
    /// only writes the ones that changed.
    stored_secrets: HashMap<String, String>,
    /// Saves go to one thread in order, so an older save never lands last.
    writer: Option<std::sync::mpsc::Sender<Persist>>,
}

impl Global for ConnectionStore {}

struct Persist {
    /// Secrets to store (`Some`) or delete (`None`), by connection name.
    secret_changes: Vec<(String, Option<String>)>,
    file: ConnectionsFile,
}

fn secret_key(name: &str) -> String {
    format!("connection:{name}")
}

pub fn init(cx: &mut App) {
    let path = settings::config_dir(cx).map(|directory| directory.join(CONNECTIONS_FILE));
    let loaded = path.as_deref().map(load);
    // A file that could not be read is left alone: saving over it would delete its connections.
    let path = path.filter(|_| loaded.as_ref().is_some_and(Option::is_some));
    let (mut connections, unreadable) = loaded.flatten().unwrap_or_default();
    // Secrets written inline by older versions move out on the first save.
    let mut migrate = false;
    let mut stored_secrets = HashMap::new();
    for connection in &mut connections {
        let Some(secret) = connection.settings.authentication.secret_mut() else {
            continue;
        };
        if secret.is_empty() {
            if let Some(stored) = secrets::get(&secret_key(&connection.name)) {
                stored_secrets.insert(connection.name.clone(), stored.clone());
                *secret = stored;
            }
        } else {
            migrate = true;
        }
    }
    let writer = path.and_then(spawn_writer);
    cx.set_global(ConnectionStore {
        connections,
        unreadable,
        stored_secrets,
        writer,
    });
    if migrate {
        persist(cx);
    }
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
    cx.update_global::<ConnectionStore, _>(|store, _| {
        let mut current = HashMap::new();
        for connection in &store.connections {
            if let Some(secret) = connection
                .settings
                .authentication
                .secret()
                .filter(|secret| !secret.is_empty())
            {
                current.insert(connection.name.clone(), secret.to_owned());
            }
        }
        let mut secret_changes: Vec<(String, Option<String>)> = current
            .iter()
            .filter(|(name, secret)| store.stored_secrets.get(*name) != Some(*secret))
            .map(|(name, secret)| (name.clone(), Some(secret.clone())))
            .collect();
        secret_changes.extend(
            store
                .stored_secrets
                .keys()
                .filter(|name| !current.contains_key(*name))
                .map(|name| (name.clone(), None)),
        );
        store.stored_secrets = current;
        let file = ConnectionsFile {
            connections: store
                .connections
                .iter()
                .cloned()
                .map(StoredConnection::from)
                .chain(store.unreadable.iter().cloned())
                .collect(),
        };
        if let Some(writer) = &store.writer
            && writer
                .send(Persist {
                    secret_changes,
                    file,
                })
                .is_err()
        {
            tracing::warn!("connections writer stopped");
        }
    });
}

fn spawn_writer(path: PathBuf) -> Option<std::sync::mpsc::Sender<Persist>> {
    let (sender, jobs) = std::sync::mpsc::channel::<Persist>();
    let spawned = std::thread::Builder::new()
        .name("connections".to_owned())
        .spawn(move || {
            // Names whose secret could not be stored elsewhere stay in the file.
            let mut kept_inline = std::collections::HashSet::new();
            for mut job in jobs {
                for (name, secret) in &job.secret_changes {
                    match secrets::set(&secret_key(name), secret.as_deref()) {
                        Ok(()) => {
                            kept_inline.remove(name);
                        }
                        Err(error) => {
                            tracing::warn!(%error, connection = %name, "keeping a secret in connections.toml");
                            kept_inline.insert(name.clone());
                        }
                    }
                }
                for entry in &mut job.file.connections {
                    if kept_inline.contains(&entry.name) {
                        continue;
                    }
                    if let Some(secret) = entry
                        .settings
                        .as_mut()
                        .and_then(|settings| settings.authentication.secret_mut())
                    {
                        secret.clear();
                    }
                }
                if let Err(error) = write(&path, &job.file) {
                    tracing::warn!(%error, "could not save connections");
                }
            }
        });
    match spawned {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::warn!(%error, "connections will not be saved");
            None
        }
    }
}

/// `None` when the file exists but cannot be read, so nothing may be saved over it.
/// A file that does not parse is set aside as `connections.toml.bad` first.
fn load(path: &Path) -> Option<(Vec<SavedConnection>, Vec<StoredConnection>)> {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<ConnectionsFile>(&text) {
            Ok(file) => Some(split(file.connections)),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "invalid connections file");
                set_aside(path).then(Default::default)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Default::default()),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read connections file; connections will not be saved");
            None
        }
    }
}

/// Renames a file this version cannot parse to an unused `<name>.bad` (then
/// `.bad.1`, ...), so neither the next save nor a later bad file destroys it.
pub(crate) fn set_aside(path: &Path) -> bool {
    let aside = (0..100).find_map(|attempt| {
        let mut name = path.as_os_str().to_owned();
        name.push(if attempt == 0 {
            ".bad".to_owned()
        } else {
            format!(".bad.{attempt}")
        });
        let candidate = PathBuf::from(name);
        matches!(candidate.try_exists(), Ok(false)).then_some(candidate)
    });
    let Some(aside) = aside else {
        tracing::warn!(path = %path.display(), "too many set-aside copies; the invalid file will not be overwritten");
        return false;
    };
    match std::fs::rename(path, &aside) {
        Ok(()) => {
            tracing::warn!(kept = %aside.display(), "kept the invalid file under a new name");
            true
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not set the invalid file aside; it will not be overwritten");
            false
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
    replace_owner_only(path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

/// Serializes this app's writes to its own files, so concurrent saves cannot interleave.
pub(crate) static FILE_WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Writes owner-only to a temporary file, then renames it over `path`, so a
/// crash mid-write leaves the old file whole.
pub(crate) fn replace_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let _guard = FILE_WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    write_owner_only(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

#[cfg(unix)]
pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
    file.write_all(bytes)?;
    // Without this a power loss after the rename can leave an empty file.
    file.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
            environment: Environment::Production,
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
        assert!(text.contains("environment = \"production\""));
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
        assert_eq!(saved[0].environment, Environment::Normal);
        assert_eq!(saved[0].settings.database, "analytics");
    }

    #[test]
    fn a_file_that_does_not_parse_is_set_aside_not_overwritten() {
        let directory =
            std::env::temp_dir().join(format!("querent-connections-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CONNECTIONS_FILE);
        std::fs::write(&path, "[[connection]]\nname = \"Fabric\"\n[").unwrap();
        let (saved, unreadable) = load(&path).unwrap();
        assert!(saved.is_empty() && unreadable.is_empty());
        write(&path, &ConnectionsFile::default()).unwrap();
        // A second bad file must not replace the first copy.
        std::fs::write(&path, "second [").unwrap();
        assert!(load(&path).is_some());
        let kept = std::fs::read_to_string(directory.join("connections.toml.bad")).unwrap();
        let second = std::fs::read_to_string(directory.join("connections.toml.bad.1")).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(kept.contains("Fabric"));
        assert!(second.contains("second"));
    }

    #[test]
    fn keeps_strings_it_cannot_read() {
        let text = r#"
[[connection]]
name = "Old"
connection_string = "Server=h;Authentication=ActiveDirectoryPassword"
"#;
        let parsed: ConnectionsFile = toml::from_str(text).unwrap();
        let (saved, unreadable) = split(parsed.connections);
        assert_eq!(saved, Vec::new());
        let rewritten = toml::to_string(&ConnectionsFile {
            connections: unreadable,
        })
        .unwrap();
        assert!(rewritten.contains("Authentication=ActiveDirectoryPassword"));
    }
}
