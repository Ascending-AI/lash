//! Host-owned handlers for MCP server-to-client requests.
//!
//! Lash only provides protocol routing. Whether a server may sample a model,
//! how elicitation is presented, and which workspace roots are visible are all
//! decisions made by the embedding host through these dyn-compatible seams.
//! The seams take and return Lash-owned values ([`protocol`]); the SDK model
//! is converted and validated here, at the wire.

#![allow(
    deprecated,
    reason = "FIG-6177 retains existing sampling and roots during the MCP deprecation window"
)]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestParams, CreateMessageResult,
    ElicitResult, ElicitationCapability, ErrorCode, ErrorData, Implementation, ListRootsResult,
    ProtocolVersion, RootsCapabilities, SamplingCapability,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub mod protocol;

use protocol::{
    McpElicitationModes, McpElicitationResult, McpHostRefusal, McpRoot, McpSamplingParams,
    McpSamplingResult,
};

/// The sole supported MCP revision. Startup uses `server/discover`; requests
/// carry their protocol revision, client identity and capabilities in `_meta`.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// [`MCP_PROTOCOL_VERSION`] as the SDK negotiates it.
pub(crate) const PROTOCOL_REVISION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// Per-request MCP host context.
///
/// Fields are sealed so Lash can extend the context without breaking handler
/// implementations. A handler must stop interactive or model work when
/// [`cancellation_token`](Self::cancellation_token) fires.
pub struct McpRequestContext {
    server_name: String,
    cancellation_token: CancellationToken,
}

impl McpRequestContext {
    fn new(server_name: String, cancellation_token: CancellationToken) -> Self {
        Self {
            server_name,
            cancellation_token,
        }
    }

    /// Configured name of the MCP server that issued this request.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Token cancelled by an MCP `notifications/cancelled` message or service
    /// shutdown.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }
}

pub struct McpSamplingRequest<'a> {
    /// Protocol request payload, validated against SEP-1577.
    pub params: &'a McpSamplingParams,
    /// Sealed request context supplied by Lash.
    pub context: &'a McpRequestContext,
}

/// Elicitation payload for MCP 2026-07-28. URL interactions resume by retrying
/// the originating request with its opaque requestState, with no completion ID.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
#[non_exhaustive]
pub enum McpElicitationParams {
    Form {
        message: String,
        #[serde(rename = "requestedSchema")]
        requested_schema: Value,
    },
    Url {
        message: String,
        url: String,
    },
}

impl<'de> serde::Deserialize<'de> for McpElicitationParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            mode: Option<String>,
            message: String,
            requested_schema: Option<Value>,
            url: Option<String>,
        }
        let payload = Payload::deserialize(deserializer)?;
        // The current specification defaults an omitted mode to form, even
        // when only 2026-07-28 is supported. Both spellings use this decoder.
        match payload.mode.as_deref().unwrap_or("form") {
            "form" => Ok(Self::Form {
                message: payload.message,
                requested_schema: payload
                    .requested_schema
                    .ok_or_else(|| serde::de::Error::missing_field("requestedSchema"))?,
            }),
            "url" => Ok(Self::Url {
                message: payload.message,
                url: payload
                    .url
                    .ok_or_else(|| serde::de::Error::missing_field("url"))?,
            }),
            mode => Err(serde::de::Error::unknown_variant(mode, &["form", "url"])),
        }
    }
}

pub struct McpElicitationRequest<'a> {
    /// Protocol request payload.
    pub params: &'a McpElicitationParams,
    /// Sealed request context supplied by Lash.
    pub context: &'a McpRequestContext,
    validator: Option<&'a jsonschema::Validator>,
}

impl McpElicitationRequest<'_> {
    /// Lash repeats this validation at the wire boundary. Calling it here
    /// gives an interactive host a typed error it can use to re-prompt rather
    /// than returning a malformed answer to Lash.
    pub fn validate_response(
        &self,
        response: &McpElicitationResult,
    ) -> Result<(), McpElicitationValidationError> {
        validate_elicitation_response(self.params, response, self.validator)
    }

    /// Accept with a form answer, validated against the requested schema.
    pub fn accept(
        &self,
        content: Value,
    ) -> Result<McpElicitationResult, McpElicitationValidationError> {
        let response = McpElicitationResult::Accept {
            content: Some(content),
        };
        self.validate_response(&response)?;
        Ok(response)
    }
}

