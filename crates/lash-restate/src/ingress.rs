//! Host-owned Restate ingress and Admin HTTP clients.
//!
//! One responsibility: speak HTTP to a Restate deployment. Authentication,
//! mTLS, proxying, and credential rotation stay with the host through
//! [`HttpTransport`] (ADR 0014), so this module owns only URL shaping, request
//! encoding, status handling, and response decoding.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use lash_http_transport::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, HttpTransportError, ReqwestClient,
    ReqwestHttpTransport, read_http_body_bytes,
};
use serde::{Serialize, de::DeserializeOwned};

const DEFAULT_CONTROL_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_ATTACH_CEILING_MS: u64 = 6 * 60 * 60 * 1_000;

const fn default_control_timeout_ms() -> u64 {
    DEFAULT_CONTROL_TIMEOUT_MS
}

const fn default_attach_ceiling_ms() -> u64 {
    DEFAULT_ATTACH_CEILING_MS
}

/// Deadline classes for HTTP operations issued through a [`RestateConnection`].
///
/// Control operations should fail quickly so callers can retry or report a
/// degraded substrate. Attach operations can legitimately remain parked for a
/// durable workflow's lifetime, so they receive a separate generous ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateConnectionConfig {
    /// Submit, cancel, status, query, and patch deadline. Default 30 seconds.
    #[serde(
        default = "default_control_timeout_ms",
        deserialize_with = "deserialize_control_timeout_ms"
    )]
    pub control_timeout_ms: u64,
    /// Await/attach request ceiling. Default 6 hours.
    #[serde(
        default = "default_attach_ceiling_ms",
        deserialize_with = "deserialize_attach_ceiling_ms"
    )]
    pub attach_ceiling_ms: u64,
}

fn deserialize_control_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "control_timeout_ms must be greater than zero",
        ));
    }
    Ok(value)
}

fn deserialize_attach_ceiling_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "attach_ceiling_ms must be greater than zero",
        ));
    }
    Ok(value)
}

