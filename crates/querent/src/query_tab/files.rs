//! A tab's `.sql` file: opening it, saving it, and knowing when the two differ.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, Context, SharedString, Window};

use gpui_kit::Task;

use super::{QueryTab, SaveFile, SaveFileAs};

/// A tab's link to a `.sql` file.
#[derive(Default)]
pub(super) struct TabFile {
    /// The file this tab was opened from or saved to.
    path: Option<PathBuf>,
    /// Fingerprint of the text as the file holds it; `None` when that is unknown.
    saved_fingerprint: Option<u64>,
    /// The text differs from the file.
    modified: bool,
    /// A write to the file is under way.
    saving: bool,
    /// The file's modification time when it was last read or written here; `None` when unknown.
    disk_modified: Option<SystemTime>,
    /// Waits for the save dialog; kept apart from the write so neither cancels the other.
    prompt_task: Option<Task<()>>,
    write_task: Option<Task<()>>,
    check_task: Option<Task<()>>,
}

/// A file's text and when the file was last modified.
pub struct FileText {
    pub text: String,
    pub modified: Option<SystemTime>,
}

/// A larger file is not a script to edit by hand.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// The file's text, or a message for the user.
pub fn read_sql_file(path: &Path) -> Result<FileText, String> {
    let failed = |error: std::io::Error| format!("Could not open {}: {error}.", path.display());
    let metadata = std::fs::metadata(path).map_err(failed)?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is larger than 50 MB and was not opened.",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(failed)?;
    let text = sql_text::decode_file(&bytes)
        .map_err(|error| format!("Could not open {}: {error}.", path.display()))?;
    Ok(FileText {
        text,
        modified: metadata.modified().ok(),
    })
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

impl QueryTab {
    pub fn file(&self) -> Option<&Path> {
        self.file.path.as_deref()
    }

    /// An untouched tab, which opening a file may reuse instead of adding one.
    pub fn is_blank(&self, cx: &App) -> bool {
        self.file.path.is_none() && self.editor.read(cx).text().len() == 0
    }

    /// Shows `file`, just read from `path`, as this tab's saved content.
    pub fn open_file(
        &mut self,
        path: PathBuf,
        file: &FileText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sql(&file.text, window, cx);
        self.file.path = Some(path);
        self.file.disk_modified = file.modified;
        // Taken from the editor: it may store line endings differently from the file.
        self.file.saved_fingerprint = Some(self.fingerprint(cx));
        self.file.modified = false;
        cx.notify();
    }

    /// The tab's file was renamed or moved; its text and saved state stay as they were.
    pub fn move_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file.path = Some(path);
        cx.notify();
    }

    /// The file is gone; the tab's text now counts as unsaved, so Save writes it back knowingly.
    pub fn forget_saved_state(&mut self, cx: &mut Context<Self>) {
        self.file.saved_fingerprint = None;
        self.file.disk_modified = None;
        self.file.modified = true;
        cx.notify();
    }

    /// Ties a tab restored from the session back to its file.
    pub fn restore_file(&mut self, path: PathBuf, modified: bool, cx: &mut Context<Self>) {
        self.file.path = Some(path);
        self.file.disk_modified = None;
        // What the file held is unknown for a modified tab; any text then counts as changed.
        self.file.saved_fingerprint = (!modified).then(|| self.fingerprint(cx));
        self.file.modified = modified;
        cx.notify();
    }

    /// Whether the tab has a file and differs from it.
    pub fn is_modified(&self) -> bool {
        self.file.modified
    }

    /// `report.sql`, with `*` while the tab differs from the file.
    pub(super) fn file_title(&self) -> Option<SharedString> {
        let name = self.file.path.as_deref()?.file_name()?.to_string_lossy();
        let mark = if self.file.modified { "*" } else { "" };
        Some(format!("{name}{mark}").into())
    }

    fn fingerprint(&self, cx: &App) -> u64 {
        sql_text::fingerprint(self.editor.read(cx).text().chunks())
    }

    /// Run on every edit; hashing reads the buffer once and copies nothing.
    pub(super) fn refresh_modified(&mut self, cx: &mut Context<Self>) {
        let modified =
            self.file.path.is_some() && self.file.saved_fingerprint != Some(self.fingerprint(cx));
        if modified != self.file.modified {
            self.file.modified = modified;
            cx.notify();
        }
    }

    pub(super) fn save_file(&mut self, _: &SaveFile, window: &mut Window, cx: &mut Context<Self>) {
        match self.file.path.clone() {
            Some(path) => self.write_file(path, window, cx),
            None => self.save_file_as(&SaveFileAs, window, cx),
        }
    }

    pub(super) fn save_file_as(
        &mut self,
        _: &SaveFileAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file.saving {
            return;
        }
        let directory = self
            .file
            .path
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(documents_directory, Path::to_path_buf);
        let name = self
            .file
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(
                || format!("query-{}.sql", self.number),
                |name| name.to_string_lossy().into_owned(),
            );
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));
        self.file.prompt_task = Some(cx.spawn_in(window, async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "save dialog failed");
                    return;
                }
            };
            let saved = this.update_in(cx, |this, window, cx| this.write_file(path, window, cx));
            if let Err(error) = saved {
                tracing::debug!(%error, "tab closed before the file was chosen");
            }
        }));
    }

    fn write_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // A second write could land before the first; the title keeps its `*` for the next save.
        if self.file.saving {
            return;
        }
        self.file.saving = true;
        let text = self.editor.read(cx).value().to_string();
        let fingerprint = sql_text::fingerprint([text.as_str()]);
        let written = cx.background_executor().spawn({
            let path = path.clone();
            async move { std::fs::write(&path, text).map(|()| modified_time(&path)) }
        });
        self.file.write_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = written.await;
            let shown = this.update_in(cx, |this, window, cx| {
                this.file.saving = false;
                match result {
                    Ok(disk_modified) => {
                        this.file.disk_modified = disk_modified;
                        this.file.path = Some(path);
                        this.file.saved_fingerprint = Some(fingerprint);
                        // The user may have typed while the file was written.
                        this.file.modified = true;
                        this.refresh_modified(cx);
                        cx.notify();
                    }
                    Err(error) => crate::workspace::notify_error(
                        format!("Could not save {}: {error}.", path.display()),
                        window,
                        cx,
                    ),
                }
            });
            if let Err(error) = shown {
                tracing::debug!(%error, "tab closed before the file was saved");
            }
        }));
    }
}

