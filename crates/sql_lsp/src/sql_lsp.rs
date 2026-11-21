//! A language server client over stdio: framing, requests, notifications, `initialize`.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lsp_types::notification::Notification;
use lsp_types::request::Request;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::sync::{mpsc, oneshot};

/// How long a request waits for its response.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Larger frames are refused rather than allocated.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// A header line longer than this is a server that never ends the line.
const MAX_HEADER_BYTES: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("could not start {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the language server stopped")]
    Stopped,
    #[error("the language server did not answer in time")]
    Timeout,
    #[error("language server error {code}: {message}")]
    Server { code: i64, message: String },
    #[error("bad message from the language server: {0}")]
    Protocol(String),
}

/// A notification from the server, such as `textDocument/publishDiagnostics`.
#[derive(Debug, Clone)]
pub struct ServerNotification {
    pub method: String,
    pub params: Value,
}

/// Waiting requests by id. A std mutex: it is held only for a map operation, never across an await.
type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;

fn lock(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<Result<Value, LspError>>>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes a request that stopped waiting (timeout, or its future dropped) and
/// tells the server to stop working on it.
struct RequestGuard<'a> {
    client: &'a LspClient,
    id: i64,
    done: bool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if lock(&self.client.pending).remove(&self.id).is_some() {
            self.client.cancel_request(self.id);
        }
    }
}

/// A running server. Dropping it stops the reader and writer tasks and, for a
/// spawned process, kills it.
pub struct LspClient {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Pending,
    /// Set once the server's output ended; no answer can come after that.
    closed: Arc<AtomicBool>,
    next_id: AtomicI64,
    capabilities: lsp_types::ServerCapabilities,
    child: Option<tokio::process::Child>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LspClient")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl LspClient {
    /// Starts `command` and runs the `initialize` handshake. Server
    /// notifications go to `notifications`; it closes when the server exits.
    pub async fn spawn(
        command: &str,
        args: &[String],
        initialization_options: Option<Value>,
        notifications: mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<Self, LspError> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| LspError::Spawn {
                command: command.to_owned(),
                source,
            })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(LspError::Protocol("no stdio pipes".to_owned()));
        };
        let mut client =
            Self::connect(stdout, stdin, initialization_options, notifications).await?;
        client.child = Some(child);
        Ok(client)
    }

    /// Runs the protocol over any reader and writer; `spawn` uses the child's
    /// stdio, tests use an in-memory pipe.
    pub async fn connect(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        initialization_options: Option<Value>,
        notifications: mpsc::UnboundedSender<ServerNotification>,
    ) -> Result<Self, LspError> {
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let (outgoing, outgoing_receiver) = mpsc::unbounded_channel();
        let tasks = vec![
            tokio::spawn(write_messages(writer, outgoing_receiver)),
            tokio::spawn(read_messages(
                reader,
                pending.clone(),
                closed.clone(),
                outgoing.clone(),
                notifications,
            )),
        ];
        let mut client = Self {
            outgoing,
            pending,
            closed,
            next_id: AtomicI64::new(1),
            capabilities: lsp_types::ServerCapabilities::default(),
            child: None,
            tasks,
        };
        let result = client
            .request::<lsp_types::request::Initialize>(initialize_params(initialization_options))
            .await?;
        client.capabilities = result.capabilities;
        client.notify::<lsp_types::notification::Initialized>(lsp_types::InitializedParams {})?;
        Ok(client)
    }

    #[must_use]
    pub fn capabilities(&self) -> &lsp_types::ServerCapabilities {
        &self.capabilities
    }

    /// Whether the process (if any) is still running and the pipes are open.
    pub fn is_running(&mut self) -> bool {
        let process_alive = self
            .child
            .as_mut()
            .is_none_or(|child| matches!(child.try_wait(), Ok(None)));
        process_alive && !self.outgoing.is_closed() && !self.closed.load(Ordering::Acquire)
    }

    pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        let mut guard = RequestGuard {
            client: self,
            id,
            done: false,
        };
        // The reader drains `pending` once when the server stops; a request
        // inserted after that would otherwise wait out the whole timeout.
        if self.closed.load(Ordering::Acquire) {
            return Err(LspError::Stopped);
        }
        if self.outgoing.send(message(id, R::METHOD, &params)).is_err() {
            return Err(LspError::Stopped);
        }
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(response)) => {
                guard.done = true;
                response?
            }
            Ok(Err(_)) => {
                guard.done = true;
                return Err(LspError::Stopped);
            }
            Err(_) => return Err(LspError::Timeout),
        };
        serde_json::from_value(response).map_err(|error| LspError::Protocol(error.to_string()))
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "callers build the params for this call, as with `request`"
    )]
    pub fn notify<N: Notification>(&self, params: N::Params) -> Result<(), LspError> {
        let mut message = message(0, N::METHOD, &params);
        if let Some(object) = message.as_object_mut() {
            object.remove("id");
        }
        self.outgoing.send(message).map_err(|_| LspError::Stopped)
    }

    /// `shutdown` then `exit`; the process is killed on drop either way.
    pub async fn shutdown(self) {
        if let Err(error) = self.request::<lsp_types::request::Shutdown>(()).await {
            tracing::debug!(%error, "language server shutdown");
        }
        if let Err(error) = self.notify::<lsp_types::notification::Exit>(()) {
            tracing::debug!(%error, "language server exit");
        }
        // Let the writer flush `exit` before dropping aborts it.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    fn cancel_request(&self, id: i64) {
        let params = lsp_types::CancelParams {
            id: lsp_types::NumberOrString::Number(i32::try_from(id).unwrap_or(i32::MAX)),
        };
        if let Err(error) = self.notify::<lsp_types::notification::Cancel>(params) {
            tracing::debug!(%error, "cancel request");
        }
    }
}