impl Default for RestateConnectionConfig {
    fn default() -> Self {
        Self {
            control_timeout_ms: default_control_timeout_ms(),
            attach_ceiling_ms: default_attach_ceiling_ms(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct RestateInvocationId(String);

impl RestateInvocationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for RestateInvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for RestateInvocationId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for RestateInvocationId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RestateHttpError {
    #[error("{operation} request failed for {url}: {source}")]
    Request {
        operation: &'static str,
        url: String,
        source: HttpTransportError,
    },
    #[error("{operation} returned status {status} for {url}: {body}")]
    Status {
        operation: &'static str,
        url: String,
        status: u16,
        body: String,
    },
    #[error("{operation} request encode failed for {url}: {source}")]
    Encode {
        operation: &'static str,
        url: String,
        source: serde_json::Error,
    },
    #[error("{operation} response decode failed for {url}: {source}")]
    Decode {
        operation: &'static str,
        url: String,
        source: serde_json::Error,
    },
    #[error("Restate /send returned unexpected status `{status}` for {url}")]
    UnexpectedSendStatus { url: String, status: String },
}

impl RestateHttpError {
    /// Whether this failure elapsed a configured transport deadline.
    pub fn is_timeout(&self) -> bool {
        matches!(
            self,
            Self::Request { source, .. }
                if source.kind == lash_core::ProviderFailureKind::Timeout
        )
    }

    /// Whether the engine answered "no such service or handler" — a **missing
    /// registration**, not a transport fault (FIG-1579).
    ///
    /// Restate answers an invocation addressed to a service no registered
    /// deployment binds, or to a handler a bound service does not declare, with
    /// `404`. That is a deterministic contract failure and the engine tier's
    /// form of the routing fact ADR 0065 names on the effect-group seam: *a
    /// child with no runner*. It is classified rather than folded into "some
    /// HTTP status came back" because the two call for opposite handling — a
    /// transport fault is worth retrying and an unbound service is not, and no
    /// amount of retrying makes a service somebody forgot to `bind` appear.
    ///
    /// Callers inside a Restate handler must turn this into a **terminal**.
    /// Restate retries a handler error with infinite exponential backoff by
    /// default, so a retryable 404 is a deployment mistake that silently becomes
    /// an invocation backing off forever with nobody told what is wrong — the
    /// engine-tier twin of the warn-and-strand this contract rules out.
    ///
    /// # Only a direct service-call route can say this
    ///
    /// The status alone does not carry the meaning; the **route** does, which is
    /// why this reads [`operation`](Self::Status) as well. A `404` from
    /// `{service}/{key}/{handler}` or its `/send` form is addressing failure:
    /// there is no such service or handler to invoke. A `404` from a **control
    /// route** — `PATCH /invocations/{id}/{action}`, the SQL query endpoint — is
    /// about a *resource*, and most often means the invocation named is already
    /// gone: completed, killed, or aged out of retention. Those are deliberately
    /// excluded. Answering `true` for them would let a caller terminalize real
    /// work over a cancel that arrived one moment late, which is the opposite of
    /// the mistake this predicate exists to prevent.
    ///
    /// A new ingress route must therefore opt in by name. That is the safe
    /// default: an unclassified route keeps the retryable handling every 404 had
    /// before this predicate existed.
    pub fn is_service_unregistered(&self) -> bool {
        matches!(
            self,
            Self::Status { status, operation, .. }
                if *status == 404 && SERVICE_CALL_OPERATIONS.contains(operation)
        )
    }
}

/// The ingress routes that address a service by name, where a `404` is a missing
/// registration rather than a missing resource.
///
/// Exactly the three routes that post to `{service}/{key}/{handler}`. Everything
/// else this client speaks — the invocation control routes and the SQL query
/// endpoint — is deliberately absent; see
/// [`RestateHttpError::is_service_unregistered`].
const SERVICE_CALL_OPERATIONS: &[&str] = &[
    "Restate workflow call",
    "Restate object call",
    "Restate /send",
];

/// What an operator is told when a child invocation names a service or handler
/// no registered deployment binds.
///
/// Names the address rather than restating the status, because "404 from
/// Restate" is not something an operator can act on and "nothing binds
/// `LashProcessWorkflow`" is.
pub(crate) fn unregistered_service_message(
    service: &str,
    handler: &str,
    error: &RestateHttpError,
) -> String {
    format!(
        "Restate has no `{service}/{handler}` to invoke: no registered \
         deployment binds that service, or the service it does bind declares no \
         such handler. This is a deployment fact rather than a transport fault \
         — retrying cannot make an unbound service appear — so bind `{service}` \
         on every endpoint that wires a Lash Restate controller ({error})"
    )
}

/// What an operator is told when a call to a **shared handler** cannot be
/// addressed.
///
/// Deliberately weaker than [`unregistered_service_message`], because on this
/// route the status genuinely carries two readings and the client cannot tell
/// them apart: nothing binds the service, *or* the service is bound and the
/// workflow key names no invocation the engine still holds — completed and aged
/// out of retention, or never submitted. Asserting the first would send an
/// operator to check a deployment that is fine. Both readings are named, and the
/// one that distinguishes them (does anything bind it?) is the one an operator
/// can check first.
pub(crate) fn unresolvable_call_target_message(
    service: &str,
    handler: &str,
    error: &RestateHttpError,
) -> String {
    format!(
        "Restate could not address `{service}/{handler}`: either no registered \
         deployment binds that service, or it is bound and this workflow key \
         names no invocation the engine still holds — one that completed and \
         aged out of retention, or was never submitted. Check the binding \
         first; a service nothing binds fails every call this way, while a \
         lapsed invocation fails only its own key ({error})"
    )
}

/// The engine-native terminal a missing registration must surface as from inside
/// a handler.
///
/// `404` is carried through as the terminal's own status so the failure reads
/// the same to an operator at the ingress as it does in the invocation's
/// journal.
pub(crate) fn unregistered_service_terminal(
    service: &str,
    handler: &str,
    error: &RestateHttpError,
) -> restate_sdk::errors::TerminalError {
    restate_sdk::errors::TerminalError::new_with_code(
        404,
        unregistered_service_message(service, handler, error),
    )
}
/// Host-owned Restate ingress connectivity.
///
/// Authentication, mTLS, proxying, and credential rotation are operational
/// policy supplied by the host through [`HttpTransport`], following
/// [ADR 0014's operational-policy-stays-with-the-host principle](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0014-operational-policy-stays-with-the-host.md).
/// A host can use a configured reqwest client for static authentication:
///
/// ```no_run
/// use lash_http_transport::reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
/// use lash_restate::RestateConnection;
///
/// let mut headers = HeaderMap::new();
/// let mut authorization = HeaderValue::from_static("Bearer restate-cloud-token");
/// authorization.set_sensitive(true);
/// headers.insert(AUTHORIZATION, authorization);
/// let client = lash_http_transport::http_client_builder()
///     .default_headers(headers)
///     .build()?;
/// let connection = RestateConnection::with_client(
///     "https://example.env.restate.cloud",
///     client,
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct RestateConnection {
    ingress_url: String,
    transport: Arc<dyn HttpTransport>,
    config: RestateConnectionConfig,
}

impl RestateConnection {
    pub fn new(ingress_url: impl Into<String>) -> Self {
        Self::with_config(ingress_url, RestateConnectionConfig::default())
    }

    pub fn with_config(ingress_url: impl Into<String>, config: RestateConnectionConfig) -> Self {
        Self::with_transport_and_config(ingress_url, Arc::new(ReqwestHttpTransport::new()), config)
    }

    pub fn with_transport(
        ingress_url: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self::with_transport_and_config(ingress_url, transport, RestateConnectionConfig::default())
    }

    pub fn with_transport_and_config(
        ingress_url: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
        config: RestateConnectionConfig,
    ) -> Self {
        Self {
            ingress_url: ingress_url.into(),
            transport,
            config,
        }
    }

    pub fn with_client(ingress_url: impl Into<String>, client: ReqwestClient) -> Self {
        Self::with_client_and_config(ingress_url, client, RestateConnectionConfig::default())
    }

    pub fn with_client_and_config(
        ingress_url: impl Into<String>,
        client: ReqwestClient,
        config: RestateConnectionConfig,
    ) -> Self {
        Self::with_transport_and_config(
            ingress_url,
            Arc::new(ReqwestHttpTransport::from_client(client)),
            config,
        )
    }

    pub fn ingress_url(&self) -> &str {
        &self.ingress_url
    }

    pub fn config(&self) -> &RestateConnectionConfig {
        &self.config
    }
}

impl From<&str> for RestateConnection {
    fn from(ingress_url: &str) -> Self {
        Self::new(ingress_url)
    }
}

impl From<String> for RestateConnection {
    fn from(ingress_url: String) -> Self {
        Self::new(ingress_url)
    }
}

#[derive(Clone, Debug)]
pub struct RestateIngressClient {
    connection: RestateConnection,
}

impl RestateIngressClient {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        Self {
            connection: connection.into(),
        }
    }

    pub fn ingress_url(&self) -> &str {
        self.connection.ingress_url()
    }

    pub async fn send_json_path<T: Serialize + ?Sized>(
        &self,
        path: impl AsRef<str>,
        body: &T,
    ) -> Result<RestateInvocationId, RestateHttpError> {
        let url = format_restate_url(self.connection.ingress_url(), path.as_ref());
        let response = self.post_json("Restate /send", &url, body).await?;
        if !response.is_success() {
            return Err(status_error("Restate /send", url, response).await);
        }
        let accepted: RestateSendResponse =
            decode_response("Restate /send", &url, response).await?;
        if !matches_restate_accepted_status(&accepted.status) {
            return Err(RestateHttpError::UnexpectedSendStatus {
                url,
                status: accepted.status,
            });
        }
        Ok(RestateInvocationId::new(accepted.invocation_id))
    }

    pub async fn send_workflow_json<T: Serialize + ?Sized>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
    ) -> Result<RestateInvocationId, RestateHttpError> {
        let workflow_key = restate_path_component(workflow_key);
        self.send_json_path(format!("{workflow}/{workflow_key}/{handler}/send"), body)
            .await
    }

    pub async fn call_workflow_json<T, R>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.call_workflow_json_inner(workflow, workflow_key, handler, body, None)
            .await
    }

    pub(crate) async fn call_workflow_json_idempotent<T, R>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
        idempotency_key: &str,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        self.call_workflow_json_inner(workflow, workflow_key, handler, body, Some(idempotency_key))
            .await
    }

