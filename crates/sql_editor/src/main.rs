mod connections;
mod object_explorer;
mod query_tab;
mod results_table;
mod settings;
mod themes;
mod tokio_bridge;
mod workspace;

use gpui_kit::component::input::DeleteToPreviousWordStart;
use gpui_kit::{App, AppContext as _, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size};
use tracing_subscriber::EnvFilter;

use crate::query_tab::{CancelQuery, ExportCsv, RunAll, RunQuery, ToggleVimMode};
use crate::workspace::{CloseTab, NewTab, NextTab, PreviousTab, WORKSPACE_CONTEXT, Workspace};

const KEY_CONTEXT: &str = "SqlEditor";

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
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            settings::init(cx);
            connections::init(cx);
            themes::init(cx);
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

fn bind_keys(cx: &mut App) {
    let (run, run_all, new_tab, close_tab) = if cfg!(target_os = "macos") {
        ("cmd-enter", "cmd-shift-enter", "cmd-n", "cmd-w")
    } else {
        ("ctrl-enter", "ctrl-shift-enter", "ctrl-n", "ctrl-shift-w")
    };
    // Registered after `gpui_kit::init` so these win over the editor's own ctrl-enter.
    cx.bind_keys([
        KeyBinding::new(run, RunQuery, Some(KEY_CONTEXT)),
        KeyBinding::new(run, RunQuery, Some("Input")),
        KeyBinding::new(run_all, RunAll, Some(KEY_CONTEXT)),
        KeyBinding::new(run_all, RunAll, Some("Input")),
        KeyBinding::new("ctrl-shift-c", CancelQuery, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-e", ExportCsv, Some(KEY_CONTEXT)),
        KeyBinding::new(new_tab, NewTab, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(close_tab, CloseTab, Some(WORKSPACE_CONTEXT)),
        // Vim's insert-mode delete-word-before-cursor; with vim off it does the same.
        KeyBinding::new("ctrl-w", DeleteToPreviousWordStart, Some("Input")),
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
        window.set_window_title("SQL Editor");
        cx.new(|cx| Workspace::new(window, cx))
    });
    if let Err(error) = opened {
        tracing::error!(%error, "failed to open window");
        cx.quit();
    }
}
