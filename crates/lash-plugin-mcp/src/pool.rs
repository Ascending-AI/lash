//! Per-core MCP connection pool.
//!
//! [`McpConnectionPool`] holds one client per configured server and is shared
//! across every session built from the same [`lash_core::LashCore`]. The pool
//! attempts to connect each server eagerly when constructed, but a server
//! that is down never fails construction: the entry stays registered and a
//! background task retries with exponential backoff until it connects (or the
//! server is detached). A connection that dies mid-life is detected on the
//! next tool call and re-established the same way. Imported tool definitions
//! are kept across a disconnect so the tool catalog stays stable; calls to a
//! disconnected server fail loudly instead.
//!
//! The wire-level transport is provided by the official [`rmcp`] SDK.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use http::{HeaderName, HeaderValue};
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequestParams, ClientInfo, Content, Implementation, RawContent, ResourceContents,
};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

use lash_core::{
    AttachmentCreateMeta, MediaType, ToolCallOutput, ToolContext, ToolDefinition, ToolFailure,
    ToolFailureClass, ToolFailureSource, ToolResult, ToolRetryDisposition, ToolValue,
};
use lash_tool_support::ToolDefinitionLashlangExt;

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::naming;

const RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Shared, per-core connection pool. Wrapped in `Arc` and cloned into each
/// session plugin instance.
pub struct McpConnectionPool {
    entries: RwLock<BTreeMap<String, Arc<McpEntry>>>,
}

/// Connection status of one configured server, for host/UI observability.
#[derive(Clone, Debug)]
pub struct McpServerStatus {
    pub server_name: String,
    pub connected: bool,
    /// Most recent connection error; cleared when a connect succeeds.
    pub last_error: Option<String>,
    /// Number of tools imported from the server's last successful discovery.
    pub tool_count: usize,
}

struct McpEntry {
    server_name: String,
    config: McpServerConfig,
    /// `None` while disconnected. Once connected we keep the running service
    /// handle alive; the transport owns its own process internally.
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
    /// Cached, prefixed tool definitions for this server, refreshed on every
    /// successful (re)connect and kept across a disconnect so the tool
    /// surface stays stable. Keys are the prefixed names
    /// (`mcp__<server>__<tool>`).
    imported_tools: RwLock<BTreeMap<String, ImportedTool>>,
    connected: AtomicBool,
    last_error: RwLock<Option<String>>,
    /// Set on detach/shutdown; stops any background reconnect loop.
    cancelled: AtomicBool,
    /// Guards against spawning concurrent reconnect loops for one entry.
    connecting: AtomicBool,
}

#[derive(Clone)]
struct ImportedTool {
    /// The native MCP tool name as advertised by the server (before
    /// prefixing/normalisation).
    original_name: String,
    definition: ToolDefinition,
}

impl McpConnectionPool {
    /// Construct an empty pool.
    pub fn empty() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    /// Build a pool for the configured servers. Each server is tried eagerly
    /// in turn so tools are available immediately when servers are up, but a
    /// connection failure never aborts construction: the entry stays
    /// registered and reconnects in the background. Only configuration errors
    /// (a misconfigured server, not an outage) fail the build.
    pub async fn connect(
        servers: BTreeMap<String, McpServerConfig>,
    ) -> Result<Arc<Self>, McpError> {
        let pool = Arc::new(Self::empty());
        for (name, config) in servers {
            config.validate(&name)?;
            let entry = Arc::new(McpEntry::new(name.clone(), config));
            pool.install(name.clone(), Arc::clone(&entry));
            if let Err(err) = entry.establish().await {
                tracing::warn!(
                    server = %name,
                    error = %err,
                    "MCP server unavailable at startup; retrying in the background"
                );
                entry.spawn_reconnect_loop();
            }
        }
        Ok(pool)
    }