    async fn call_workflow_json_inner<T, R>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
        body: &T,
        idempotency_key: Option<&str>,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let workflow_key = restate_path_component(workflow_key);
        let path = format!("{workflow}/{workflow_key}/{handler}");
        let url = format_restate_url(self.connection.ingress_url(), &path);
        let body = serde_json::to_vec(body).map_err(|source| RestateHttpError::Encode {
            operation: "Restate workflow call",
            url: url.clone(),
            source,
        })?;
        let mut request =
            HttpRequest::post(&url, body).with_header("content-type", "application/json");
        if let Some(idempotency_key) = idempotency_key {
            request = request.with_header("idempotency-key", idempotency_key);
        }
        let response = send_request(
            &self.connection,
            RestateRequestClass::Attach,
            "Restate workflow call",
            request,
        )
        .await?;
        if !response.is_success() {
            return Err(status_error("Restate workflow call", url, response).await);
        }
        decode_response("Restate workflow call", &url, response).await
    }

    /// Invoke a no-argument workflow handler and decode its JSON response.
    pub async fn call_workflow_empty<R>(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
    ) -> Result<R, RestateHttpError>
    where
        R: DeserializeOwned,
    {
        let workflow_key = restate_path_component(workflow_key);
        let path = format!("{workflow}/{workflow_key}/{handler}");
        let url = format_restate_url(self.connection.ingress_url(), &path);
        let response = send_request(
            &self.connection,
            RestateRequestClass::Control,
            "Restate workflow call",
            HttpRequest::post(&url, ""),
        )
        .await?;
        if !response.is_success() {
            return Err(status_error("Restate workflow call", url, response).await);
        }
        decode_response("Restate workflow call", &url, response).await
    }

    pub async fn call_object_json<T, R>(
        &self,
        object: &str,
        object_key: &str,
        handler: &str,
        body: &T,
    ) -> Result<R, RestateHttpError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let path = format!("{object}/{object_key}/{handler}");
        let url = format_restate_url(self.connection.ingress_url(), &path);
        let response = self.post_json("Restate object call", &url, body).await?;
        if !response.is_success() {
            return Err(status_error("Restate object call", url, response).await);
        }
        decode_response("Restate object call", &url, response).await
    }

    /// Invoke a no-argument object handler. Restate's ingress rejects calls to
    /// zero-input handlers that carry a body or content-type, so this posts an
    /// empty request and ignores the (empty or `null`) response payload.
    pub async fn call_object_empty(
        &self,
        object: &str,
        object_key: &str,
        handler: &str,
    ) -> Result<(), RestateHttpError> {
        let path = format!("{object}/{object_key}/{handler}");
        let url = format_restate_url(self.connection.ingress_url(), &path);
        let response = send_request(
            &self.connection,
            RestateRequestClass::Control,
            "Restate object call",
            HttpRequest::post(&url, ""),
        )
        .await?;
        if !response.is_success() {
            return Err(status_error("Restate object call", url, response).await);
        }
        Ok(())
    }

    pub async fn send_service_json<T: Serialize + ?Sized>(
        &self,
        service: &str,
        handler: &str,
        body: &T,
    ) -> Result<RestateInvocationId, RestateHttpError> {
        self.send_json_path(format!("{service}/{handler}/send"), body)
            .await
    }

    pub async fn send_object_json<T: Serialize + ?Sized>(
        &self,
        object: &str,
        key: &str,
        handler: &str,
        body: &T,
    ) -> Result<RestateInvocationId, RestateHttpError> {
        self.send_json_path(format!("{object}/{key}/{handler}/send"), body)
            .await
    }

    async fn post_json<T: Serialize + ?Sized>(
        &self,
        operation: &'static str,
        url: &str,
        body: &T,
    ) -> Result<RestateHttpResponse, RestateHttpError> {
        let body = serde_json::to_vec(body).map_err(|source| RestateHttpError::Encode {
            operation,
            url: url.to_string(),
            source,
        })?;
        send_request(
            &self.connection,
            RestateRequestClass::Control,
            operation,
            HttpRequest::post(url, body).with_header("content-type", "application/json"),
        )
        .await
    }
}

