//! Language servers from `settings.toml`, one per saved connection, restarted with backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{App, BorrowAppContext as _, Global, SharedString, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sql_db::{Authentication, ConnectionSettings, DriverKind};
use sql_lsp::{LspClient, ServerNotification};
use tokio::sync::mpsc;

use crate::connections::{self, ConnectionStore};
use crate::settings::{self, SettingsStore};
use crate::tokio_bridge;

/// A server that stayed up this long starts its backoff from the beginning again.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// A server for one kind of database, from `[language_servers.<kind>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageServerSetting {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Send the connection (including its password) to the server, for
    /// servers such as `sqls` that read the live schema.
    #[serde(default)]
    pub pass_connection: bool,
}

/// What the status line says about a connection's server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Starting,
    Running,
    /// Failed to start or exited; restarts on next use after `retry_at`.
    Failed {
        message: String,
        retry_at: Instant,
    },
}

struct Server {
    /// What it was started with; a change to either restarts it.
    setting: LanguageServerSetting,
    connection: ConnectionSettings,
    /// Tells this start's results from those of an older, replaced one.
    generation: u64,
    status: ServerStatus,
    client: Option<Arc<LspClient>>,
    started_at: Option<Instant>,
    attempts: u32,
    /// Version and text last sent, per document.
    documents: HashMap<String, (i32, Arc<str>)>,
    _listener: Option<Task<()>>,
}

#[derive(Default)]
pub struct LspServers {
    servers: HashMap<SharedString, Server>,
    /// Latest `publishDiagnostics` per connection and document.
    diagnostics: HashMap<(SharedString, String), Vec<lsp_types::Diagnostic>>,
    next_generation: u64,
}

impl Global for LspServers {}

pub fn init(cx: &mut App) {
    cx.set_global(LspServers::default());
    cx.observe_global::<SettingsStore>(stop_changed).detach();
    cx.observe_global::<ConnectionStore>(stop_changed).detach();
}

/// Stops servers whose setting or connection changed; they restart on next use.
fn stop_changed(cx: &mut App) {
    let changed: Vec<SharedString> = cx
        .global::<LspServers>()
        .servers
        .iter()
        .filter(|(connection, server)| {
            let current = setting_for(cx, connection);
            let saved = connections::find(cx, connection).map(|saved| &saved.settings);
            current.as_ref() != Some(&server.setting) || saved != Some(&server.connection)
        })
        .map(|(connection, _)| connection.clone())
        .collect();
    if changed.is_empty() {
        return;
    }
    cx.update_global::<LspServers, _>(|servers, _| {
        for connection in &changed {
            servers.servers.remove(connection);
            servers
                .diagnostics
                .retain(|(owner, _), _| owner != connection);
        }
    });
}

fn setting_for(cx: &App, connection: &str) -> Option<LanguageServerSetting> {
    let saved = connections::find(cx, connection)?;
    settings::get(cx)
        .language_servers
        .get(&saved.settings.driver)
        .cloned()
}

/// The running server for `connection`, starting or restarting it when due.
pub fn client(cx: &mut App, connection: &SharedString) -> Option<Arc<LspClient>> {
    let setting = setting_for(cx, connection)?;
    let state = cx
        .global::<LspServers>()
        .servers
        .get(connection)
        .map(|server| (server.status.clone(), server.client.clone()));
    match state {
        Some((ServerStatus::Running, client)) => client,
        Some((ServerStatus::Starting, _)) => None,
        Some((ServerStatus::Failed { retry_at, .. }, _)) if Instant::now() < retry_at => None,
        Some((ServerStatus::Failed { .. }, _)) | None => {
            start(cx, connection.clone(), setting);
            None
        }
    }
}

pub fn status(cx: &App, connection: &str) -> Option<(String, ServerStatus)> {
    let server = cx.global::<LspServers>().servers.get(connection)?;
    Some((server.setting.command.clone(), server.status.clone()))
}

