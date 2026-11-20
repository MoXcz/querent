# AGENTS.md

Rules for agents writing code in this repo: a desktop SQL editor in Rust on gpui. The plan and stack choices live in `ROADMAP.md`. Rules here are traps to avoid, not architecture; when a rule only applies to one crate, put it in that crate's own `AGENTS.md`.

## Commands

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Run all four before saying a change is done.

## Layout

```text
Cargo.toml                 # workspace: shared deps, [workspace.lints]
crates/
  sql_editor/              # binary: gpui app, views, actions, keymap
  sql_db/                  # Driver trait, Value, one module per database
  sql_text/                # pure logic: statement under cursor, vim state machine
  sql_lsp/                 # language server client over stdio, no UI
```

`sql_text`, `sql_db` and `sql_lsp` never depend on gpui. Logic that can be tested without a window goes in `sql_text`.

## Project rules

- Every driver returns the same shape: `ResultSet { columns: Vec<Column>, rows: Vec<Vec<Value>> }`. Never pass driver row types to the UI.
- Every query has a cancel path that stops the server-side query. Dropping a future does not.
- Never log SQL text, connection strings, tokens or passwords. Log the connection id and timings.
- Secrets live in the OS keychain (`keyring`), never in the config file.
- Pin `gpui` (as `gpui-pre`) and `gpui-component` to exact versions and upgrade them together.
- The project is GPL-3.0-or-later, so code from Zed's `editor` and `vim` crates (GPL-3.0-or-later), DBDelve and ZQLZ (GPL-3.0), and gpui, gpui-component and DBFlux (Apache-2.0) may be copied. Keep the original copyright header on copied files, add `// Adapted from <repo>/<path> (<license>)` at the top, and list the source in `NOTICE.md`. Never copy from a source with no license or a non-commercial-only license.

## Rust rules

Each rule shows the preferred form first. Sources are linked once per section.

### Errors