fn initialize_params(initialization_options: Option<Value>) -> lsp_types::InitializeParams {
    use lsp_types::{
        ClientCapabilities, CompletionClientCapabilities, HoverClientCapabilities, MarkupKind,
        PublishDiagnosticsClientCapabilities, TextDocumentClientCapabilities,
        TextDocumentSyncClientCapabilities,
    };
    lsp_types::InitializeParams {
        process_id: Some(std::process::id()),
        initialization_options,
        capabilities: ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                synchronization: Some(TextDocumentSyncClientCapabilities::default()),
                completion: Some(CompletionClientCapabilities::default()),
                hover: Some(HoverClientCapabilities {
                    content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    ..HoverClientCapabilities::default()
                }),
                publish_diagnostics: Some(PublishDiagnosticsClientCapabilities::default()),
                formatting: Some(lsp_types::DocumentFormattingClientCapabilities::default()),
                ..TextDocumentClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        },
        client_info: Some(lsp_types::ClientInfo {
            name: "querent".to_owned(),
            version: None,
        }),
        ..lsp_types::InitializeParams::default()
    }
}

/// A JSON-RPC message; `params` is left out when it is `null` (`shutdown`, `exit`).
fn message(id: i64, method: &str, params: &impl serde::Serialize) -> Value {
    let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
    let params = serde_json::to_value(params).unwrap_or(Value::Null);
    if !params.is_null()
        && let Some(object) = message.as_object_mut()
    {
        object.insert("params".to_owned(), params);
    }
    message
}

async fn write_messages(
    mut writer: impl AsyncWrite + Unpin,
    mut messages: mpsc::UnboundedReceiver<Value>,
) {
    while let Some(message) = messages.recv().await {
        let body = message.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        if let Err(error) = writer.write_all(frame.as_bytes()).await {
            tracing::debug!(%error, "language server stdin closed");
            return;
        }
        if let Err(error) = writer.flush().await {
            tracing::debug!(%error, "language server stdin closed");
            return;
        }
    }
}

async fn read_messages(
    reader: impl AsyncRead + Unpin,
    pending: Pending,
    closed: Arc<AtomicBool>,
    outgoing: mpsc::UnboundedSender<Value>,
    notifications: mpsc::UnboundedSender<ServerNotification>,
) {
    let mut reader = BufReader::new(reader);
    loop {
        let message = match read_frame(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%error, "language server stdout");
                break;
            }
        };
        dispatch(&message, &pending, &outgoing, &notifications);
    }
    closed.store(true, Ordering::Release);
    // Wake every waiting request: no answer will come.
    for (_, waiter) in lock(&pending).drain() {
        if waiter.send(Err(LspError::Stopped)).is_err() {
            tracing::debug!("request dropped before the server stopped");
        }
    }
}