pub fn diagnostics(cx: &App, connection: &SharedString, uri: &str) -> Vec<lsp_types::Diagnostic> {
    cx.global::<LspServers>()
        .diagnostics
        .get(&(connection.clone(), uri.to_owned()))
        .cloned()
        .unwrap_or_default()
}

/// Sends `text` to the server as the document's content, opening it the first time.
pub fn sync_document(cx: &mut App, connection: &SharedString, uri: &lsp_types::Uri, text: &str) {
    let Some(client) = client(cx, connection) else {
        return;
    };
    let key = uri.as_str().to_owned();
    let previous = cx
        .global::<LspServers>()
        .servers
        .get(connection)
        .and_then(|server| server.documents.get(&key).cloned());
    let version = match &previous {
        Some((_, sent)) if **sent == *text => return,
        Some((version, _)) => version + 1,
        None => 1,
    };
    let sent = match previous {
        None => client.notify::<lsp_types::notification::DidOpenTextDocument>(
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem::new(
                    uri.clone(),
                    "sql".to_owned(),
                    version,
                    text.to_owned(),
                ),
            },
        ),
        Some(_) => client.notify::<lsp_types::notification::DidChangeTextDocument>(
            lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier::new(
                    uri.clone(),
                    version,
                ),
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.to_owned(),
                }],
            },
        ),
    };
    if let Err(error) = sent {
        tracing::debug!(%error, "document sync");
        return;
    }
    cx.update_global::<LspServers, _>(|servers, _| {
        if let Some(server) = servers.servers.get_mut(connection) {
            server.documents.insert(key, (version, Arc::from(text)));
        }
    });
}

/// Tells the server a tab closed or moved to another connection.
pub fn close_document(cx: &mut App, connection: &SharedString, uri: &lsp_types::Uri) {
    let key = uri.as_str().to_owned();
    let client = cx.update_global::<LspServers, _>(|servers, _| {
        servers
            .diagnostics
            .remove(&(connection.clone(), key.clone()));
        let server = servers.servers.get_mut(connection)?;
        server.documents.remove(&key)?;
        server.client.clone()
    });
    let Some(client) = client else {
        return;
    };
    let closed = client.notify::<lsp_types::notification::DidCloseTextDocument>(
        lsp_types::DidCloseTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier::new(uri.clone()),
        },
    );
    if let Err(error) = closed {
        tracing::debug!(%error, "document close");
    }
}

/// Capabilities count only when present and not `false`.
pub fn can_format(capabilities: &lsp_types::ServerCapabilities) -> bool {
    matches!(
        capabilities.document_formatting_provider,
        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
    )
}

pub fn can_hover(capabilities: &lsp_types::ServerCapabilities) -> bool {
    matches!(
        capabilities.hover_provider,
        Some(
            lsp_types::HoverProviderCapability::Simple(true)
                | lsp_types::HoverProviderCapability::Options(_)
        )
    )
}

/// Backoff after the `attempts`-th failure: 2, 4, 8 ... seconds, at most a minute.
fn retry_delay(attempts: u32) -> Duration {
    Duration::from_secs(2_u64.saturating_pow(attempts.min(6)).min(60))
}

