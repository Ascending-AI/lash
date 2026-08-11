//! Host-owned handlers for MCP server-to-client requests.
//!
//! Lash only provides protocol routing. Whether a server may sample a model,
//! how elicitation is presented, and which workspace roots are visible are all
//! decisions made by the embedding host through these dyn-compatible seams.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateElicitationRequestParams, CreateElicitationResult,
    CreateMessageRequestParams, CreateMessageResult, ElicitationAction, ElicitationCapability,
    ElicitationResponseNotificationParam, ErrorCode, ErrorData, Implementation, ListRootsResult,
    ProtocolVersion, Root, RootsCapabilities, SamplingCapability,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RoleClient};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// MCP protocol revision used by the client handshake.
///
/// This is the revision targeted by the workspace's pinned `rmcp` release and
/// is the first pinned revision here that includes sampling, elicitation, and
/// roots together (including URL elicitation).
pub const MCP_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

#[async_trait]
pub(crate) trait McpToolListChangedHandler: Send + Sync {
    async fn refresh_tools(&self, peer: Peer<RoleClient>);
}

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

/// Inputs for one server-to-client sampling request.
pub struct McpSamplingRequest<'a> {
    /// Protocol request payload.
    pub params: &'a CreateMessageRequestParams,
    /// Sealed request context supplied by Lash.
    pub context: &'a McpRequestContext,
}

/// Inputs for one server-to-client elicitation request.
pub struct McpElicitationRequest<'a> {
    /// Protocol request payload.
    pub params: &'a CreateElicitationRequestParams,
    /// Sealed request context supplied by Lash.
    pub context: &'a McpRequestContext,
    validator: Option<&'a jsonschema::JSONSchema>,
}

impl McpElicitationRequest<'_> {
    /// Validate an answer before returning it from the host handler.
    ///
    /// Lash repeats this validation at the wire boundary. Calling it here
    /// gives an interactive host a typed error it can use to re-prompt rather
    /// than returning a malformed answer to Lash.
    pub fn validate_response(
        &self,
        response: &CreateElicitationResult,
    ) -> Result<(), McpElicitationValidationError> {
        validate_elicitation_response(self.params, response, self.validator)
    }

    /// Construct and validate an accepted form response.
    pub fn accept(
        &self,
        content: Value,
    ) -> Result<CreateElicitationResult, McpElicitationValidationError> {
        let response =
            CreateElicitationResult::new(ElicitationAction::Accept).with_content(content);
        self.validate_response(&response)?;
        Ok(response)
    }
}

/// Inputs for one server-to-client roots request.
pub struct McpRootsRequest<'a> {
    /// Sealed request context supplied by Lash.
    pub context: &'a McpRequestContext,
}

/// Context for a server notification routed to an MCP host handler.
pub struct McpNotificationContext {
    server_name: String,
}

impl McpNotificationContext {
    fn new(server_name: String) -> Self {
        Self { server_name }
    }

    /// Configured name of the MCP server that issued this notification.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// Inputs for a URL-elicitation completion notification.
pub struct McpUrlElicitationComplete<'a> {
    /// Identifier from the original URL elicitation request.
    pub elicitation_id: &'a str,
    /// Sealed notification context supplied by Lash.
    pub context: &'a McpNotificationContext,
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

    fn into_protocol_error(self) -> ErrorData {
        ErrorData::invalid_params(
            self.to_string(),
            Some(serde_json::json!({
                "kind": "elicitation_response_validation",
                "message": self.message,
            })),
        )
    }
}

/// Host-owned MCP sampling policy and execution.
#[async_trait]
pub trait McpSamplingHandler: Send + Sync + 'static {
    /// Handle one server-to-client `sampling/createMessage` request.
    async fn create_message(
        &self,
        request: McpSamplingRequest<'_>,
    ) -> Result<CreateMessageResult, ErrorData>;
}

