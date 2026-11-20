mod query_window;
mod results_table;
mod settings;
mod themes;
mod tokio_bridge;

use gpui_kit::{App, AppContext as _, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size};
use tracing_subscriber::EnvFilter;

use crate::query_window::{CancelQuery, QueryWindow, RunAll, RunQuery};

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
            themes::init(cx);
            cx.set_global(runtime);
            bind_keys(cx);
            open_main_window(cx);
            cx.activate(true);
        });
    Ok(())
}

fn bind_keys(cx: &mut App) {
    let (run, run_all) = if cfg!(target_os = "macos") {
        ("cmd-enter", "cmd-shift-enter")
    } else {
        ("ctrl-enter", "ctrl-shift-enter")
    };
    // Registered after `gpui_kit::init` so these win over the editor's own ctrl-enter.
    cx.bind_keys([
        KeyBinding::new(run, RunQuery, Some(KEY_CONTEXT)),
        KeyBinding::new(run, RunQuery, Some("Input")),
        KeyBinding::new(run_all, RunAll, Some(KEY_CONTEXT)),
        KeyBinding::new(run_all, RunAll, Some("Input")),
        KeyBinding::new("ctrl-shift-c", CancelQuery, Some(KEY_CONTEXT)),
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
        cx.new(|cx| QueryWindow::new(window, cx))
    });
    if let Err(error) = opened {
        tracing::error!(%error, "failed to open window");
        cx.quit();
    }
}
