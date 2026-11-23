//! Listing, filtering, renaming and searching files under the open folders; no UI.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::WalkBuilder;

/// Files the tree's filter walks before it stops and says so.
pub const MAX_WALKED_FILES: usize = 50_000;
/// Matches a search collects before it stops and says so.
pub const MAX_SEARCH_MATCHES: usize = 2_000;
/// A larger file is not a script, so the search skips it.
const MAX_SEARCHED_BYTES: u64 = 5 * 1024 * 1024;
/// A matched line is cut to this many characters in the results.
const MAX_LINE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// Ignore files apply outside a git repository too: an opened folder is often a copy of one.
fn walker(root: &Path, show_all: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(!show_all)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

/// A directory's entries, folders first, each group by name ignoring case.
pub fn list_directory(directory: &Path, show_all: bool) -> std::io::Result<Vec<Entry>> {
    // The walker skips an unreadable root silently; this reports it.
    std::fs::read_dir(directory)?;
    let mut entries: Vec<Entry> = walker(directory, show_all)
        .max_depth(Some(1))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .map(|entry| Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: entry.file_type().is_some_and(|kind| kind.is_dir()),
            path: entry.into_path(),
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

/// Every file under `root`, and whether the walk stopped at `limit`.
pub fn all_files(root: &Path, show_all: bool, limit: usize) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    for entry in walker(root, show_all).build().filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if files.len() == limit {
            return (files, true);
        }
        files.push(entry.into_path());
    }
    (files, false)
}

/// `path` relative to `root` with `/` between parts, or the whole path when outside it.
pub fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One line of the tree while it is filtered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredRow {
    pub path: PathBuf,
    /// Levels below the root, starting at 1.
    pub depth: usize,
    pub is_dir: bool,
}

/// Files whose relative path contains `filter` (lowercase), each under the folders leading to it.
pub fn filter_tree(root: &Path, files: &[PathBuf], filter: &str) -> Vec<FilteredRow> {
    let mut matching: Vec<&PathBuf> = files
        .iter()
        .filter(|path| {
            path.starts_with(root) && relative_path(root, path).to_lowercase().contains(filter)
        })
        .collect();
    matching.sort_by_key(|path| {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
    });
    let mut rows = Vec::new();
    let mut open_folders: Vec<PathBuf> = Vec::new();
    for path in matching {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let parts: Vec<Component> = relative.components().collect();
        let mut folder = root.to_path_buf();
        for (index, part) in parts.iter().enumerate() {
            folder.push(part);
            let is_file = index + 1 == parts.len();
            if is_file {
                rows.push(FilteredRow {
                    path: folder.clone(),
                    depth: index + 1,
                    is_dir: false,
                });
                break;
            }
            if open_folders.get(index) != Some(&folder) {
                open_folders.truncate(index);
                open_folders.push(folder.clone());
                rows.push(FilteredRow {
                    path: folder.clone(),
                    depth: index + 1,
                    is_dir: true,
                });
            }
        }
    }
    rows
}

/// Where `path` ends up when `from` is renamed to `to`: itself or anything under it.
pub fn renamed_path(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(from).ok()?;
    Some(if rest.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(rest)
    })
}

/// A single file or folder name typed by the user.
pub fn check_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Type a name.");
    }
    let mut parts = Path::new(name).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    );
    // `:` would name a drive or a stream on Windows and leave the folder.
    if !single || name.contains(['/', '\\', ':']) {
        return Err("A name cannot be . or .. or contain /, \\ or :.");
    }
    Ok(name)
}

/// A new file's name, with `.sql` added when it has no extension.
pub fn new_file_name(name: &str) -> String {
    // A dotfile such as `.env` has no extension but needs none.
    if name.starts_with('.') || Path::new(name).extension().is_some() {
        name.to_owned()
    } else {
        format!("{name}.sql")
    }
}

/// The open folder that holds `path`; the deepest one when folders nest.
pub fn containing_folder<'a>(
    folders: impl IntoIterator<Item = &'a Path>,
    path: &Path,
) -> Option<&'a Path> {
    folders
        .into_iter()
        .filter(|folder| path.starts_with(folder))
        .max_by_key(|folder| folder.components().count())
}

/// A change the file tree makes on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOperation {
    CreateFile(PathBuf),
    CreateFolder(PathBuf),
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Moves the entry to the trash, so it can be brought back.
    Delete(PathBuf),
}