pub struct McpRootsRequest<'a> {
    /// Sealed request context supplied by Lash.
    pub context: &'a McpRequestContext,
}

/// A host elicitation answer did not conform to the requested MCP schema.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid MCP elicitation response: {message}")]
pub struct McpElicitationValidationError {
    message: String,
}

impl McpElicitationValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Human-readable validation failure suitable for a host re-prompt.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<McpElicitationValidationError> for McpHostRefusal {
    fn from(error: McpElicitationValidationError) -> Self {
        Self::invalid_params(error.to_string()).with_data(serde_json::json!({
            "kind": "elicitation_response_validation",
            "message": error.message,
        }))
    }
}

/// Host-owned MCP sampling policy and execution.
#[async_trait]
pub trait McpSamplingHandler: Send + Sync + 'static {
    async fn create_message(
        &self,
        request: McpSamplingRequest<'_>,
    ) -> Result<McpSamplingResult, McpHostRefusal>;
}

/// Host-owned MCP elicitation policy and interaction.
#[async_trait]
pub trait McpElicitationHandler: Send + Sync + 'static {
    /// Exact elicitation modes this handler supports, read once and snapshotted
    /// when the factory builds. URL interactions return their result through
    /// `create_elicitation` before the originating request is retried.
    fn modes(&self) -> McpElicitationModes;

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<McpElicitationResult, McpHostRefusal>;
}

/// Host-owned workspace roots visible to MCP servers.
#[async_trait]
pub trait McpRootsProvider: Send + Sync + 'static {
    async fn list_roots(
        &self,
        request: McpRootsRequest<'_>,
    ) -> Result<Vec<McpRoot>, McpHostRefusal>;
}

/// Optional server-to-client handlers shared by every connection in a pool.
#[derive(Clone, Default)]
pub(crate) struct McpHostServices {
    pub(crate) route_policy: crate::observations::McpRoutePolicy,
    pub(crate) sampling: Option<Arc<dyn McpSamplingHandler>>,
    pub(crate) elicitation: Option<McpElicitationService>,
    pub(crate) roots: Option<Arc<dyn McpRootsProvider>>,
}

#[derive(Clone)]
pub(crate) struct McpElicitationService {
    pub(crate) handler: Arc<dyn McpElicitationHandler>,
    pub(crate) capability: ElicitationCapability,
}

impl McpHostServices {
    #[allow(
        deprecated,
        reason = "MCP retains sampling and roots during the deprecation window"
    )]
    pub(crate) fn capabilities(&self) -> ClientCapabilities {
        let mut capabilities = ClientCapabilities::default();
        if self.sampling.is_some() {
            capabilities.sampling = Some(SamplingCapability::default());
        }
        capabilities.elicitation = self
            .elicitation
            .as_ref()
            .map(|service| service.capability.clone());
        if self.roots.is_some() {
            let roots = RootsCapabilities::default();

            capabilities.roots = Some(roots);
        }
        capabilities
    }
}

/// Owns host work spawned beneath rmcp's request-dispatch tasks.
///
/// rmcp does not retain those dispatch tasks. This extra ownership layer lets
/// the pool abort host work deterministically when a connection shuts down.
#[derive(Default)]
pub(crate) struct McpHostRequestTasks {
    shut_down: AtomicBool,
    tasks: Mutex<JoinSet<()>>,
}

impl McpHostRequestTasks {
    async fn run<T, F>(&self, future: F) -> Result<T, ErrorData>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, ErrorData>> + Send + 'static,
    {
        if self.shut_down.load(Ordering::SeqCst) {
            return Err(host_tasks_shut_down());
        }
        let (sender, receiver) = oneshot::channel();
        {
            let mut tasks = self.tasks.lock().await;
            while let Some(result) = tasks.try_join_next() {
                if let Err(error) = result {
                    tracing::warn!(%error, "MCP host request task failed");
                }
            }
            if self.shut_down.load(Ordering::SeqCst) {
                return Err(host_tasks_shut_down());
            }
            tasks.spawn(async move {
                let _ = sender.send(future.await);
            });
        }
        receiver
            .await
            .unwrap_or_else(|_| Err(host_tasks_shut_down()))
    }