fn dispatch(
    message: &Value,
    pending: &Pending,
    outgoing: &mpsc::UnboundedSender<Value>,
    notifications: &mpsc::UnboundedSender<ServerNotification>,
) {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let id = message.get("id").cloned();
    match (method, id) {
        // A request from the server: answer so it does not wait. Settings go
        // through `didChangeConfiguration`, so `workspace/configuration` gets
        // one `null` per item it asked for.
        (Some(method), Some(id)) => {
            let result = if method == "workspace/configuration" {
                let items = message
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Value::Array(vec![Value::Null; items])
            } else {
                Value::Null
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if outgoing.send(reply).is_err() {
                tracing::debug!("reply to a server request after stop");
            }
        }
        (Some(method), None) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            if notifications
                .send(ServerNotification { method, params })
                .is_err()
            {
                tracing::debug!("server notification after the editor stopped listening");
            }
        }
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else {
                return;
            };
            let Some(waiter) = lock(pending).remove(&id) else {
                return;
            };
            let result = match message.get("error") {
                Some(error) => Err(LspError::Server {
                    code: error
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            if waiter.send(result).is_err() {
                tracing::debug!(id, "response after the request gave up");
            }
        }
        (None, None) => tracing::debug!("language server message with no id or method"),
    }
}

/// One `Content-Length` framed message; `None` at end of stream.
async fn read_frame(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Value>, LspError> {
    let mut length = None;
    loop {
        let mut line = String::new();
        let read = (&mut *reader)
            .take(MAX_HEADER_BYTES as u64)
            .read_line(&mut line)
            .await
            .map_err(|error| LspError::Protocol(error.to_string()))?;
        if read == 0 {
            return Ok(None);
        }
        if read == MAX_HEADER_BYTES && !line.ends_with('\n') {
            return Err(LspError::Protocol("header line is too long".to_owned()));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.ok_or_else(|| LspError::Protocol("missing Content-Length".to_owned()))?;
    if length > MAX_FRAME_BYTES {
        return Err(LspError::Protocol(format!(
            "message of {length} bytes is too large"
        )));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| LspError::Protocol(error.to_string()))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| LspError::Protocol(error.to_string()))
}

impl LspError {
    /// A name for logs; the full text can quote the server's words, and those may hold SQL.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "spawn",
            Self::Stopped => "stopped",
            Self::Timeout => "timeout",
            Self::Server { .. } => "server error",
            Self::Protocol(_) => "protocol",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_endless_headers_and_huge_frames() {
        let endless = vec![b'x'; MAX_HEADER_BYTES * 2];
        let mut reader = tokio::io::BufReader::new(endless.as_slice());
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(LspError::Protocol(_))
        ));
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        let mut reader = tokio::io::BufReader::new(huge.as_bytes());
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(LspError::Protocol(_))
        ));
        let fine = "Content-Length: 2\r\n\r\n{}";
        let mut reader = tokio::io::BufReader::new(fine.as_bytes());
        assert!(matches!(read_frame(&mut reader).await, Ok(Some(_))));
    }

    /// A fake server: answers `initialize`, `textDocument/completion` and
    /// `shutdown`, and publishes one diagnostic after `didOpen`.
    async fn fake_server(
        reader: impl AsyncRead + Unpin + Send + 'static,
        mut writer: impl AsyncWrite + Unpin + Send + 'static,
    ) {
        let mut reader = BufReader::new(reader);
        let send = async |writer: &mut (dyn AsyncWrite + Unpin + Send), message: Value| {
            let body = message.to_string();
            let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
            writer.write_all(frame.as_bytes()).await.unwrap();
        };
        while let Ok(Some(message)) = read_frame(&mut reader).await {
            let id = message.get("id").cloned();
            match message.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    send(&mut writer, json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{"completionProvider":{}}}})).await;
                }
                Some("textDocument/completion") => {
                    // A server request first, which the client must answer.
                    send(&mut writer, json!({"jsonrpc":"2.0","id":99,"method":"workspace/configuration","params":{"items":[]}})).await;
                    send(
                        &mut writer,
                        json!({"jsonrpc":"2.0","id":id,"result":[{"label":"customers"}]}),
                    )
                    .await;
                }
                Some("textDocument/didOpen") => {
                    send(&mut writer, json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///t.sql","diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"message":"bad"}]}})).await;
                }
                Some("shutdown") => {
                    send(&mut writer, json!({"jsonrpc":"2.0","id":id,"result":null})).await;
                }
                Some("exit") => return,
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn talks_to_a_server_over_a_pipe() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_side);
        tokio::spawn(fake_server(server_reader, server_writer));
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (notification_sender, mut notifications) = mpsc::unbounded_channel();
        let client = LspClient::connect(client_reader, client_writer, None, notification_sender)
            .await
            .unwrap();
        assert!(client.capabilities().completion_provider.is_some());

        let uri: lsp_types::Uri = "file:///t.sql".parse().unwrap();
        client
            .notify::<lsp_types::notification::DidOpenTextDocument>(
                lsp_types::DidOpenTextDocumentParams {
                    text_document: lsp_types::TextDocumentItem::new(
                        uri.clone(),
                        "sql".to_owned(),
                        1,
                        "selct".to_owned(),
                    ),
                },
            )
            .unwrap();
        let notification = notifications.recv().await.unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");

        let response = client
            .request::<lsp_types::request::Completion>(lsp_types::CompletionParams {
                text_document_position: lsp_types::TextDocumentPositionParams::new(
                    lsp_types::TextDocumentIdentifier::new(uri),
                    lsp_types::Position::new(0, 0),
                ),
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
                context: None,
            })
            .await
            .unwrap();
        let Some(lsp_types::CompletionResponse::Array(items)) = response else {
            panic!("unexpected response {response:?}");
        };
        assert_eq!(items[0].label, "customers");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn requests_after_the_server_stops_fail_at_once() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (server_reader, mut server_writer) = tokio::io::split(server_side);
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let message = read_frame(&mut reader).await.unwrap().unwrap();
            let body = json!({"jsonrpc":"2.0","id":message["id"],"result":{"capabilities":{}}})
                .to_string();
            let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
            server_writer.write_all(frame.as_bytes()).await.unwrap();
            // Then the server goes away.
        });
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (sender, _notifications) = mpsc::unbounded_channel();
        let client = LspClient::connect(client_reader, client_writer, None, sender)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let result = client.request::<lsp_types::request::Shutdown>(()).await;
        assert!(matches!(result, Err(LspError::Stopped)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_missing_command_is_a_spawn_error() {
        let (sender, _) = mpsc::unbounded_channel();
        let result = LspClient::spawn("querent-no-such-server", &[], None, sender).await;
        assert!(matches!(result, Err(LspError::Spawn { .. })));
    }
}
