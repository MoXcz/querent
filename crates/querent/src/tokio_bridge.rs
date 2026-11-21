//! gpui runs its own executors; the database drivers need tokio.

use std::future::Future;

use gpui_kit::{App, Global, Task};

pub struct TokioRuntime(tokio::runtime::Runtime);

impl Global for TokioRuntime {}

impl TokioRuntime {
    pub fn new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("querent-tokio")
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

/// Blocks this thread until `future` ends or `limit` passes. Only for app
/// quit, where nothing else will run the tasks that send a cancel request.
pub fn block_on_for(cx: &App, limit: std::time::Duration, future: impl Future<Output = ()>) {
    let runtime = &cx.global::<TokioRuntime>().0;
    let finished = runtime.block_on(async { tokio::time::timeout(limit, future).await });
    if finished.is_err() {
        tracing::warn!("gave up waiting for running queries to stop");
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