    pub(crate) async fn shutdown(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
        let mut tasks = self.tasks.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

fn host_tasks_shut_down() -> ErrorData {
    ErrorData::internal_error(
        "MCP host request was aborted because the connection shut down",
        None,
    )
}

/// Per-server rmcp handler that routes protocol requests to host seams.
#[derive(Clone)]
pub(crate) struct LashMcpClientHandler {
    pub(crate) observations: Arc<Mutex<crate::observations::Routes>>,
    server_name: String,
    client_info: ClientConfig,
    services: McpHostServices,
    request_tasks: Arc<McpHostRequestTasks>,
    tool_changes: Option<Arc<crate::pool::tool_changes::ToolChanges>>,
    /// The connection's open `events/stream` requests, by request id.
    event_streams: Arc<crate::events::client::EventStreams>,
}

impl LashMcpClientHandler {
    pub(crate) fn new(server_name: &str, services: McpHostServices) -> Self {
        let implementation = Implementation::new("lash", env!("CARGO_PKG_VERSION"));
        let client_info = ClientConfig::new(services.capabilities(), implementation)
            .with_protocol_version(PROTOCOL_REVISION);
        Self {
            observations: Arc::new(Mutex::new(crate::observations::Routes::new(
                server_name,
                services.route_policy,
            ))),
            server_name: server_name.to_string(),
            client_info,
            services,
            request_tasks: Arc::new(McpHostRequestTasks::default()),
            tool_changes: None,
            event_streams: Arc::default(),
        }
    }

    pub(crate) fn with_tool_changes(
        mut self,
        changes: Arc<crate::pool::tool_changes::ToolChanges>,
    ) -> Self {
        self.tool_changes = Some(changes);
        self
    }

    pub(crate) fn with_event_policy(
        mut self,
        policy: crate::events::McpEventTransportPolicy,
    ) -> Self {
        self.event_streams = Arc::new(crate::events::client::EventStreams::new(
            policy.orphan_notifications,
        ));
        self
    }

    pub(crate) fn event_streams(&self) -> Arc<crate::events::client::EventStreams> {
        Arc::clone(&self.event_streams)
    }

    pub(crate) fn request_tasks(&self) -> Arc<McpHostRequestTasks> {
        Arc::clone(&self.request_tasks)
    }

    pub(crate) async fn fulfill_elicitation(
        &self,
        params: McpElicitationParams,
        cancellation: CancellationToken,
    ) -> Result<ElicitResult, ErrorData> {
        let Some(service) = &self.services.elicitation else {
            return Err(Self::unavailable("elicitation", "elicitation"));
        };
        let handler = Arc::clone(&service.handler);
        let host_context = McpRequestContext::new(self.server_name.clone(), cancellation);
        self.request_tasks
            .run(async move {
                let validator = compile_elicitation_response_validator(&params)
                    .map_err(|error| McpHostRefusal::from(error).into_protocol_error())?;
                let response = handler
                    .create_elicitation(McpElicitationRequest {
                        params: &params,
                        context: &host_context,
                        validator: validator.as_ref(),
                    })
                    .await
                    .map_err(McpHostRefusal::into_protocol_error)?;
                validate_elicitation_response(&params, &response, validator.as_ref())
                    .map_err(|error| McpHostRefusal::from(error).into_protocol_error())?;
                Ok(response.into_sdk())
            })
            .await
    }

    fn unavailable(capability: &'static str, handler: &'static str) -> ErrorData {
        ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("{capability} is not available: no host {handler} handler is installed"),
            None,
        )
    }
}

impl ClientHandler for LashMcpClientHandler {
    fn get_info(&self) -> ClientConfig {
        self.client_info.clone()
    }

    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        let Some(handler) = &self.services.sampling else {
            return Err(Self::unavailable("sampling", "sampling"));
        };
        let params =
            McpSamplingParams::from_sdk(params).map_err(McpHostRefusal::into_protocol_error)?;
        let handler = Arc::clone(handler);
        let host_context = McpRequestContext::new(self.server_name.clone(), context.ct);
        self.request_tasks
            .run(async move {
                handler
                    .create_message(McpSamplingRequest {
                        params: &params,
                        context: &host_context,
                    })
                    .await
                    .map_err(McpHostRefusal::into_protocol_error)?
                    .into_sdk()
            })
            .await
    }