    /// Add (or replace) one server in the pool. Connects eagerly and returns
    /// the definitive result — use this for interactive attach, where the
    /// caller wants to know whether the server is reachable.
    pub async fn attach(
        self: &Arc<Self>,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<(), McpError> {
        config.validate(&server_name)?;
        let entry = Arc::new(McpEntry::new(server_name.clone(), config));
        entry.establish().await?;
        self.install(server_name, entry);
        Ok(())
    }

    /// Remove and shut down one server.
    pub async fn detach(self: &Arc<Self>, server_name: &str) -> Result<(), McpError> {
        let removed = {
            let mut entries = self
                .entries
                .write()
                .expect("MCP pool entries lock poisoned");
            entries.remove(server_name)
        };
        if let Some(entry) = removed {
            entry.cancelled.store(true, Ordering::SeqCst);
            entry.shutdown().await;
        }
        Ok(())
    }

    /// Register an entry, shutting down any previous entry under the name.
    fn install(&self, server_name: String, entry: Arc<McpEntry>) {
        let previous = {
            let mut entries = self
                .entries
                .write()
                .expect("MCP pool entries lock poisoned");
            entries.insert(server_name, entry)
        };
        if let Some(previous) = previous {
            previous.cancelled.store(true, Ordering::SeqCst);
            tokio::spawn(async move { previous.shutdown().await });
        }
    }

    /// Connection status of every configured server.
    pub fn server_statuses(&self) -> Vec<McpServerStatus> {
        let guard = self.entries.read().expect("MCP pool entries lock poisoned");
        guard
            .values()
            .map(|entry| McpServerStatus {
                server_name: entry.server_name.clone(),
                connected: entry.connected.load(Ordering::SeqCst),
                last_error: entry
                    .last_error
                    .read()
                    .expect("MCP entry error lock poisoned")
                    .clone(),
                tool_count: entry
                    .imported_tools
                    .read()
                    .expect("MCP entry tools lock poisoned")
                    .len(),
            })
            .collect()
    }

    /// All advertised tools across every server, with `mcp__<server>__<tool>`
    /// prefixed names. Cheap — these are precomputed `ToolDefinition` clones.
    /// Includes tools of currently disconnected servers (last successful
    /// discovery) so the tool catalog stays stable across an outage.
    pub fn advertised_tools(&self) -> Vec<ToolDefinition> {
        let guard = self.entries.read().expect("MCP pool entries lock poisoned");
        guard
            .values()
            .flat_map(|entry| {
                entry
                    .imported_tools
                    .read()
                    .expect("MCP entry tools lock poisoned")
                    .values()
                    .map(|tool| tool.definition.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Route a prefixed tool call (`mcp__<server>__<tool>`) to the appropriate
    /// server and translate its result back to `ToolResult`.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        args: &Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let (entry, original_name) = match self.lookup(prefixed_name).await {
            Some(found) => found,
            None => {
                return ToolResult::err_fmt(format!("Unknown MCP tool: {prefixed_name}"));
            }
        };

        let call_timeout = entry.config.call_timeout();
        let server_name = entry.server_name.clone();
        let arguments = match args {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            other => {
                return ToolResult::err_fmt(format!(
                    "MCP tool `{prefixed_name}` expected an object argument, got {}",
                    other
                ));
            }
        };

        // Clone the peer handle while briefly holding the lock, then release it
        // before issuing the request. `rmcp::Peer` is a cheap, cloneable handle
        // (an mpsc sender plus an internal request-id provider) that supports
        // concurrent in-flight requests, so holding the mutex across the network
        // await would needlessly serialize tool calls to the same server and
        // risk a guard held across `.await`.
        let peer = {
            let service_guard = entry.service.lock().await;
            match service_guard.as_ref() {
                Some(service) => service.peer().clone(),
                None => {
                    let last_error = entry
                        .last_error
                        .read()
                        .expect("MCP entry error lock poisoned")
                        .clone();
                    return ToolResult::err_fmt(McpError::Protocol(match last_error {
                        Some(last_error) => format!(
                            "MCP server `{server_name}` is not connected \
                             (reconnecting in the background; last error: {last_error})"
                        ),
                        None => format!("MCP server `{server_name}` is not connected"),
                    }));
                }
            }
        };

        let response = timeout(call_timeout, async {
            let mut params = CallToolRequestParams::default();
            params.name = original_name.clone().into();
            params.arguments = arguments;
            peer.call_tool(params).await
        })
        .await;

        match response {
            Ok(Ok(result)) => {
                tool_result_from_rmcp(result, context, entry.config.binary_content_attachments())
                    .await
            }
            Ok(Err(err)) => {
                if is_connection_loss(&err) {
                    entry.mark_disconnected().await;
                    return ToolResult::err_fmt(McpError::Protocol(format!(
                        "MCP server `{server_name}` connection lost: {err}; \
                         reconnecting in the background"
                    )));
                }
                ToolResult::err_fmt(McpError::Protocol(err.to_string()))
            }
            Err(_) => ToolResult::err_fmt(McpError::CallTimeout {
                server: server_name,
                timeout_ms: call_timeout.as_millis() as u64,
            }),
        }
    }

    async fn lookup(&self, prefixed_name: &str) -> Option<(Arc<McpEntry>, String)> {
        let guard = self.entries.read().expect("MCP pool entries lock poisoned");
        for entry in guard.values() {
            let original_name = entry
                .imported_tools
                .read()
                .expect("MCP entry tools lock poisoned")
                .get(prefixed_name)
                .map(|tool| tool.original_name.clone());
            if let Some(original_name) = original_name {
                return Some((Arc::clone(entry), original_name));
            }
        }
        None
    }

    /// Tear down all connections. Call this before dropping the pool for a
    /// graceful shutdown; `Drop` itself cannot await.
    pub async fn shutdown_all(&self) {
        let entries: Vec<Arc<McpEntry>> = {
            let mut guard = self
                .entries
                .write()
                .expect("MCP pool entries lock poisoned");
            std::mem::take(&mut *guard).into_values().collect()
        };
        for entry in entries {
            entry.cancelled.store(true, Ordering::SeqCst);
            entry.shutdown().await;
        }
    }
}

/// Transport-level failures mean the connection is gone (dead child process,
/// closed HTTP stream) — reconnect. Protocol-level errors (a tool failing,
/// an unexpected response) leave the connection usable.
fn is_connection_loss(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportSend(_)
            | ServiceError::TransportClosed
            | ServiceError::Cancelled { .. }
    )
}

impl McpEntry {
    fn new(server_name: String, config: McpServerConfig) -> Self {
        Self {
            server_name,
            config,
            service: tokio::sync::Mutex::new(None),
            imported_tools: RwLock::new(BTreeMap::new()),
            connected: AtomicBool::new(false),
            last_error: RwLock::new(None),
            cancelled: AtomicBool::new(false),
            connecting: AtomicBool::new(false),
        }
    }

    /// One connection attempt: handshake, tool discovery, then swap in the
    /// fresh service and definitions. Records the error on failure so status
    /// and call-time messages can report it.
    async fn establish(&self) -> Result<(), McpError> {
        match self.try_connect().await {
            Ok(()) => Ok(()),
            Err(err) => {
                *self
                    .last_error
                    .write()
                    .expect("MCP entry error lock poisoned") = Some(err.to_string());
                Err(err)
            }
        }
    }

    async fn try_connect(&self) -> Result<(), McpError> {
        let service = timeout(
            self.config.startup_timeout(),
            connect_service(&self.server_name, &self.config),
        )
        .await
        .map_err(|_| McpError::StartupTimeout {
            server: self.server_name.clone(),
            timeout_ms: self.config.startup_timeout().as_millis() as u64,
        })??;

        // Bound the discovery call so a server that completes the handshake but
        // then hangs on `tools/list` surfaces a timeout instead of blocking the
        // connect attempt indefinitely. Discovery happens during startup, so
        // the startup budget is the natural bound.
        let discovery_timeout = self.config.startup_timeout();
        let tools = timeout(discovery_timeout, service.peer().list_all_tools())
            .await
            .map_err(|_| McpError::StartupTimeout {
                server: self.server_name.clone(),
                timeout_ms: discovery_timeout.as_millis() as u64,
            })?
            .map_err(|err| McpError::Protocol(format!("list_tools failed: {err}")))?;

        *self
            .imported_tools
            .write()
            .expect("MCP entry tools lock poisoned") = import_tools(&self.server_name, tools);
        *self.service.lock().await = Some(service);
        self.connected.store(true, Ordering::SeqCst);
        *self
            .last_error
            .write()
            .expect("MCP entry error lock poisoned") = None;
        Ok(())
    }

    /// Retry [`establish`](Self::establish) with exponential backoff until it
    /// succeeds or the entry is detached. At most one loop runs per entry.
    fn spawn_reconnect_loop(self: &Arc<Self>) {
        if self.connecting.swap(true, Ordering::SeqCst) {
            return;
        }
        let entry = Arc::clone(self);
        tokio::spawn(async move {
            let mut backoff = RECONNECT_INITIAL_BACKOFF;
            loop {
                tokio::time::sleep(backoff).await;
                if entry.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                match entry.establish().await {
                    Ok(()) => {
                        tracing::info!(server = %entry.server_name, "MCP server reconnected");
                        break;
                    }
                    Err(err) => {
                        tracing::warn!(
                            server = %entry.server_name,
                            error = %err,
                            "MCP reconnect attempt failed"
                        );
                    }
                }
                backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
            }
            entry.connecting.store(false, Ordering::SeqCst);
        });
    }

    /// Drop the dead service and start reconnecting in the background. The
    /// imported tool definitions are kept so the tool catalog stays stable.
    async fn mark_disconnected(self: &Arc<Self>) {
        self.connected.store(false, Ordering::SeqCst);
        let service = self.service.lock().await.take();
        if let Some(service) = service {
            let _ = service.cancel().await;
        }
        if !self.cancelled.load(Ordering::SeqCst) {
            self.spawn_reconnect_loop();
        }
    }

    async fn shutdown(&self) {
        self.connected.store(false, Ordering::SeqCst);
        let mut guard = self.service.lock().await;
        if let Some(service) = guard.take() {
            // `cancel` consumes the service and waits for the transport
            // task to finish. Errors only surface if the transport already
            // shut down; ignore them.
            let _ = service.cancel().await;
        }
    }
}

async fn connect_service(
    server_name: &str,
    config: &McpServerConfig,
) -> Result<RunningService<RoleClient, ClientInfo>, McpError> {
    let mut implementation = Implementation::default();
    implementation.name = "lash".to_string();
    implementation.version = env!("CARGO_PKG_VERSION").to_string();
    let mut client_info = ClientInfo::default();
    client_info.client_info = implementation;

    match config {
        McpServerConfig::Stdio {
            command,
            args,
            env,
            cwd,
            ..
        } => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            if let Some(cwd) = cwd {
                cmd.current_dir(cwd);
            }
            for (key, value) in env {
                cmd.env(key, value);
            }
            let transport = TokioChildProcess::new(cmd).map_err(|err| {
                McpError::Protocol(format!(
                    "failed to spawn `{command}` for `{server_name}`: {err}"
                ))
            })?;
            client_info.serve(transport).await.map_err(|err| {
                McpError::Protocol(format!("MCP handshake with `{server_name}`: {err}"))
            })
        }
        McpServerConfig::StreamableHttp { url, headers, .. } => {
            let custom_headers = build_http_headers(server_name, headers)?;
            let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .custom_headers(custom_headers);
            let transport = StreamableHttpClientTransport::from_config(config);
            client_info.serve(transport).await.map_err(|err| {
                McpError::Protocol(format!("MCP handshake with `{server_name}`: {err}"))
            })
        }
    }
}

/// Translate a config `headers` map into the `http` header types `rmcp`'s
/// streamable-HTTP transport expects, failing with a clear config error on a
/// malformed name or value. Header names are case-insensitive per HTTP, so a
/// configured `Authorization` reaches the server as `authorization`.
fn build_http_headers(
    server_name: &str,
    headers: &BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, McpError> {
    let mut out = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header_name = HeaderName::try_from(name.as_str()).map_err(|err| {
            McpError::Config(format!(
                "MCP server `{server_name}` has invalid HTTP header name `{name}`: {err}"
            ))
        })?;
        let header_value = HeaderValue::try_from(value.as_str()).map_err(|err| {
            McpError::Config(format!(
                "MCP server `{server_name}` has invalid value for HTTP header `{name}`: {err}"
            ))
        })?;
        out.insert(header_name, header_value);
    }
    Ok(out)
}

fn import_tools(
    server_name: &str,
    tools: Vec<rmcp::model::Tool>,
) -> BTreeMap<String, ImportedTool> {
    let mut used_names = BTreeSet::new();
    let mut imported = BTreeMap::new();
    for tool in tools {
        let original_name = tool.name.to_string();
        let description = tool
            .description
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        let input_schema = Value::Object((*tool.input_schema).clone());
        let output_schema = tool
            .output_schema
            .as_ref()
            .map(|s| Value::Object((**s).clone()))
            .unwrap_or_else(|| json!({}));
        let (prefixed, lashlang_binding) =
            naming::build_prefixed_name(server_name, &original_name, &mut used_names);

        let description = if description.is_empty() {
            format!("MCP tool from server `{server_name}`")
        } else {
            format!("[MCP {server_name}] {description}")
        };

        imported.insert(
            prefixed.clone(),
            ImportedTool {
                original_name,
                definition: ToolDefinition::raw(
                    format!("mcp:{server_name}/{prefixed}"),
                    prefixed,
                    description,
                    input_schema,
                    output_schema,
                )
                .with_lashlang_binding(lashlang_binding),
            },
        );
    }
    imported
}

async fn tool_result_from_rmcp(
    result: rmcp::model::CallToolResult,
    context: &ToolContext<'_>,
    binary_content_attachments: bool,
) -> ToolResult {
    let is_error = result.is_error.unwrap_or(false);

    let mut text_parts = Vec::new();
    let mut content_items: Vec<ToolValue> = Vec::new();
    let mut has_attachments = false;

    for Content { raw, .. } in result.content {
        match raw {
            RawContent::Text(text) => {
                text_parts.push(text.text.clone());
                content_items.push(ToolValue::String(text.text));
            }
            RawContent::Image(image) => {
                let reference =
                    match store_mcp_attachment(context, &image.data, &image.mime_type, "MCP image")
                        .await
                    {
                        Ok(reference) => reference,
                        Err(result) => return result,
                    };
                has_attachments = true;
                content_items.push(ToolValue::Attachment(lash_core::AttachmentSource::stored(
                    reference,
                )));
            }
            RawContent::Audio(audio) if binary_content_attachments => {
                let reference =
                    match store_mcp_attachment(context, &audio.data, &audio.mime_type, "MCP audio")
                        .await
                    {
                        Ok(reference) => reference,
                        Err(result) => return result,
                    };
                has_attachments = true;
                content_items.push(ToolValue::Attachment(lash_core::AttachmentSource::stored(
                    reference,
                )));
            }
            RawContent::Resource(resource) if binary_content_attachments => {
                match resource.resource {
                    ResourceContents::BlobResourceContents {
                        uri,
                        mime_type,
                        blob,
                        ..
                    } => {
                        let Some(mime_type) = mime_type else {
                            return ToolResult::err_fmt(
                                "MCP binary resource attachment is missing its MIME type",
                            );
                        };
                        let reference = match store_mcp_attachment(
                            context,
                            &blob,
                            &mime_type,
                            &format!("MCP resource {uri}"),
                        )
                        .await
                        {
                            Ok(reference) => reference,
                            Err(result) => return result,
                        };
                        has_attachments = true;
                        content_items.push(ToolValue::Attachment(
                            lash_core::AttachmentSource::stored(reference),
                        ));
                    }
                    text_resource => {
                        if let Ok(value) = serde_json::to_value(text_resource) {
                            content_items.push(ToolValue::from(value));
                        }
                    }
                }
            }
            other => {
                if let Ok(value) = serde_json::to_value(&other) {
                    content_items.push(ToolValue::from(value));
                }
            }
        }
    }

    let value = if let Some(structured) = result.structured_content {
        if !has_attachments {
            ToolValue::from(structured)
        } else {
            ToolValue::Object(
                [
                    ("structured".to_string(), ToolValue::from(structured)),
                    ("content".to_string(), ToolValue::Array(content_items)),
                ]
                .into_iter()
                .collect(),
            )
        }
    } else if content_items.is_empty() {
        ToolValue::Null
    } else if content_items.len() == 1 {
        content_items.into_iter().next().unwrap_or(ToolValue::Null)
    } else {
        ToolValue::Array(content_items)
    };
    if is_error {
        ToolResult::from_output(ToolCallOutput::failure(ToolFailure {
            class: ToolFailureClass::Execution,
            code: "mcp_tool_error".into(),
            message: if text_parts.is_empty() {
                "MCP tool returned an error".into()
            } else {
                text_parts.join("\n\n")
            },
            source: ToolFailureSource::Tool,
            retry: ToolRetryDisposition::Never,
            raw: Some(value),
        }))
    } else {
        ToolResult::from_output(ToolCallOutput::success(value))
    }
}

async fn store_mcp_attachment(
    context: &ToolContext<'_>,
    encoded: &str,
    mime_type: &str,
    label: &str,
) -> Result<lash_core::AttachmentRef, ToolResult> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| ToolResult::err_fmt(McpError::Decode(err)))?;
    let media_type = MediaType::parse(mime_type)
        .map_err(|err| ToolResult::err_fmt(format_args!("Invalid MCP attachment MIME: {err}")))?;
    context
        .attachments()
        .put(
            data,
            AttachmentCreateMeta::new(media_type, None, Some(label.to_string())),
        )
        .await
        .map_err(|err| ToolResult::err_fmt(format_args!("Failed to store MCP attachment: {err}")))
}