fn restate_path_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}
#[derive(Debug, serde::Deserialize)]
struct RestateSendResponse {
    #[serde(rename = "invocationId")]
    invocation_id: String,
    status: String,
}
#[derive(Clone, Debug)]
pub struct RestateAdminClient {
    connection: RestateConnection,
}

impl RestateAdminClient {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        Self {
            connection: connection.into(),
        }
    }

    pub fn admin_url(&self) -> &str {
        self.connection.ingress_url()
    }

    pub async fn cancel_invocation(
        &self,
        invocation_id: &RestateInvocationId,
    ) -> Result<(), RestateHttpError> {
        self.patch_invocation(invocation_id, "cancel", "Restate invocation cancel")
            .await
    }

    /// Forcefully kill an invocation. This is intended for test/dev cleanup
    /// after graceful cancellation has failed.
    pub async fn kill_invocation_for_test_cleanup(
        &self,
        invocation_id: &RestateInvocationId,
    ) -> Result<(), RestateHttpError> {
        self.patch_invocation(invocation_id, "kill", "Restate invocation kill")
            .await
    }

    pub async fn invocation_status(
        &self,
        invocation_id: &RestateInvocationId,
    ) -> Result<Option<RestateInvocationStatus>, RestateHttpError> {
        let id = sql_string_literal(invocation_id.as_str());
        let mut rows = self
            .query_json::<RestateInvocationStatus>(&format!(
                "SELECT id, target, target_service_name, target_service_key, target_handler_name, status, completion_result, completion_failure FROM sys_invocation WHERE id = {id}"
            ))
            .await?;
        Ok(rows.pop())
    }

    pub async fn workflow_invocation_status(
        &self,
        workflow: &str,
        workflow_key: &str,
        handler: &str,
    ) -> Result<Option<RestateInvocationStatus>, RestateHttpError> {
        let workflow = sql_string_literal(workflow);
        let workflow_key = sql_string_literal(workflow_key);
        let handler = sql_string_literal(handler);
        let mut rows = self
            .query_json::<RestateInvocationStatus>(&format!(
                "SELECT id, target, target_service_name, target_service_key, target_handler_name, status, completion_result, completion_failure FROM sys_invocation WHERE target_service_name = {workflow} AND target_service_key = {workflow_key} AND target_handler_name = {handler} ORDER BY modified_at DESC LIMIT 1"
            ))
            .await?;
        Ok(rows.pop())
    }

    pub async fn unfinished_invocations_for_service_prefixes(
        &self,
        prefixes: &[&str],
    ) -> Result<Vec<RestateInvocationStatus>, RestateHttpError> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }
        let service_filter = prefixes
            .iter()
            .map(|prefix| {
                format!(
                    "target_service_name LIKE {}",
                    sql_string_literal(&format!("{prefix}%"))
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        self.query_json(&format!(
            "SELECT id, target, target_service_name, target_service_key, target_handler_name, status, completion_result, completion_failure, pinned_deployment_id FROM sys_invocation WHERE status IN ('pending', 'ready', 'running', 'backing-off', 'suspended') AND ({service_filter}) ORDER BY modified_at DESC"
        ))
        .await
    }

    /// Count open Restate invocations grouped by their pinned deployment.
    pub async fn open_invocations_by_deployment(
        &self,
    ) -> Result<Vec<DeploymentOpenInvocations>, RestateHttpError> {
        self.query_json(
            "SELECT pinned_deployment_id, COUNT(1) as open_count FROM sys_invocation WHERE status IN ('pending', 'ready', 'running', 'backing-off', 'suspended') GROUP BY pinned_deployment_id",
        )
        .await
    }

    pub async fn query_json<T: DeserializeOwned>(
        &self,
        query: &str,
    ) -> Result<Vec<T>, RestateHttpError> {
        #[derive(Serialize)]
        struct QueryRequest<'a> {
            query: &'a str,
        }

        #[derive(serde::Deserialize)]
        struct QueryResponse<T> {
            rows: Vec<T>,
        }

        let url = format_restate_url(self.connection.ingress_url(), "query");
        let body = serde_json::to_vec(&QueryRequest { query }).map_err(|source| {
            RestateHttpError::Encode {
                operation: "Restate SQL query",
                url: url.clone(),
                source,
            }
        })?;
        let request = HttpRequest::post(&url, body).with_headers([
            ("accept", "application/json"),
            ("content-type", "application/json"),
        ]);
        let response = send_request(
            &self.connection,
            RestateRequestClass::Control,
            "Restate SQL query",
            request,
        )
        .await?;
        if !response.is_success() {
            return Err(status_error("Restate SQL query", url, response).await);
        }
        decode_response::<QueryResponse<T>>("Restate SQL query", &url, response)
            .await
            .map(|response| response.rows)
    }

    async fn patch_invocation(
        &self,
        invocation_id: &RestateInvocationId,
        action: &str,
        operation: &'static str,
    ) -> Result<(), RestateHttpError> {
        let url = format_restate_url(
            self.connection.ingress_url(),
            &format!("invocations/{invocation_id}/{action}"),
        );
        let response = send_request(
            &self.connection,
            RestateRequestClass::Control,
            operation,
            HttpRequest::new(HttpMethod::Patch, &url, ""),
        )
        .await?;
        if response.is_success() {
            Ok(())
        } else {
            Err(status_error(operation, url, response).await)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RestateInvocationStatus {
    pub id: String,
    pub target: String,
    pub target_service_name: String,
    #[serde(default)]
    pub pinned_deployment_id: Option<String>,
    #[serde(default)]
    pub target_service_key: Option<String>,
    pub target_handler_name: String,
    pub status: String,
    #[serde(default)]
    pub completion_result: Option<String>,
    #[serde(default)]
    pub completion_failure: Option<String>,
}

impl RestateInvocationStatus {
    pub fn invocation_id(&self) -> RestateInvocationId {
        RestateInvocationId::new(self.id.clone())
    }

    pub fn is_still_active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "pending" | "ready" | "running" | "backing-off" | "suspended"
        )
    }

    pub fn completed_successfully(&self) -> bool {
        self.status == "completed" && self.completion_result.as_deref() == Some("success")
    }
}

/// Number of open Restate invocations associated with one deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct DeploymentOpenInvocations {
    #[serde(default)]
    pub pinned_deployment_id: Option<String>,
    pub open_count: u64,
}