/// Host-owned MCP elicitation policy and interaction.
#[async_trait]
pub trait McpElicitationHandler: Send + Sync + 'static {
    /// Exact elicitation modes this handler supports, read once and snapshotted
    /// when the factory builds.
    ///
    /// Returning an empty capability is invalid: an installed handler must
    /// advertise at least form or URL elicitation honestly. Advertising URL
    /// mode also promises a meaningful implementation of
    /// [`url_elicitation_complete`](Self::url_elicitation_complete).
    fn capability(&self) -> ElicitationCapability;

    /// Handle one server-to-client `elicitation/create` request.
    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<CreateElicitationResult, ErrorData>;

    /// Observe completion of a previously accepted URL elicitation.
    async fn url_elicitation_complete(&self, notification: McpUrlElicitationComplete<'_>);
}

/// Host-owned workspace roots visible to MCP servers.
#[async_trait]
pub trait McpRootsProvider: Send + Sync + 'static {
    /// Return the current roots for one connected server.
    async fn list_roots(&self, request: McpRootsRequest<'_>) -> Result<Vec<Root>, ErrorData>;
}

/// Optional server-to-client handlers shared by every connection in a pool.
#[derive(Clone, Default)]
pub(crate) struct McpHostServices {
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
    #[allow(deprecated, reason = "MCP 2025-11-25 still defines sampling and roots")]
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
            capabilities.roots = Some(RootsCapabilities {
                list_changed: Some(true),
            });
        }
        capabilities
    }

    pub(crate) fn has_roots(&self) -> bool {
        self.roots.is_some()
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
    server_name: String,
    client_info: ClientInfo,
    services: McpHostServices,
    request_tasks: Arc<McpHostRequestTasks>,
    tool_list_changed: Option<Arc<dyn McpToolListChangedHandler>>,
}

impl LashMcpClientHandler {
    pub(crate) fn new(server_name: &str, services: McpHostServices) -> Self {
        let implementation = Implementation::new("lash", env!("CARGO_PKG_VERSION"));
        let client_info = ClientInfo::new(services.capabilities(), implementation)
            .with_protocol_version(MCP_PROTOCOL_VERSION);
        Self {
            server_name: server_name.to_string(),
            client_info,
            services,
            request_tasks: Arc::new(McpHostRequestTasks::default()),
            tool_list_changed: None,
        }
    }

    pub(crate) fn with_tool_list_changed_handler(
        mut self,
        handler: Arc<dyn McpToolListChangedHandler>,
    ) -> Self {
        self.tool_list_changed = Some(handler);
        self
    }

    pub(crate) fn request_tasks(&self) -> Arc<McpHostRequestTasks> {
        Arc::clone(&self.request_tasks)
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
    fn get_info(&self) -> ClientInfo {
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
            })
            .await
    }

    async fn create_elicitation(
        &self,
        params: CreateElicitationRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        let Some(service) = &self.services.elicitation else {
            return Err(Self::unavailable("elicitation", "elicitation"));
        };
        let handler = Arc::clone(&service.handler);
        let host_context = McpRequestContext::new(self.server_name.clone(), context.ct);
        self.request_tasks
            .run(async move {
                let validator = compile_elicitation_response_validator(&params)
                    .map_err(McpElicitationValidationError::into_protocol_error)?;
                let response = handler
                    .create_elicitation(McpElicitationRequest {
                        params: &params,
                        context: &host_context,
                        validator: validator.as_ref(),
                    })
                    .await?;
                validate_elicitation_response(&params, &response, validator.as_ref())
                    .map_err(McpElicitationValidationError::into_protocol_error)?;
                Ok(response)
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
                    .map(ListRootsResult::new)
            })
            .await
    }

    async fn on_url_elicitation_notification_complete(
        &self,
        params: ElicitationResponseNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let Some(service) = &self.services.elicitation else {
            return;
        };
        if service.capability.url.is_none() {
            return;
        }
        let handler = Arc::clone(&service.handler);
        let host_context = McpNotificationContext::new(self.server_name.clone());
        let _ = self
            .request_tasks
            .run(async move {
                handler
                    .url_elicitation_complete(McpUrlElicitationComplete {
                        elicitation_id: &params.elicitation_id,
                        context: &host_context,
                    })
                    .await;
                Ok(())
            })
            .await;
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        if let Some(handler) = &self.tool_list_changed {
            handler.refresh_tools(context.peer).await;
        }
    }
}