    async fn list_roots(
        &self,
        context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        let Some(provider) = &self.services.roots else {
            return Err(Self::unavailable("roots", "roots"));
        };
        let provider = Arc::clone(provider);
        let host_context = McpRequestContext::new(self.server_name.clone(), context.ct);
        self.request_tasks
            .run(async move {
                provider
                    .list_roots(McpRootsRequest {
                        context: &host_context,
                    })
                    .await
                    .map(|roots| {
                        ListRootsResult::new(roots.into_iter().map(McpRoot::into_sdk).collect())
                    })
                    .map_err(McpHostRefusal::into_protocol_error)
            })
            .await
    }

    async fn on_subscriptions_acknowledged(
        &self,
        params: rmcp::model::SubscriptionsAcknowledgedNotificationParams,
        context: NotificationContext<RoleClient>,
    ) {
        if let Some(changes) = &self.tool_changes {
            // rmcp 3.5.1 moves envelope extensions before extracting `meta`;
            // notification metadata therefore remains in the typed extensions.
            let id = context
                .extensions
                .get::<rmcp::model::NotificationMetaObject>()
                .and_then(|meta| meta.subscription_id());
            changes.acknowledge(params, id);
        }
    }

    async fn on_custom_notification(
        &self,
        notification: rmcp::model::CustomNotification,
        context: NotificationContext<RoleClient>,
    ) {
        let subscription = context
            .extensions
            .get::<rmcp::model::NotificationMetaObject>()
            .and_then(|meta| meta.subscription_id())
            .map(rmcp::model::RequestId::into_json_value);
        crate::events::client::route_notification(&self.event_streams, &notification, subscription);
    }

    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        self.observations.lock().await.progress(params);
    }

    #[allow(
        deprecated,
        reason = "forward server logging while it remains on the MCP wire"
    )]
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        self.observations.lock().await.log(params, context);
    }

    async fn on_resource_list_changed(&self, _: NotificationContext<RoleClient>) {}

    async fn on_prompt_list_changed(&self, _: NotificationContext<RoleClient>) {}

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        if let Some(changes) = &self.tool_changes {
            let id = context
                .extensions
                .get::<rmcp::model::NotificationMetaObject>()
                .and_then(|meta| meta.subscription_id());
            changes.changed(id);
        }
    }
}

fn validate_elicitation_response(
    request: &McpElicitationParams,
    response: &McpElicitationResult,
    validator: Option<&jsonschema::Validator>,
) -> Result<(), McpElicitationValidationError> {
    let McpElicitationResult::Accept { content } = response else {
        return Ok(());
    };
    match request {
        McpElicitationParams::Form { .. } => {
            let content = content.as_ref().ok_or_else(|| {
                McpElicitationValidationError::new(
                    "an accepted form elicitation must contain an answer",
                )
            })?;
            let validator = validator.ok_or_else(|| {
                McpElicitationValidationError::new(
                    "requested schema was not compiled before answer validation",
                )
            })?;
            if let Err(error) = validator.validate(content) {
                return Err(McpElicitationValidationError::new(format!(
                    "answer does not match requested schema at `{}`: {error}",
                    error.instance_path()
                )));
            }
            Ok(())
        }
        McpElicitationParams::Url { .. } => {
            if content.is_some() {
                return Err(McpElicitationValidationError::new(
                    "an accepted URL elicitation must not contain form content",
                ));
            }
            Ok(())
        }
    }
}

fn compile_elicitation_response_validator(
    request: &McpElicitationParams,
) -> Result<Option<jsonschema::Validator>, McpElicitationValidationError> {
    let McpElicitationParams::Form {
        requested_schema, ..
    } = request
    else {
        return Ok(None);
    };
    let schema = requested_schema;
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012.detect(schema))
        .should_validate_formats(true)
        .build(schema)
        .map(Some)
        .map_err(|error| {
            McpElicitationValidationError::new(format!(
                "requested schema could not be compiled: {error}"
            ))
        })
}
