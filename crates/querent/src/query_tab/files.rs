//! A tab's `.sql` file: opening it, saving it, and knowing when the two differ.

use std::path::{Path, PathBuf};

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
    /// Waits for the save dialog; kept apart from the write so neither cancels the other.
    prompt_task: Option<Task<()>>,
    write_task: Option<Task<()>>,
}

impl QueryTab {
    pub fn file(&self) -> Option<&Path> {
        self.file.path.as_deref()
    }

    /// An untouched tab, which opening a file may reuse instead of adding one.
    pub fn is_blank(&self, cx: &App) -> bool {
        self.file.path.is_none() && self.editor.read(cx).text().len() == 0
    }

    /// Shows `text`, just read from `path`, as this tab's saved content.
    pub fn open_file(
        &mut self,
        path: PathBuf,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sql(text, window, cx);
        self.file.path = Some(path);
        // Taken from the editor: it may store line endings differently from the file.
        self.file.saved_fingerprint = Some(self.fingerprint(cx));
        self.file.modified = false;
        cx.notify();
    }

    /// Ties a tab restored from the session back to its file.
    pub fn restore_file(&mut self, path: PathBuf, modified: bool, cx: &mut Context<Self>) {
        self.file.path = Some(path);
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
            async move { std::fs::write(&path, text) }
        });
        self.file.write_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = written.await;
            let shown = this.update_in(cx, |this, window, cx| {
                this.file.saving = false;
                match result {
                    Ok(()) => {
                        this.file.path = Some(path);
                        this.file.saved_fingerprint = Some(fingerprint);
                        // The user may have typed while the file was written.
                        this.file.modified = true;
                        this.refresh_modified(cx);
                        cx.notify();
                    }
                    Err(error) => window.push_notification(
                        format!("Could not save {}: {error}.", path.display()),
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