impl FileOperation {
    /// Never replaces an existing entry; the message is for the user.
    pub fn apply(&self) -> Result<(), String> {
        let failed = |path: &Path, error: &dyn std::fmt::Display| {
            format!("Could not change {}: {error}.", path.display())
        };
        let refuse_existing = |path: &Path| {
            if path.symlink_metadata().is_ok() {
                Err(format!("{} already exists.", path.display()))
            } else {
                Ok(())
            }
        };
        match self {
            Self::CreateFile(path) => std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(drop)
                .map_err(|error| failed(path, &error)),
            Self::CreateFolder(path) => {
                std::fs::create_dir(path).map_err(|error| failed(path, &error))
            }
            Self::Rename { from, to } => {
                if from == to {
                    return Ok(());
                }
                // `rename` replaces an existing file on Unix; on a case-insensitive
                // disk a case-only rename finds the source itself, which is fine.
                if !is_same_entry(from, to) {
                    refuse_existing(to)?;
                }
                std::fs::rename(from, to).map_err(|error| failed(from, &error))
            }
            Self::Delete(path) => trash::delete(path).map_err(|error| failed(path, &error)),
        }
    }
}

fn is_same_entry(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        (Err(_), _) | (_, Err(_)) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: PathBuf,
    /// 1-based.
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, Default)]
pub struct SearchResults {
    pub matches: Vec<SearchMatch>,
    /// The search stopped at `MAX_SEARCH_MATCHES`.
    pub truncated: bool,
    pub files_searched: usize,
}

/// Matching lines, ignoring case; checks `cancelled` between files.
pub fn search(
    roots: &[PathBuf],
    pattern: &str,
    is_regex: bool,
    show_all: bool,
    cancelled: &AtomicBool,
) -> Result<SearchResults, regex::Error> {
    let source = if is_regex {
        pattern.to_owned()
    } else {
        regex::escape(pattern)
    };
    let matcher = regex::RegexBuilder::new(&source)
        .case_insensitive(true)
        .build()?;
    let mut results = SearchResults::default();
    for root in roots {
        for entry in walker(root, show_all).build().filter_map(Result::ok) {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(results);
            }
            let is_small_file = entry.file_type().is_some_and(|kind| kind.is_file())
                && entry
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() <= MAX_SEARCHED_BYTES);
            if !is_small_file {
                continue;
            }
            let Some(text) = read_text(entry.path()) else {
                continue;
            };
            results.files_searched += 1;
            for (index, line) in text.lines().enumerate() {
                if !matcher.is_match(line) {
                    continue;
                }
                if results.matches.len() == MAX_SEARCH_MATCHES {
                    results.truncated = true;
                    return Ok(results);
                }
                results.matches.push(SearchMatch {
                    path: entry.path().to_path_buf(),
                    line_number: index + 1,
                    line: line.trim().chars().take(MAX_LINE_CHARS).collect(),
                });
            }
        }
    }
    Ok(results)
}

