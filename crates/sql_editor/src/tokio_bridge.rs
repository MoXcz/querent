//! gpui runs its own executors; the database drivers need tokio.

use std::future::Future;

use gpui_kit::{App, Global, Task};

pub struct TokioRuntime(tokio::runtime::Runtime);

impl Global for TokioRuntime {}

impl TokioRuntime {
    pub fn new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sql-editor-tokio")
            .enable_all()
            .build()?;
        Ok(Self(runtime))
    }
}

/// Aborts the tokio task when the returned gpui `Task` is dropped.
pub fn spawn<R>(
    cx: &App,
    future: impl Future<Output = R> + Send + 'static,
) -> Task<anyhow::Result<R>>
where
    R: Send + 'static,
{
    let join_handle = cx.global::<TokioRuntime>().0.spawn(future);
    let abort_on_drop = AbortOnDrop(join_handle.abort_handle());
    cx.background_executor().spawn(async move {
        let _abort_on_drop = abort_on_drop;
        Ok(join_handle.await?)
    })
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