Sources: [Zed AGENTS.md](https://github.com/zed-industries/zed/blob/main/AGENTS.md), [Effective Rust: errors](https://www.effective-rust.com/errors.html), [thiserror](https://github.com/dtolnay/thiserror).

**`thiserror` in `sql_db` and `sql_text`, `anyhow` in `sql_editor`.** Library callers need to match on variants; the app only needs context and a message.

Good:
```rust
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("connection lost")]
    ConnectionLost(#[source] std::io::Error),
    #[error("query cancelled")]
    Cancelled,
}
```

Bad:
```rust
pub async fn execute(sql: &str) -> Result<ResultSet, Box<dyn std::error::Error>> { .. }
```

**Propagate with `?` and add context at boundaries.** The chain stays readable and the happy path stays flat.

Good:
```rust
let config = fs::read_to_string(&path)
    .with_context(|| format!("reading {}", path.display()))?;
```

Bad:
```rust
let config = match fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) => return Err(anyhow!("{error}")), // drops the source chain
};
```

**No `unwrap()` or `expect()` outside tests.** A panic kills the whole app and every open tab. The one exception is a documented invariant that a test checks.

Good:
```rust
let Some(tab) = self.tabs.get(index) else { return };
let port: u16 = input.parse().context("port must be a number")?;
```

Bad:
```rust
let tab = self.tabs.get(index).unwrap();
let port: u16 = input.parse().unwrap();
```

**Never discard a `Result` with `let _ =`.** Use `?`, `.log_err()`, or an explicit `match`.

Good:
```rust
connection.close().await.log_err();
```

Bad:
```rust
let _ = connection.close().await;
```

**Use `.get()` on data you do not control.** Indexing panics out of bounds, and slicing a `&str` panics off a char boundary.

Good:
```rust
let Some(value) = row.get(column_index) else { return Err(QueryError::ColumnOutOfRange) };
```

Bad:
```rust
let value = &row[column_index];
let preview = &sql[..80];
```

**Show async failures in the UI.** A query that fails silently looks like a hang.

Good:
```rust
let result = query.await;
this.update(cx, |tab, cx| {
    tab.result = Some(result);
    cx.notify();
})
```

Bad:
```rust
if let Ok(rows) = query.await {
    // error path does nothing
}
```

### Types

Sources: [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html), [Effective Rust: types](https://www.effective-rust.com/use-types.html).

**Newtypes for ids.** The compiler then rejects a `TabId` where a `ConnectionId` belongs.

Good:
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);
```

Bad:
```rust
fn close_connection(connection_id: u64) { .. }
close_connection(tab.id); // compiles, wrong
```

**Enums instead of bools and strings.** The call site says what it means and the set of values is closed.

Good:
```rust
pub enum DriverKind { Fabric, SqlServer, Postgres, Sqlite, Snowflake }
execute(&sql, RunScope::StatementUnderCursor);
```

Bad:
```rust
if driver == "fabric" { .. }
execute(&sql, true, false);
```

**Match your own enums exhaustively; no `_` arm.** Adding a variant then fails to compile everywhere it matters.

Good:
```rust
match state {
    QueryState::Idle => ..,
    QueryState::Running(_) => ..,
    QueryState::Done(_) => ..,
    QueryState::Failed(_) => ..,
}
```

Bad:
```rust
match state {
    QueryState::Running(_) => ..,
    _ => {}
}
```

**`From`/`TryFrom` instead of ad-hoc converters.** They work with `?` and `.into()`.

Good:
```rust
impl TryFrom<&str> for DriverKind {
    type Error = UnknownDriver;
    fn try_from(value: &str) -> Result<Self, Self::Error> { .. }
}
```

Bad:
```rust
fn driver_from_string(value: &str) -> Option<DriverKind> { .. }
```

**Builder or `..Default::default()` instead of long positional arguments.**

Good:
```rust
let options = ConnectOptions { port: 1433, ..ConnectOptions::new(host) };
```

Bad:
```rust
let options = ConnectOptions::new(host, Some(1433), None, true, None, Some(30));
```

**`#[must_use]` on values that are pointless to ignore**, such as handles and builder methods.

Good:
```rust
#[must_use = "dropping the handle cannot cancel the query"]
pub struct CancelHandle { .. }
```

Bad:
```rust
pub fn with_timeout(mut self, timeout: Duration) -> Self { .. } // `options.with_timeout(d);` silently does nothing
```

**Derive `Debug` on every public type; `Clone`, `PartialEq`, `Eq` where they make sense.**

Good:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column { pub name: String, pub type_name: String }
```

Bad:
```rust
pub struct Column { pub name: String, pub type_name: String }
```

### Ownership

Sources: [Rust Design Patterns](https://rust-unofficial.github.io/patterns/), [API Guidelines: flexibility](https://rust-lang.github.io/api-guidelines/flexibility.html).

**Take borrowed parameters: `&str`, `&[T]`, `&Path`.**

Good:
```rust
fn quote_identifier(name: &str) -> String { .. }
```

Bad:
```rust
fn quote_identifier(name: &String) -> String { .. }
fn render_rows(rows: &Vec<Vec<Value>>) { .. }
```

**Borrow instead of cloning to please the borrow checker.** `redundant_clone` is denied.

Good:
```rust
let name = &self.connections[&id].name;
```

Bad:
```rust
let connections = self.connections.clone();
let name = connections[&id].name.clone();
```

**Take ownership only when you store the value**, and let the caller choose with `impl Into<_>`.

Good:
```rust
fn new(title: impl Into<SharedString>) -> Self { Self { title: title.into() } }
```

Bad:
```rust
fn new(title: &str) -> Self { Self { title: title.to_string().into() } }
```

**`Cow` when a value is usually returned unchanged.**

Good:
```rust
fn normalize_newlines(sql: &str) -> Cow<'_, str> {
    if sql.contains('\r') { Cow::Owned(sql.replace("\r\n", "\n")) } else { Cow::Borrowed(sql) }
}
```

Bad:
```rust
fn normalize_newlines(sql: &str) -> String { sql.replace("\r\n", "\n") }
```

### Idioms

Sources: [Effective Rust](https://www.effective-rust.com/), [Zed AGENTS.md](https://github.com/zed-industries/zed/blob/main/AGENTS.md).

**Iterators instead of index loops.**

Good:
```rust
let widths: Vec<usize> = columns.iter().map(|column| column.name.len()).collect();
```

Bad:
```rust
let mut widths = Vec::new();
for i in 0..columns.len() { widths.push(columns[i].name.len()); }
```

**`let-else` and combinators instead of nested `match`.**

Good:
```rust
let Some(connection) = self.active_connection() else {
    return Task::ready(Err(anyhow!("not connected")));
};
```

Bad:
```rust
match self.active_connection() {
    Some(connection) => match .. { .. },
    None => ..,
}
```

**No generics or trait objects until there is a second implementation.** `impl Trait` in argument position is fine.

Good:
```rust
fn load(paths: impl IntoIterator<Item = PathBuf>) { .. }
```

Bad:
```rust
fn run<D: Driver + Send + Sync + 'static, C: Codec<D>>(driver: D, codec: C) { .. } // one caller
```

**Full-word names.**

Good:
```rust
for (row_index, row) in rows.iter().enumerate() { .. }
```

Bad:
```rust
for (i, r) in rs.iter().enumerate() { .. }
```

**Shadow clones into async blocks.**

Good:
```rust
cx.background_spawn({
    let connection = connection.clone();
    async move { connection.ping().await }
})
```

Bad:
```rust
let connection2 = connection.clone();
cx.background_spawn(async move { connection2.ping().await })
```

**Narrow visibility and no glob imports.**

Good:
```rust
pub(crate) fn parse_host(..) { .. }
use crate::drivers::{Driver, DriverKind};
```

Bad:
```rust
pub fn parse_host(..) { .. } // internal helper
use crate::drivers::*;
```

**Comments explain why, never what.**

Good:
```rust
// Fabric drops idle sessions; reconnect once on error 24804 before failing.
```

Bad:
```rust
// loop over rows and push each one
```

### Async: tokio drivers inside gpui

Sources: [tokio Mutex](https://docs.rs/tokio/latest/tokio/sync/struct.Mutex.html), [tokio select!](https://docs.rs/tokio/latest/tokio/macro.select.html), [Zed gpui_tokio](https://github.com/zed-industries/zed/blob/main/crates/gpui_tokio/src/gpui_tokio.rs).

gpui's executors are not tokio. The database drivers need tokio, so `sql_editor` owns one tokio runtime stored as a gpui `Global`, and a helper spawns onto it and returns a gpui `Task` that aborts the tokio task when dropped.

**Never block an executor.** Blocking the foreground executor freezes the window.

Good:
```rust
let rows = tokio::task::spawn_blocking(move || sqlite_query(&path, &sql)).await??;
```

Bad:
```rust
async fn export(&self) { std::fs::write(&path, csv_bytes)?; std::thread::sleep(delay); }
```

**Never hold a `std::sync::Mutex` guard across `.await`.**

Good:
```rust
let sql = { self.state.lock().pending_sql.clone() };
let result = connection.execute(&sql).await?;
```

Bad:
```rust
let state = self.state.lock().unwrap();
let result = connection.execute(&state.pending_sql).await?;
```

**Create long-lived futures once and pin them before `select!`.** A future built inside the loop restarts on every iteration.

Good:
```rust
let query = connection.execute(&sql);
tokio::pin!(query);
tokio::select! {
    result = &mut query => result,
    () = cancel.cancelled() => Err(QueryError::Cancelled),
}
```

Bad:
```rust
loop {
    tokio::select! { result = connection.execute(&sql) => .., Some(message) = receiver.recv() => .. }
}
```

**Tie every task to an owner.** No orphaned `tokio::spawn`.

Good:
```rust
let cancel = CancellationToken::new();
self.running = Some(RunningQuery { task: spawn_query(cancel.clone(), ..), cancel });
```

Bad:
```rust
tokio::spawn(run_query(sql)); // keeps running after the tab closes
```

### gpui

Source: [Zed AGENTS.md, GPUI section](https://github.com/zed-industries/zed/blob/main/AGENTS.md).

**Store, await or detach every `Task`.** Dropping a `Task` cancels it. Storing it in a field also means a new query cancels the old one.

Good:
```rust
self.running_query = Some(cx.spawn(async move |this, cx| { .. }));
```

Bad:
```rust
cx.spawn(async move |this, cx| { .. }); // dropped at once, never runs
```

**Use the inner `cx` inside `update` closures, and never update an entity from inside its own update.** Re-entrant updates panic.

Good:
```rust
tab.update(cx, |tab, cx| {
    tab.clear_results();
    cx.notify();
});
```

Bad:
```rust
tab.update(cx, |tab, _| {
    tab.update(outer_cx, |..| ..);
});
```

**Call `cx.notify()` after any state change that affects rendering.**

Good:
```rust
fn set_result(&mut self, result: ResultSet, cx: &mut Context<Self>) {
    self.result = Some(result);
    cx.notify();
}
```

Bad:
```rust
fn set_result(&mut self, result: ResultSet) { self.result = Some(result); } // view stays stale
```

**Keyboard commands are actions, bound in a key context.** Never match raw keystrokes in `on_key_down` for commands.

Good:
```rust
actions!(sql_editor, [RunStatement, NewTab, CloseTab, ExportCsv]);
cx.bind_keys([KeyBinding::new("ctrl-enter", RunStatement, Some("SqlEditor"))]);
div().key_context("SqlEditor").on_action(cx.listener(Self::run_statement))
```

Bad:
```rust
div().on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
    if event.keystroke.key == "enter" && event.keystroke.modifiers.control { this.run(cx) }
}))
```

**Keep `Subscription`s in a field.** Dropping one unsubscribes.

Good:
```rust
self._subscriptions.push(cx.subscribe(&editor, Self::on_editor_event));
```

Bad:
```rust
cx.subscribe(&editor, Self::on_editor_event); // unsubscribed immediately
```

**Weak handles inside spawned tasks.** A strong `Entity` held by its own task leaks the view.

Good:
```rust
cx.spawn(async move |this: WeakEntity<Tab>, cx| { this.update(cx, |tab, cx| ..)? ; anyhow::Ok(()) })
```

Bad:
```rust
let this = cx.entity(); // strong handle moved into a task the entity owns
```

**Large result grids use virtualized lists.** Never build one element per row.

Good:
```rust
uniform_list("results", rows.len(), cx.processor(|this, range, _window, _cx| this.render_rows(range)))
```

Bad:
```rust
div().children(rows.iter().map(render_row))
```

### Files, lints, tests

Sources: [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [Effective Rust: clippy](https://www.effective-rust.com/clippy.html), [Rust book: tests](https://doc.rust-lang.org/book/ch11-03-test-organization.html).

**`foo.rs` next to `foo/`, never `foo/mod.rs`.** Crate roots are named after the crate: `[lib] path = "src/sql_db.rs"`.

```text
Good: src/drivers.rs + src/drivers/fabric.rs
Bad:  src/drivers/mod.rs
```

**Add code to an existing file unless it is a new component.** Avoid many tiny files.

**Versions and lints live in the workspace root.** Each crate sets `[lints] workspace = true` and `dep.workspace = true`.

Good:
```toml
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
unwrap_used = "deny"
expect_used = "deny"
dbg_macro = "deny"
todo = "deny"
print_stdout = "deny"
redundant_clone = "deny"
wildcard_enum_match_arm = "warn"
mod_module_files = "deny"
```

Bad:
```rust
#![deny(warnings)] // breaks the build on every new compiler; use -D warnings in CI instead
```

**Unit tests in the same file, integration tests in `tests/`.** `unwrap` is fine in tests. Database tests that need a server are `#[ignore]` and read connection details from env vars.

Good:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_statement_under_cursor() {
        assert_eq!(statement_at("select 1;\nselect 2;", 12), Some(10..18));
    }
}
```

Bad:
```rust
pub fn statement_at_for_tests(..) // made public only so a separate file can test it
```

**Log with `tracing` and fields; no `println!` or `dbg!`.**

Good:
```rust
#[tracing::instrument(skip_all, fields(connection = %connection_id))]
async fn execute(..) { tracing::debug!(rows = result.rows.len(), elapsed_ms, "query done"); }
```

Bad:
```rust
println!("running {sql} on {connection_string}"); // leaks secrets
```