fn start(cx: &mut App, connection: SharedString, setting: LanguageServerSetting) {
    let Some(saved) = connections::find(cx, &connection).map(|saved| saved.settings.clone()) else {
        return;
    };
    let configuration = if setting.pass_connection {
        sqls_configuration(&saved)
    } else {
        None
    };
    let (notification_sender, mut notifications) = mpsc::unbounded_channel::<ServerNotification>();
    let generation = cx.update_global::<LspServers, _>(|servers, _| {
        servers.next_generation += 1;
        servers.next_generation
    });
    let listener = cx.spawn({
        let connection = connection.clone();
        async move |cx| {
            while let Some(notification) = notifications.recv().await {
                cx.update(|cx| handle_notification(cx, &connection, notification));
            }
            // Output closed: the server exited. A failed start reports its own error.
            cx.update(|cx| {
                if is_current(cx, &connection, generation, &ServerStatus::Running) {
                    mark_failed(cx, &connection, generation, "the server exited");
                }
            });
        }
    });
    let attempts = cx
        .global::<LspServers>()
        .servers
        .get(&connection)
        .map_or(0, |server| server.attempts);
    cx.update_global::<LspServers, _>(|servers, _| {
        servers.servers.insert(
            connection.clone(),
            Server {
                setting: setting.clone(),
                connection: saved,
                generation,
                status: ServerStatus::Starting,
                client: None,
                started_at: None,
                attempts,
                documents: HashMap::new(),
                _listener: Some(listener),
            },
        );
    });
    tracing::debug!(connection = %connection, command = %setting.command, "starting language server");
    let task = tokio_bridge::spawn(cx, async move {
        let mut client =
            LspClient::spawn(&setting.command, &setting.args, None, notification_sender).await?;
        if let Some(settings) = configuration {
            client.notify::<lsp_types::notification::DidChangeConfiguration>(
                lsp_types::DidChangeConfigurationParams { settings },
            )?;
        }
        // It may have exited right after `initialize`.
        if !client.is_running() {
            return Err(sql_lsp::LspError::Stopped);
        }
        Ok::<_, sql_lsp::LspError>(client)
    });
    cx.spawn(async move |cx| {
        let outcome = task.await;
        cx.update(|cx| {
            if !is_current(cx, &connection, generation, &ServerStatus::Starting) {
                return;
            }
            match outcome {
                Ok(Ok(client)) => {
                    cx.update_global::<LspServers, _>(|servers, _| {
                        if let Some(server) = servers.servers.get_mut(&connection) {
                            server.client = Some(Arc::new(client));
                            server.status = ServerStatus::Running;
                            server.started_at = Some(Instant::now());
                        }
                    });
                }
                Ok(Err(error)) => mark_failed(cx, &connection, generation, &error.to_string()),
                Err(error) => mark_failed(cx, &connection, generation, &error.to_string()),
            }
        });
    })
    .detach();
}

/// Whether `connection`'s server is still the one started as `generation`, in `status`.
fn is_current(cx: &App, connection: &SharedString, generation: u64, status: &ServerStatus) -> bool {
    cx.global::<LspServers>()
        .servers
        .get(connection)
        .is_some_and(|server| {
            server.generation == generation
                && std::mem::discriminant(&server.status) == std::mem::discriminant(status)
        })
}

fn mark_failed(cx: &mut App, connection: &SharedString, generation: u64, message: &str) {
    // The message can come from the server; it may quote the connection, so it is not logged.
    tracing::debug!(connection = %connection, "language server failed");
    cx.update_global::<LspServers, _>(|servers, _| {
        servers
            .diagnostics
            .retain(|(owner, _), _| owner != connection);
        let Some(server) = servers.servers.get_mut(connection) else {
            return;
        };
        if server.generation != generation {
            return;
        }
        if server
            .started_at
            .is_some_and(|started| started.elapsed() >= STABLE_AFTER)
        {
            server.attempts = 0;
        }
        server.attempts += 1;
        server.client = None;
        server.started_at = None;
        server.documents.clear();
        server.status = ServerStatus::Failed {
            message: message.to_owned(),
            retry_at: Instant::now() + retry_delay(server.attempts),
        };
    });
}

fn handle_notification(cx: &mut App, connection: &SharedString, notification: ServerNotification) {
    if notification.method != "textDocument/publishDiagnostics" {
        return;
    }
    let Ok(params) =
        serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(notification.params)
    else {
        return;
    };
    cx.update_global::<LspServers, _>(|servers, _| {
        servers.diagnostics.insert(
            (connection.clone(), params.uri.as_str().to_owned()),
            params.diagnostics,
        );
    });
}