impl QueryTab {
    /// Looks whether another program changed the tab's file. A tab without
    /// changes of its own takes the new text; one with changes is only told.
    pub fn check_file_on_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.file.path.clone() else {
            return;
        };
        if self.file.saving {
            return;
        }
        let known = self.file.disk_modified;
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move {
                // A file that is gone is left alone: saving writes it again.
                let now = modified_time(&path)?;
                (Some(now) != known).then(|| read_sql_file(&path))
            }
        });
        self.file.check_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(read) = read.await else {
                return;
            };
            let applied = this.update_in(cx, |this, window, cx| {
                // A save or Save As may have happened while the file was read.
                if this.file.path.as_deref() != Some(path.as_path())
                    || this.file.saving
                    || this.file.disk_modified != known
                {
                    return;
                }
                match read {
                    Ok(file) => this.take_disk_change(&path, &file, known.is_some(), window, cx),
                    Err(message) => tracing::debug!(%message, "could not reread a tab's file"),
                }
            });
            if let Err(error) = applied {
                tracing::debug!(%error, "tab closed before its file was checked");
            }
        }));
    }

    /// `known_before` is false on the first look after a restart, when a
    /// difference may only be this tab's own unsaved text.
    fn take_disk_change(
        &mut self,
        path: &Path,
        file: &FileText,
        known_before: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        self.file.disk_modified = file.modified;
        if self.file.modified {
            if known_before {
                window.push_notification(
                    format!("{name} changed on disk. Saving this tab will overwrite it."),
                    cx,
                );
            }
            return;
        }
        let before = self.fingerprint(cx);
        self.set_sql(&file.text, window, cx);
        let after = self.fingerprint(cx);
        self.file.saved_fingerprint = Some(after);
        if before != after {
            window.push_notification(format!("Reloaded {name}: it changed on disk."), cx);
        }
        cx.notify();
    }
}

/// Documents, else home, else the working directory.
fn documents_directory() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|directories| {
            directories
                .document_dir()
                .map(Path::to_path_buf)
                .or_else(|| Some(directories.home_dir().to_path_buf()))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}