impl Drop for McpConnectionPool {
    fn drop(&mut self) {
        // We can't .await in Drop. The RunningService values inside each
        // entry will cancel their processes when they're dropped
        // (rmcp drops the transport, which kills the child process or
        // closes the HTTP connection). For a graceful shutdown, callers
        // should call `shutdown_all` themselves.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the header-drop bug: custom/auth headers configured for
    /// an HTTP MCP server must be translated into the `http` header types the
    /// transport actually sends. Before the fix, `connect_service` called
    /// `from_uri` and dropped the configured `headers` map entirely, so an
    /// `Authorization` header never reached the server.
    #[test]
    fn build_http_headers_carries_configured_headers() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer secret-token".to_string(),
        );
        headers.insert("X-Tenant".to_string(), "acme".to_string());

        let built = build_http_headers("api", &headers).expect("valid headers convert");

        assert_eq!(
            built
                .get(&HeaderName::from_static("authorization"))
                .map(|v| v.to_str().unwrap()),
            Some("Bearer secret-token"),
            "configured Authorization header must be carried through to the transport"
        );
        assert_eq!(
            built
                .get(&HeaderName::from_static("x-tenant"))
                .map(|v| v.to_str().unwrap()),
            Some("acme")
        );
        assert_eq!(built.len(), 2);
    }

    #[test]
    fn build_http_headers_empty_map_is_empty() {
        let built = build_http_headers("api", &BTreeMap::new()).expect("empty converts");
        assert!(built.is_empty());
    }

    #[test]
    fn build_http_headers_rejects_malformed_name() {
        let mut headers = BTreeMap::new();
        headers.insert("Bad Header Name".to_string(), "x".to_string());
        let err = build_http_headers("api", &headers).expect_err("malformed name rejected");
        assert!(
            matches!(err, McpError::Config(_)),
            "expected a config error, got {err:?}"
        );
    }

    #[test]
    fn build_http_headers_rejects_malformed_value() {
        let mut headers = BTreeMap::new();
        // A newline is not a legal header value byte.
        headers.insert("X-Bad".to_string(), "line1\nline2".to_string());
        let err = build_http_headers("api", &headers).expect_err("malformed value rejected");
        assert!(
            matches!(err, McpError::Config(_)),
            "expected a config error, got {err:?}"
        );
    }

    /// A server that is down at startup must not fail pool construction: the
    /// entry stays registered (status: disconnected, with the error recorded)
    /// and only configuration errors abort.
    #[tokio::test]
    async fn connect_tolerates_unreachable_server() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "down".to_string(),
            McpServerConfig::Stdio {
                // Spawns, says nothing, exits — the handshake fails fast.
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "exit 1".to_string()],
                env: BTreeMap::new(),
                cwd: None,
                startup_timeout_ms: 1_000,
                call_timeout_ms: 1_000,
                binary_content_attachments: false,
            },
        );

        let pool = McpConnectionPool::connect(servers)
            .await
            .expect("an unreachable server must not fail pool construction");

        assert!(pool.advertised_tools().is_empty());
        let statuses = pool.server_statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].server_name, "down");
        assert!(!statuses[0].connected);
        assert!(
            statuses[0].last_error.is_some(),
            "the connection failure is recorded for observability"
        );

        let result = pool
            .call_tool(
                "mcp__down__anything",
                &json!({}),
                &lash_core::testing::mock_tool_context(),
            )
            .await;
        assert!(!result.is_success(), "calls fail loudly while disconnected");

        pool.shutdown_all().await;
    }

    /// A connection that dies mid-life is detected on the next call and
    /// re-established by the background reconnect loop; tool definitions are
    /// kept across the outage so the surface stays stable.
    #[tokio::test]
    async fn pool_reconnects_after_transport_death() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "demo", "version": "1.0.0" }
            }
        });
        let list = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "ping",
                    "description": "Ping",
                    "inputSchema": { "type": "object", "properties": {} }
                }]
            }
        });
        let call = json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "pong" }] } });

        // Serve initialize, tools/list, and exactly one tools/call, then exit:
        // the transport dies after the first successful call. Every reconnect
        // runs the same script again (rmcp request ids restart per connection).
        let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; printf '%s\\n' \"$RESP3\""
            .to_string();

        let mut env = BTreeMap::new();
        env.insert("RESP1".to_string(), initialize.to_string());
        env.insert("RESP2".to_string(), list.to_string());
        env.insert("RESP3".to_string(), call.to_string());

        let mut servers = BTreeMap::new();
        servers.insert(
            "flaky".to_string(),
            McpServerConfig::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env,
                cwd: None,
                startup_timeout_ms: 10_000,
                call_timeout_ms: 2_000,
                binary_content_attachments: false,
            },
        );

        let pool = McpConnectionPool::connect(servers)
            .await
            .expect("connects to the mock");
        let ctx = lash_core::testing::mock_tool_context();
        let args = json!({});

        let first = pool.call_tool("mcp__flaky__ping", &args, &ctx).await;
        assert!(first.is_success(), "first call succeeds: {first:?}");

        // The mock exited after the first call. Definitions must survive the
        // outage, and calls must fail until the reconnect loop brings the
        // (respawned) server back, after which calls succeed again.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut recovered = false;
        while std::time::Instant::now() < deadline {
            assert_eq!(
                pool.advertised_tools().len(),
                1,
                "tool definitions are kept across a disconnect"
            );
            let result = pool.call_tool("mcp__flaky__ping", &args, &ctx).await;
            if result.is_success() {
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(recovered, "pool must reconnect after the transport died");

        pool.shutdown_all().await;
    }

    /// Regression for the missing discovery timeout: a server that completes
    /// the handshake but then hangs on `tools/list` must surface a
    /// `StartupTimeout` rather than blocking `connect` forever.
    #[tokio::test]
    async fn discovery_hang_surfaces_startup_timeout() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "demo", "version": "1.0.0" }
            }
        });

        // Respond to `initialize`, swallow `notifications/initialized`, read the
        // `tools/list` request line, then hang (never respond) by blocking on
        // stdin. The short startup timeout must trip.
        let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; \
            cat >/dev/null"
            .to_string();

        let mut env = BTreeMap::new();
        env.insert("RESP1".to_string(), initialize.to_string());

        let config = McpServerConfig::Stdio {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env,
            cwd: None,
            startup_timeout_ms: 750,
            call_timeout_ms: 10_000,
            binary_content_attachments: false,
        };

        let entry = McpEntry::new("hangs".to_string(), config);
        match entry.establish().await {
            Err(McpError::StartupTimeout { .. }) => {}
            Err(other) => panic!("expected StartupTimeout from a hung tools/list, got {other:?}"),
            Ok(_) => panic!("a hung tools/list must not connect"),
        }
        assert!(!entry.connected.load(Ordering::SeqCst));
        assert!(
            entry
                .last_error
                .read()
                .expect("error lock")
                .as_deref()
                .is_some_and(|err| err.contains("timed out") || err.contains("timeout")),
            "the failure is recorded for status reporting"
        );
    }

    /// Regression for the service mutex held across the network await: two
    /// concurrent `tools/call` requests to the same server must be able to be
    /// in flight at once. The mock refuses to answer the first call until it
    /// has read the second request line, so a serializing implementation (lock
    /// held across `.await`) would deadlock and time out, while the concurrent
    /// implementation completes both calls.
    #[tokio::test]
    async fn concurrent_calls_are_not_serialized_by_the_service_mutex() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "demo", "version": "1.0.0" }
            }
        });
        let list = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "ping",
                    "description": "Ping",
                    "inputSchema": { "type": "object", "properties": {} }
                }]
            }
        });
        // rmcp assigns request ids 2 and 3 to the two concurrent calls. The
        // mock reads BOTH request lines before emitting EITHER response, which
        // is only possible if both requests are in flight concurrently.
        let call2 = json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "pong" }] } });
        let call3 = json!({ "jsonrpc": "2.0", "id": 3, "result": { "content": [{ "type": "text", "text": "pong" }] } });

        let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; \
            read -r _; \
            printf '%s\\n' \"$RESP3\"; \
            printf '%s\\n' \"$RESP4\"; \
            cat >/dev/null"
            .to_string();

        let mut env = BTreeMap::new();
        env.insert("RESP1".to_string(), initialize.to_string());
        env.insert("RESP2".to_string(), list.to_string());
        env.insert("RESP3".to_string(), call2.to_string());
        env.insert("RESP4".to_string(), call3.to_string());

        let mut servers = BTreeMap::new();
        servers.insert(
            "svc".to_string(),
            McpServerConfig::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env,
                cwd: None,
                startup_timeout_ms: 10_000,
                call_timeout_ms: 5_000,
                binary_content_attachments: false,
            },
        );

        let pool = McpConnectionPool::connect(servers)
            .await
            .expect("connects to concurrency mock");

        let ctx = lash_core::testing::mock_tool_context();
        let args = json!({});
        let (a, b) = tokio::join!(
            pool.call_tool("mcp__svc__ping", &args, &ctx),
            pool.call_tool("mcp__svc__ping", &args, &ctx),
        );
        assert!(a.is_success(), "first concurrent call failed: {a:?}");
        assert!(b.is_success(), "second concurrent call failed: {b:?}");

        pool.shutdown_all().await;
    }
}