fn validate_elicitation_response(
    request: &CreateElicitationRequestParams,
    response: &CreateElicitationResult,
    validator: Option<&jsonschema::JSONSchema>,
) -> Result<(), McpElicitationValidationError> {
    match response.action {
        ElicitationAction::Accept => match request {
            CreateElicitationRequestParams::FormElicitationParams { .. } => {
                let content = response.content.as_ref().ok_or_else(|| {
                    McpElicitationValidationError::new(
                        "an accepted form elicitation must contain an answer",
                    )
                })?;
                let validator = validator.ok_or_else(|| {
                    McpElicitationValidationError::new(
                        "requested schema was not compiled before answer validation",
                    )
                })?;
                if let Err(mut errors) = validator.validate(content) {
                    let error = errors.next().expect("validation failure includes an error");
                    return Err(McpElicitationValidationError::new(format!(
                        "answer does not match requested schema at `{}`: {error}",
                        error.instance_path
                    )));
                }
                Ok(())
            }
            CreateElicitationRequestParams::UrlElicitationParams { .. } => {
                if response.content.is_some() {
                    return Err(McpElicitationValidationError::new(
                        "an accepted URL elicitation must not contain form content",
                    ));
                }
                Ok(())
            }
        },
        ElicitationAction::Decline | ElicitationAction::Cancel => {
            if response.content.is_some() {
                return Err(McpElicitationValidationError::new(
                    "declined or cancelled elicitation responses must not contain content",
                ));
            }
            Ok(())
        }
    }
}

fn compile_elicitation_response_validator(
    request: &CreateElicitationRequestParams,
) -> Result<Option<jsonschema::JSONSchema>, McpElicitationValidationError> {
    let CreateElicitationRequestParams::FormElicitationParams {
        requested_schema, ..
    } = request
    else {
        return Ok(None);
    };
    let schema = serde_json::to_value(requested_schema).map_err(|error| {
        McpElicitationValidationError::new(format!(
            "requested schema could not be compiled: {error}"
        ))
    })?;
    jsonschema::JSONSchema::options()
        .should_validate_formats(true)
        .compile(&schema)
        .map(Some)
        .map_err(|error| {
            McpElicitationValidationError::new(format!(
                "requested schema could not be compiled: {error}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{FormElicitationCapability, UrlElicitationCapability};

    struct ElicitationModes;

    #[async_trait]
    impl McpElicitationHandler for ElicitationModes {
        fn capability(&self) -> ElicitationCapability {
            ElicitationCapability {
                form: Some(FormElicitationCapability::default()),
                url: Some(UrlElicitationCapability::default()),
            }
        }

        async fn create_elicitation(
            &self,
            _request: McpElicitationRequest<'_>,
        ) -> Result<CreateElicitationResult, ErrorData> {
            unreachable!("capability test does not issue requests")
        }

        async fn url_elicitation_complete(&self, _notification: McpUrlElicitationComplete<'_>) {
            unreachable!("capability test does not issue notifications")
        }
    }

    #[test]
    #[allow(deprecated, reason = "MCP 2025-11-25 still defines sampling and roots")]
    fn capabilities_are_derived_only_from_wired_host_services() {
        let absent = McpHostServices::default().capabilities();
        assert!(absent.sampling.is_none());
        assert!(absent.elicitation.is_none());
        assert!(absent.roots.is_none());

        let elicitation_only = McpHostServices {
            elicitation: Some(McpElicitationService {
                handler: Arc::new(ElicitationModes),
                capability: ElicitationModes.capability(),
            }),
            ..Default::default()
        }
        .capabilities();
        assert!(elicitation_only.sampling.is_none());
        assert!(elicitation_only.roots.is_none());
        let elicitation = elicitation_only
            .elicitation
            .expect("wired elicitation capability");
        assert!(elicitation.form.is_some());
        assert!(elicitation.url.is_some());
    }

    #[test]
    fn protocol_version_is_known_to_the_pinned_rmcp_sdk() {
        assert!(ProtocolVersion::KNOWN_VERSIONS.contains(&MCP_PROTOCOL_VERSION));
    }
}