/// `sqls` settings with the connection, sent as `workspace/didChangeConfiguration`.
/// `None` for connections sqls cannot open (Fabric, Snowflake, Entra sign-in).
fn sqls_configuration(settings: &ConnectionSettings) -> Option<Value> {
    let (driver, data_source) = match (&settings.driver, &settings.authentication) {
        (DriverKind::Postgres, Authentication::Password { user, password }) => (
            "postgresql",
            format!(
                "host={} port={} user={} password={} dbname={} sslmode={}",
                libpq_value(&settings.server),
                settings.port.unwrap_or(5432),
                libpq_value(user),
                libpq_value(password),
                libpq_value(&settings.database),
                if settings.encryption == sql_db::Encryption::Optional {
                    "prefer"
                } else {
                    "require"
                },
            ),
        ),
        (DriverKind::Sqlite, _) => ("sqlite3", settings.server.clone()),
        (DriverKind::SqlServer, Authentication::Password { user, password }) => {
            // `host\instance` becomes `host/instance`, as the Go driver expects.
            let (host, instance) = settings
                .server
                .split_once('\\')
                .map_or((settings.server.as_str(), None), |(host, instance)| {
                    (host, Some(instance))
                });
            let location = match (instance, settings.port) {
                (_, Some(port)) => format!("{}:{port}", percent_encode(host)),
                (Some(instance), None) => {
                    format!("{}/{}", percent_encode(host), percent_encode(instance))
                }
                (None, None) => format!("{}:1433", percent_encode(host)),
            };
            (
                "mssql",
                format!(
                    "sqlserver://{}:{}@{location}?database={}",
                    percent_encode(user),
                    percent_encode(password),
                    percent_encode(&settings.database),
                ),
            )
        }
        (
            DriverKind::Postgres | DriverKind::SqlServer,
            Authentication::AzureCli
            | Authentication::EntraInteractive { .. }
            | Authentication::Integrated
            | Authentication::ServicePrincipal { .. }
            | Authentication::None
            | Authentication::KeyPair { .. },
        )
        | (DriverKind::Fabric | DriverKind::Snowflake, _) => return None,
    };
    Some(json!({
        "sqls": {
            "connections": [{ "driver": driver, "dataSourceName": data_source }]
        }
    }))
}

/// A libpq `key=value` value, quoted when it holds spaces or quotes.
fn libpq_value(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\'', '\\']) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_grows_and_caps() {
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(3), Duration::from_secs(8));
        assert_eq!(retry_delay(40), Duration::from_secs(60));
    }

    #[test]
    fn builds_sqls_connections() {
        let settings = ConnectionSettings {
            server: "db".to_owned(),
            database: "shop".to_owned(),
            authentication: Authentication::Password {
                user: "app".to_owned(),
                password: "p w'd".to_owned(),
            },
            ..ConnectionSettings::new(DriverKind::Postgres)
        };
        let value = sqls_configuration(&settings).unwrap();
        let source = value["sqls"]["connections"][0]["dataSourceName"]
            .as_str()
            .unwrap();
        assert!(source.contains(r"password='p w\'d'"), "{source}");
        let fabric = ConnectionSettings::new(DriverKind::Fabric);
        assert!(sqls_configuration(&fabric).is_none());
        assert_eq!(percent_encode("a@b c"), "a%40b%20c");

        let instance = ConnectionSettings {
            server: r"db1\SQLEXPRESS".to_owned(),
            database: "shop".to_owned(),
            authentication: Authentication::Password {
                user: "sa".to_owned(),
                password: "x".to_owned(),
            },
            ..ConnectionSettings::new(DriverKind::SqlServer)
        };
        let value = sqls_configuration(&instance).unwrap();
        assert_eq!(
            value["sqls"]["connections"][0]["dataSourceName"],
            "sqlserver://sa:x@db1/SQLEXPRESS?database=shop"
        );
    }
}