#[cfg(test)]
mod tests {
    use super::{DeploymentOpenInvocations, RestateInvocationStatus};

    #[test]
    fn invocation_status_deserializes_captured_rows_with_and_without_deployment() {
        let pinned: RestateInvocationStatus = serde_json::from_str(
            r#"{
                "id": "invocation-1",
                "target": "service/handler",
                "target_service_name": "service",
                "pinned_deployment_id": "deployment-a",
                "target_service_key": null,
                "target_handler_name": "handler",
                "status": "running",
                "completion_result": null,
                "completion_failure": null
            }"#,
        )
        .expect("pinned invocation row should deserialize");
        assert_eq!(pinned.pinned_deployment_id.as_deref(), Some("deployment-a"));

        let legacy: RestateInvocationStatus = serde_json::from_str(
            r#"{
                "id": "invocation-2",
                "target": "service/handler",
                "target_service_name": "service",
                "target_handler_name": "handler",
                "status": "pending"
            }"#,
        )
        .expect("row without a deployment column should deserialize");
        assert_eq!(legacy.pinned_deployment_id, None);
    }

    #[test]
    fn deployment_counts_deserialize_unpinned_rows_without_filtering_them() {
        let rows: Vec<DeploymentOpenInvocations> = serde_json::from_str(
            r#"[
                {"pinned_deployment_id": "deployment-a", "open_count": 2},
                {"pinned_deployment_id": null, "open_count": 1}
            ]"#,
        )
        .expect("deployment count rows should deserialize");

        assert_eq!(
            rows[0].pinned_deployment_id.as_deref(),
            Some("deployment-a")
        );
        assert_eq!(rows[0].open_count, 2);
        assert_eq!(rows[1].pinned_deployment_id, None);
        assert_eq!(rows[1].open_count, 1);
    }
}
fn format_restate_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

