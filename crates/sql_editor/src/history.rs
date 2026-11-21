//! Every run, kept in `<config dir>/history.jsonl` (owner-only) and shown per connection.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::{App, BorrowAppContext as _, Global};
use serde::{Deserialize, Serialize};

use crate::settings;

const HISTORY_FILE: &str = "history.jsonl";
/// Entries kept in memory and, after a rewrite, on disk.
const MAX_ENTRIES: usize = 2_000;
/// Longer statements are stored cut, so one huge script cannot bloat the file.
const MAX_SQL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Seconds since the Unix epoch.
    pub at: u64,
    pub connection: String,
    #[serde(default)]
    pub database: String,
    pub sql: String,
    pub elapsed_ms: u64,
    /// Rows in the last result set, when there was one.
    #[serde(default)]
    pub rows: Option<usize>,
    pub succeeded: bool,
}

enum Write {
    Append(HistoryEntry),
    Rewrite(Vec<HistoryEntry>),
}

pub struct QueryHistory {
    entries: Vec<HistoryEntry>,
    /// Writes go to one thread in order, so a rewrite never drops a later append.
    writer: Option<std::sync::mpsc::Sender<Write>>,
}

impl Global for QueryHistory {}

pub fn init(cx: &mut App) {
    let path = settings::config_dir(cx).map(|directory| directory.join(HISTORY_FILE));
    let (entries, overgrown) = path.as_deref().map(load).unwrap_or_default();
    let writer = path.and_then(spawn_writer);
    // Appends alone never shrink the file; without this it grows across sessions.
    if overgrown
        && let Some(writer) = &writer
        && writer.send(Write::Rewrite(entries.clone())).is_err()
    {
        tracing::warn!("query history writer stopped");
    }
    cx.set_global(QueryHistory { entries, writer });
}

/// Newest first, for one connection.
pub fn for_connection(cx: &App, connection: &str) -> Vec<HistoryEntry> {
    cx.global::<QueryHistory>()
        .entries
        .iter()
        .rev()
        .filter(|entry| entry.connection == connection)
        .cloned()
        .collect()
}

pub fn record(cx: &mut App, mut entry: HistoryEntry) {
    if entry.sql.trim().is_empty() {
        return;
    }
    if entry.sql.len() > MAX_SQL_BYTES {
        let mut end = MAX_SQL_BYTES;
        while !entry.sql.is_char_boundary(end) {
            end -= 1;
        }
        entry.sql.truncate(end);
    }
    cx.update_global::<QueryHistory, _>(|history, _| {
        history.entries.push(entry.clone());
        let write = if history.entries.len() > MAX_ENTRIES * 2 {
            let excess = history.entries.len() - MAX_ENTRIES;
            history.entries.drain(..excess);
            Write::Rewrite(history.entries.clone())
        } else {
            Write::Append(entry)
        };
        if let Some(writer) = &history.writer
            && writer.send(write).is_err()
        {
            tracing::warn!("query history writer stopped");
        }
    });
}

/// A thread that writes history jobs in order; it ends when the app drops the sender.
fn spawn_writer(path: PathBuf) -> Option<std::sync::mpsc::Sender<Write>> {
    let (sender, jobs) = std::sync::mpsc::channel::<Write>();
    let spawned = std::thread::Builder::new()
        .name("query-history".to_owned())
        .spawn(move || {
            for job in jobs {
                let result = match job {
                    Write::Append(entry) => append(&path, &entry),
                    Write::Rewrite(entries) => rewrite_file(&path, &entries),
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "could not save query history");
                }
            }
        });
    match spawned {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::warn!(%error, "query history will not be saved");
            None
        }
    }
}

#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `3s ago`, `5m ago`, `2h ago`, `4d ago`.
#[must_use]
pub fn age(at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(at);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The newest [`MAX_ENTRIES`] runs, and whether the file holds more than that.
fn load(path: &Path) -> (Vec<HistoryEntry>, bool) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), false),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read query history");
            return (Vec::new(), false);
        }
    };
    // A line cut by a crash is skipped, not fatal.
    let mut entries: Vec<HistoryEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    (entries, excess > 0)
}

fn append(path: &Path, entry: &HistoryEntry) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let line = serde_json::to_string(entry).map_err(std::io::Error::other)? + "\n";
    let _guard = crate::connections::FILE_WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // One write, so a line never splits around another writer's.
    file.write_all(line.as_bytes())
}

fn rewrite_file(path: &Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    crate::connections::replace_owner_only(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ages() {
        assert_eq!(age(100, 103), "3s ago");
        assert_eq!(age(0, 300), "5m ago");
        assert_eq!(age(0, 7_200), "2h ago");
        assert_eq!(age(0, 3 * 86_400), "3d ago");
    }

    #[test]
    fn skips_cut_lines() {
        let path =
            std::env::temp_dir().join(format!("sql-editor-history-{}.jsonl", std::process::id()));
        let entry = HistoryEntry {
            at: 1,
            connection: "c".to_owned(),
            database: "d".to_owned(),
            sql: "select 1".to_owned(),
            elapsed_ms: 5,
            rows: Some(1),
            succeeded: true,
        };
        append(&path, &entry).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"at\":2,\"conn")
            .unwrap();
        let (loaded, overgrown) = load(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded, vec![entry]);
        assert!(!overgrown);
    }

    #[test]
    fn an_overgrown_file_is_reported_for_trimming() {
        let path = std::env::temp_dir().join(format!(
            "sql-editor-history-long-{}.jsonl",
            std::process::id()
        ));
        let entries: Vec<HistoryEntry> = (0..MAX_ENTRIES as u64 + 500)
            .map(|at| HistoryEntry {
                at,
                connection: "c".to_owned(),
                database: String::new(),
                sql: "select 1".to_owned(),
                elapsed_ms: 1,
                rows: None,
                succeeded: true,
            })
            .collect();
        rewrite_file(&path, &entries).unwrap();
        let (loaded, overgrown) = load(&path);
        assert!(overgrown);
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded.first().map(|entry| entry.at), Some(500));
        rewrite_file(&path, &loaded).unwrap();
        let (_, overgrown) = load(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(!overgrown);
    }
}
