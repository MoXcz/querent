// Without this, Windows opens a console window next to the app.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod command_palette;
mod connection_form;
mod connections;
mod database_picker;
mod file_explorer;
mod find_in_folder;
mod folders;
mod history;
mod lsp_servers;
mod object_explorer;
mod query_tab;
mod release_notes;
mod results_table;
mod schema_cache;
mod secrets;
mod session;
mod settings;
mod sign_in;
mod sql_language;
mod themes;
mod tokio_bridge;
mod updater;
mod workspace;

use gpui_kit::base::actions::{SelectDown, SelectUp};
use gpui_kit::component::input::DeleteToPreviousWordStart;
use gpui_kit::{
    App, AppContext as _, AssetSource, Bounds, KeyBinding, SharedString, WindowBounds,
    WindowOptions, px, size,
};
use tracing_subscriber::EnvFilter;

use crate::command_palette::{OpenCommandPalette, OpenSettings};
use crate::query_tab::{
    CancelQuery, CopySelectionOrLine, CutSelectionOrLine, ExportCsv, FormatSql,
    PasteSelectionOrLine, RunAll, RunQuery, SaveFile, SaveFileAs, ToggleVimMode,
};
use crate::results_table::{CopySelection, CopySelectionWithHeaders};
use crate::workspace::{
    CloseTab, FindInFolders, NewTab, NextTab, OpenFile, OpenFolder, PreviousTab, ReopenClosedTab,
    ShowHistory, WORKSPACE_CONTEXT, Workspace,
};

const KEY_CONTEXT: &str = "SqlEditor";
/// gpui-kit's editor inside a query tab.
const EDITOR_INPUT_CONTEXT: &str = "SqlEditor > Input";
/// gpui-kit's results grid inside a query tab.
const GRID_CONTEXT: &str = "SqlEditor > DataTable";
/// gpui-kit's key context for `Command` lists.
const PALETTE_CONTEXT: &str = "Command";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            // mssql_tds logs every server error at ERROR; the UI already shows them.
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,mssql_tds=off")),
        )
        .init();

    let runtime = tokio_bridge::TokioRuntime::new()?;

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            settings::init(cx);
            secrets::init(settings::config_dir(cx));
            sign_in::init(cx);
            connections::init(cx);
            schema_cache::init(cx);
            history::init(cx);
            lsp_servers::init(cx);
            themes::init(cx);
            command_palette::init(cx);
            updater::init(cx);
            cx.on_action(|_: &ToggleVimMode, cx| {
                settings::update(cx, |settings| settings.vim_mode = !settings.vim_mode);
                cx.refresh_windows();
            });
            cx.set_global(runtime);
            bind_keys(cx);
            open_main_window(cx);
            cx.activate(true);
        });
    Ok(())
}

// Icons beyond gpui-kit's default set, embedded one by one.
gpui_kit::assets::icon_assets!(ExtraIcons, [CircleStop, Download]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn bind_keys(cx: &mut App) {
    let (open_file, save, save_as, reopen_tab) = if cfg!(target_os = "macos") {
        ("cmd-o", "cmd-s", "cmd-shift-s", "cmd-shift-t")
    } else {
        ("ctrl-o", "ctrl-s", "ctrl-shift-s", "ctrl-shift-t")
    };
    // ctrl-shift and alt chords pass vim's normal mode untouched.
    let (open_folder, find_in_folders) = if cfg!(target_os = "macos") {
        ("cmd-shift-o", "cmd-alt-f")
    } else {
        ("ctrl-shift-o", "ctrl-alt-f")
    };
    let (palette, open_settings, history) = if cfg!(target_os = "macos") {
        ("cmd-shift-p", "cmd-,", "cmd-shift-h")
    } else {
        ("ctrl-shift-p", "ctrl-,", "ctrl-shift-h")
    };
    let (run, run_all, new_tab, close_tab, format) = if cfg!(target_os = "macos") {
        (
            "cmd-enter",
            "cmd-shift-enter",
            "cmd-n",
            "cmd-w",
            "cmd-shift-f",
        )
    } else {
        (
            "ctrl-enter",
            "ctrl-shift-enter",
            "ctrl-n",
            "ctrl-shift-w",
            "ctrl-shift-f",
        )
    };
    let (copy, copy_with_headers, cut, paste) = if cfg!(target_os = "macos") {
        ("cmd-c", "cmd-shift-c", "cmd-x", "cmd-v")
    } else {
        ("ctrl-c", "ctrl-shift-c", "ctrl-x", "ctrl-v")
    };
    // Registered after `gpui_kit::init` so these win over the editor's own ctrl-enter.
    cx.bind_keys([
        KeyBinding::new(run, RunQuery, Some(KEY_CONTEXT)),
        KeyBinding::new(run, RunQuery, Some("Input")),
        KeyBinding::new(run_all, RunAll, Some(KEY_CONTEXT)),
        KeyBinding::new(run_all, RunAll, Some("Input")),
        KeyBinding::new("ctrl-shift-c", CancelQuery, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-e", ExportCsv, Some(KEY_CONTEXT)),
        KeyBinding::new(format, FormatSql, Some(KEY_CONTEXT)),
        KeyBinding::new(save, SaveFile, Some(KEY_CONTEXT)),
        KeyBinding::new(save_as, SaveFileAs, Some(KEY_CONTEXT)),
        KeyBinding::new(open_file, OpenFile, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(reopen_tab, ReopenClosedTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(open_folder, OpenFolder, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(find_in_folders, FindInFolders, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(new_tab, NewTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(palette, OpenCommandPalette, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(open_settings, OpenSettings, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(history, ShowHistory, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(close_tab, CloseTab, Some(WORKSPACE_CONTEXT)),
        // Vim's insert-mode delete-word-before-cursor; with vim off it does the same.
        KeyBinding::new("ctrl-w", DeleteToPreviousWordStart, Some("Input")),
        // Over the editor's own copy, cut and paste, which do nothing without a selection.
        KeyBinding::new(copy, CopySelectionOrLine, Some(EDITOR_INPUT_CONTEXT)),
        KeyBinding::new(cut, CutSelectionOrLine, Some(EDITOR_INPUT_CONTEXT)),
        KeyBinding::new(paste, PasteSelectionOrLine, Some(EDITOR_INPUT_CONTEXT)),
        // In the grid these win over Cancel Query's ctrl-shift-c, bound on the whole tab.
        KeyBinding::new(copy, CopySelection, Some(GRID_CONTEXT)),
        KeyBinding::new(
            copy_with_headers,
            CopySelectionWithHeaders,
            Some(GRID_CONTEXT),
        ),
        // Vim-style list movement in the palette and the history list.
        KeyBinding::new("ctrl-n", SelectDown, Some(PALETTE_CONTEXT)),
        KeyBinding::new("ctrl-p", SelectUp, Some(PALETTE_CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-pagedown", NextTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-pageup", PreviousTab, Some(WORKSPACE_CONTEXT)),
    ]);
}

fn open_main_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1200.), px(800.)),
            cx,
        ))),
        ..WindowOptions::default()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("Querent");
        cx.new(|cx| Workspace::new(window, cx))
    });
    if let Err(error) = opened {
        tracing::error!(%error, "failed to open window");
        cx.quit();
    }
}