async fn status_error(
    operation: &'static str,
    url: String,
    response: RestateHttpResponse,
) -> RestateHttpError {
    let status = response.response.status;
    let timeout_message = format!(
        "{operation} {} response body timed out",
        response.deadline.class.name()
    );
    match read_http_body_bytes(
        response.response.body,
        Some(response.body_timeout),
        &timeout_message,
    )
    .await
    {
        Ok(body) => RestateHttpError::Status {
            operation,
            url,
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        },
        Err(source) => RestateHttpError::Request {
            operation,
            url,
            source,
        },
    }
}

async fn send_request(
    connection: &RestateConnection,
    class: RestateRequestClass,
    operation: &'static str,
    request: HttpRequest,
) -> Result<RestateHttpResponse, RestateHttpError> {
    let url = request.url.clone();
    let deadline = RestateRequestDeadline::new(class, class.timeout(&connection.config));
    let request = request.with_response_start_timeout_message(format!(
        "{operation} {} response start timed out",
        class.name()
    ));
    let response = connection
        .transport
        .send(request, Some(deadline.remaining()))
        .await
        .map_err(|source| RestateHttpError::Request {
            operation,
            url,
            source,
        })?;
    let body_timeout = deadline
        .remaining()
        .min(Duration::from_millis(connection.config.control_timeout_ms));
    Ok(RestateHttpResponse {
        response,
        deadline,
        body_timeout,
    })
}