/// A file's text, or `None` for binary files and files that cannot be read.
fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let head = bytes.get(..8192).unwrap_or(&bytes);
    // UTF-16 text has NUL bytes too; its byte-order mark says so.
    let utf16 = head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]);
    if !utf16 && head.contains(&0) {
        return None;
    }
    sql_text::decode_file(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFolder(PathBuf);

    impl TempFolder {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("querent-{name}-{}", std::process::id()));
            if path.exists() {
                std::fs::remove_dir_all(&path).unwrap();
            }
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for TempFolder {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn lists_folders_first_and_skips_ignored_and_hidden() {
        let folder = TempFolder::new("list");
        folder.write(".gitignore", "target/\n");
        folder.write("b.sql", "");
        folder.write("A.sql", "");
        folder.write("models/x.sql", "");
        folder.write("target/compiled.sql", "");
        folder.write(".git/HEAD", "");

        let names = |show_all| {
            list_directory(&folder.0, show_all)
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(false), ["models", "A.sql", "b.sql"]);
        assert_eq!(
            names(true),
            ["models", "target", ".gitignore", "A.sql", "b.sql"]
        );
        assert!(list_directory(&folder.0.join("missing"), false).is_err());
    }

    #[test]
    fn filtered_tree_keeps_the_folders_of_each_match() {
        let root = Path::new("/r");
        let files: Vec<PathBuf> = [
            "/r/marts/fin/cierre.sql",
            "/r/marts/fin/cartera.sql",
            "/r/marts/ventas.sql",
            "/r/top.sql",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let rows: Vec<(String, usize, bool)> = filter_tree(root, &files, "fin/c")
            .into_iter()
            .map(|row| (relative_path(root, &row.path), row.depth, row.is_dir))
            .collect();
        assert_eq!(
            rows,
            [
                ("marts".to_owned(), 1, true),
                ("marts/fin".to_owned(), 2, true),
                ("marts/fin/cartera.sql".to_owned(), 3, false),
                ("marts/fin/cierre.sql".to_owned(), 3, false),
            ]
        );
        assert_eq!(filter_tree(root, &files, "top").len(), 1);
    }

    #[test]
    fn rename_moves_the_path_and_everything_under_it() {
        let (from, to) = (Path::new("/r/old"), Path::new("/r/new"));
        assert_eq!(renamed_path(from, from, to), Some(to.to_path_buf()));
        assert_eq!(
            renamed_path(Path::new("/r/old/a.sql"), from, to),
            Some(PathBuf::from("/r/new/a.sql"))
        );
        assert_eq!(renamed_path(Path::new("/r/older.sql"), from, to), None);
    }

    #[test]
    fn names_are_single_parts() {
        assert_eq!(check_name(" report.sql "), Ok("report.sql"));
        assert!(check_name("").is_err());
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("C:report.sql").is_err());
        assert_eq!(new_file_name("report"), "report.sql");
        assert_eq!(new_file_name("notes.md"), "notes.md");
        assert_eq!(new_file_name(".env"), ".env");
    }

    #[test]
    fn picks_the_deepest_open_folder() {
        let folders = [PathBuf::from("/r"), PathBuf::from("/r/dbt")];
        let found = containing_folder(
            folders.iter().map(PathBuf::as_path),
            Path::new("/r/dbt/a.sql"),
        );
        assert_eq!(found, Some(Path::new("/r/dbt")));
        assert_eq!(
            containing_folder(folders.iter().map(PathBuf::as_path), Path::new("/x")),
            None
        );
    }

    #[test]
    fn operations_never_replace_an_entry() {
        let folder = TempFolder::new("operations");
        let file = folder.0.join("a.sql");
        FileOperation::CreateFile(file.clone()).apply().unwrap();
        assert!(FileOperation::CreateFile(file.clone()).apply().is_err());
        FileOperation::CreateFolder(folder.0.join("b"))
            .apply()
            .unwrap();
        folder.write("c.sql", "keep");
        let into_existing = FileOperation::Rename {
            from: file.clone(),
            to: folder.0.join("c.sql"),
        };
        assert!(into_existing.apply().is_err());
        assert_eq!(
            std::fs::read_to_string(folder.0.join("c.sql")).unwrap(),
            "keep"
        );
        FileOperation::Rename {
            from: file,
            to: folder.0.join("b/a.sql"),
        }
        .apply()
        .unwrap();
        assert!(folder.0.join("b/a.sql").is_file());
        let unchanged = folder.0.join("c.sql");
        FileOperation::Rename {
            from: unchanged.clone(),
            to: unchanged,
        }
        .apply()
        .unwrap();
    }

    #[test]
    fn searches_text_files_ignoring_case() {
        let folder = TempFolder::new("search");
        folder.write("a.sql", "select 1;\nSELECT * from Ventas;\n");
        folder.write("b/c.sql", "-- ventas por mes\n");
        std::fs::write(folder.0.join("blob.bin"), b"ventas\0\x01").unwrap();
        let roots = [folder.0.clone()];
        let never = AtomicBool::new(false);

        let results = search(&roots, "ventas", false, false, &never).unwrap();
        let mut found: Vec<(String, usize)> = results
            .matches
            .iter()
            .map(|found| (relative_path(&folder.0, &found.path), found.line_number))
            .collect();
        found.sort();
        assert_eq!(found, [("a.sql".to_owned(), 2), ("b/c.sql".to_owned(), 1)]);

        let literal = search(&roots, "*", false, false, &never).unwrap();
        assert_eq!(literal.matches.len(), 1);
        assert!(search(&roots, "(", true, false, &never).is_err());
        let cancelled = search(&roots, "ventas", false, false, &AtomicBool::new(true)).unwrap();
        assert_eq!(cancelled.matches.len(), 0);
    }
}