async fn decode_response<T: DeserializeOwned>(
    operation: &'static str,
    url: &str,
    response: RestateHttpResponse,
) -> Result<T, RestateHttpError> {
    let timeout_message = format!(
        "{operation} {} response body timed out",
        response.deadline.class.name()
    );
    let body = read_http_body_bytes(
        response.response.body,
        Some(response.body_timeout),
        &timeout_message,
    )
    .await
    .map_err(|source| RestateHttpError::Request {
        operation,
        url: url.to_string(),
        source,
    })?;
    serde_json::from_slice(&body).map_err(|source| RestateHttpError::Decode {
        operation,
        url: url.to_string(),
        source,
    })
}

#[derive(Clone, Copy, Debug)]
enum RestateRequestClass {
    Control,
    Attach,
}

impl RestateRequestClass {
    fn timeout(self, config: &RestateConnectionConfig) -> Duration {
        match self {
            Self::Control => Duration::from_millis(config.control_timeout_ms),
            Self::Attach => Duration::from_millis(config.attach_ceiling_ms),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Control => "control deadline",
            Self::Attach => "attach ceiling",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RestateRequestDeadline {
    class: RestateRequestClass,
    expires_at: tokio::time::Instant,
}

impl RestateRequestDeadline {
    fn new(class: RestateRequestClass, timeout: Duration) -> Self {
        Self {
            class,
            expires_at: tokio::time::Instant::now() + timeout,
        }
    }

    fn remaining(self) -> Duration {
        self.expires_at
            .saturating_duration_since(tokio::time::Instant::now())
    }
}

#[derive(Debug)]
struct RestateHttpResponse {
    response: HttpResponse,
    deadline: RestateRequestDeadline,
    body_timeout: Duration,
}

impl RestateHttpResponse {
    fn is_success(&self) -> bool {
        self.response.is_success()
    }
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn matches_restate_accepted_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("accepted") || status.eq_ignore_ascii_case("previouslyaccepted")
}
