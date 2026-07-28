//! Restate durable execution adapter for Lash runtime effects.
//!
//! The primary entrypoint is [`RestateRuntimeEffectController`]. Construct it inside
//! a Restate service, object, or workflow handler, derive a stable
//! [`ScopedEffectController`](lash_core::ScopedEffectController) from an
//! [`ExecutionScope`](lash_core::ExecutionScope), and run Lash through the scoped API.
//! Restate recovery is handler replay with the same scope id and request data,
//! not Lash checkpoint reload.
//!
//! ```rust,ignore
//! use lash_restate::RestateRuntimeEffectController;
//! use restate_sdk::prelude::*;
//!
//! # #[derive(serde::Serialize, serde::Deserialize)]
//! # struct TurnRequest {
//! #     turn_id: String,
//! #     incarnation_id: lash_core::IncarnationId,
//! # }
//! # #[derive(serde::Serialize, serde::Deserialize)]
//! # struct TurnResponse;
//! # async fn run_lash_turn(
//! #     _scope: lash_core::ScopedEffectController<'_>,
//! #     _req: TurnRequest,
//! # ) -> Result<TurnResponse, std::io::Error> {
//! #     Ok(TurnResponse)
//! # }
//! #[restate_sdk::workflow]
//! pub trait AgentTurnWorkflow {
//!     async fn run(req: Json<TurnRequest>) -> HandlerResult<Json<TurnResponse>>;
//! }
//!
//! pub struct AgentTurnWorkflowImpl;
//!
//! impl AgentTurnWorkflow for AgentTurnWorkflowImpl {
//!     async fn run(
//!         &self,
//!         ctx: WorkflowContext<'_>,
//!         Json(req): Json<TurnRequest>,
//!     ) -> HandlerResult<Json<TurnResponse>> {
//!         let effect_controller = RestateRuntimeEffectController::new(ctx);
//!         let turn_id = req.turn_id.clone();
//!         let scoped_effect_controller = effect_controller
//!             .scoped_effect_controller(lash_core::ExecutionScope::turn_incarnation(
//!                 "session",
//!                 req.incarnation_id.clone(),
//!                 &turn_id,
//!             ))
//!             .map_err(TerminalError::from_error)?;
//!         let response = run_lash_turn(scoped_effect_controller, req)
//!             .await
//!             .map_err(TerminalError::from_error)?;
//!         Ok(Json(response))
//!     }
//! }
//! ```
//!
//! Restate's Rust SDK requires `ctx.run` closures to be awaited immediately and
//! not to call the Restate context from inside the closure. This adapter wraps
//! atomic Lash effects in immediately awaited
//! `ctx.run(...).name(lash:<replay_key>)` calls. Composite tool-batch and
//! exec-code interpreters are rebuilt on every handler attempt while their
//! nested atomic effects retain stable replay keys. Sleep commands map to
//! Restate's durable timer, and process commands call Restate workflow
//! scheduling directly through idempotent registry/workflow operations.
//! Substrate-native Restate turns do not use store-side in-flight replay rows;
//! Lash only commits final session state through turn-commit idempotency.
//!
//! Endpoints using this controller must also bind
//! [`LashDurableWaitWorkflowImpl`] and [`LashDurableWaitIndexImpl`]. The first
//! owns exact-address promises and durable deadline timers for every
//! [`ExecutionScope`]; the second indexes session-owned waits so cancellation
//! and deletion can resolve them durably.

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;
use std::time::Duration;

use lash_core::{
    AbandonEvidence, AbandonWriter, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity,
    CanonicalRuntimeEffectEnvelope, DurableProcessWorker, EffectHost, ExecutionScope, PluginError,
    ProcessAttach, ProcessAwaitOutput, ProcessCommand, ProcessCompletionAuthority,
    ProcessEffectOutcome, ProcessEventSink, ProcessExecutionContext, ProcessExternalRef,
    ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessRunHandle, ProcessWorkDriver,
    RecoveryDisposition, Resolution, ResolveOutcome, RuntimeAwaitEventOptions,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeError, RuntimeInvocation, RuntimeSleepOptions, ScopedEffectController, TurnAddress,
    TurnAttach, TurnTerminal, TurnWorkDriver, validate_replayed_effect_envelope,
    watch_process_registry_with_sink,
};
use lash_http_transport::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, HttpTransportError, ReqwestClient,
    ReqwestHttpTransport, read_http_body_bytes,
};
use restate_sdk::context::{
    Context as RestateContext, ObjectContext, RunRetryPolicy, SharedObjectContext,
    SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::context::{
    ContextAwakeables, ContextClient, ContextPromises, ContextReadState, ContextWriteState,
    InvocationHandle, RequestTarget,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

pub use restate_sdk;

/// Fuse a Restate context future across both of its terminal poll shapes.
///
/// `DurableFutureImpl` returns `Ready` on success, but after an internal error
/// it marks the handler failed, synchronously wakes the task, and returns
/// `Pending`. Its inner `Map` is already complete in that second shape and
/// panics if select teardown polls it again, so an ordinary `FutureExt::fuse`
/// cannot guard it. Stop polling after `Ready`; if the future synchronously
/// wakes and returns `Pending`, fail the handler attempt immediately so an
/// entry-scoped SDK error cannot be left recorded behind a parked outer future.
struct RestateContextFuture<F> {
    future: Option<Pin<Box<F>>>,
}

impl<F> Future for RestateContextFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(future) = self.future.as_mut() else {
            return Poll::Pending;
        };
        let tracker = Arc::new(SynchronousWakeTracker {
            parent: cx.waker().clone(),
            polling_thread: std::thread::current().id(),
            polling: AtomicBool::new(true),
            woke_during_poll: AtomicBool::new(false),
        });
        let tracked_waker = Waker::from(Arc::clone(&tracker));
        let mut tracked_context = Context::from_waker(&tracked_waker);
        let result = future.as_mut().poll(&mut tracked_context);
        tracker.polling.store(false, Ordering::Release);

        if result.is_ready() {
            self.future = None;
        } else if tracker.woke_during_poll.load(Ordering::Acquire) {
            panic!(
                "Restate context future returned Pending after a synchronous wake; failing the handler attempt"
            );
        }
        result
    }
}

struct SynchronousWakeTracker {
    parent: Waker,
    polling_thread: ThreadId,
    polling: AtomicBool,
    woke_during_poll: AtomicBool,
}

impl Wake for SynchronousWakeTracker {
    fn wake(self: Arc<Self>) {
        self.record_wake();
        self.parent.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.record_wake();
        self.parent.wake_by_ref();
    }
}

impl SynchronousWakeTracker {
    fn record_wake(&self) {
        if self.polling.load(Ordering::Acquire)
            && std::thread::current().id() == self.polling_thread
        {
            self.woke_during_poll.store(true, Ordering::Release);
        }
    }
}

fn guard_restate_context_future<F>(future: F) -> RestateContextFuture<F>
where
    F: Future,
{
    RestateContextFuture {
        future: Some(Box::pin(future)),
    }
}

fn restate_await_event_key(
    scope: &ExecutionScope,
    wait: AwaitEventWaitIdentity,
) -> Result<AwaitEventKey, RuntimeError> {
    let key_id = lash_core::promise_semantics::derive_key_id(scope, &wait)?;
    Ok(AwaitEventKey {
        scope: scope.clone(),
        wait,
        key_id,
        signature: "restate-handler".to_string(),
    })
}

fn restate_await_event_key_is_valid(key: &AwaitEventKey) -> bool {
    let Ok(expected) = restate_await_event_key(&key.scope, key.wait.clone()) else {
        return false;
    };
    lash_core::promise_semantics::constant_time_eq(
        expected.key_id.as_bytes(),
        key.key_id.as_bytes(),
    ) && lash_core::promise_semantics::constant_time_eq(
        expected.signature.as_bytes(),
        key.signature.as_bytes(),
    )
}

fn restate_unknown_or_revoked() -> RuntimeError {
    RuntimeError::new(
        "await_event_unknown_or_revoked",
        "await-event key is invalid or revoked",
    )
}

const DURABLE_WAIT_PROMISE_KEY: &str = "resolution";
const PROCESS_CANCEL_PROMISE_KEY: &str = "process_cancel_requested";
const PROCESS_CANCEL_CONFIRM_RETRIES: usize = 3;
const PROCESS_CANCEL_CONFIRM_RETRY_DELAY: Duration = Duration::from_millis(5);
const LEGACY_DURABLE_WAIT_INDEX_STATE_KEY: &str = "waits";
const DURABLE_WAIT_INDEX_METADATA_KEY: &str = "wait-index/v1/metadata";
const DURABLE_WAIT_INDEX_WAIT_PREFIX: &str = "wait-index/v1/wait/";
const DURABLE_WAIT_INDEX_RESOLUTION_PREFIX: &str = "wait-index/v1/resolution/";

/// Wall-clock epoch milliseconds for terminal evidence written at the Restate
/// tier (ADR 0019 recovery enforcement). The Restate boundary carries no
/// injected Lash clock — its durability comes from the engine and workflow-key
/// coalescing rather than a Lash lease — so it reads the system clock directly.
fn restate_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Completion authority for a row the Restate workflow ran itself. Restate's
/// single-writer discipline is per-`process_id` workflow-key coalescing, not a
/// Lash lease (ADR 0027); the workflow key is that `process_id`.
fn workflow_key_authority(process_id: &str) -> ProcessCompletionAuthority {
    ProcessCompletionAuthority::WorkflowKey {
        workflow_key: process_id.to_string(),
    }
}

fn process_segment_workflow_key(process_id: &str, segment_ordinal: u64) -> String {
    if segment_ordinal == 0 {
        process_id.to_string()
    } else {
        format!("{process_id}#{segment_ordinal}")
    }
}

fn terminal_completion_workflow_key(process_id: &str, segment_ordinal: u64) -> Option<String> {
    (segment_ordinal > 0).then(|| process_id.to_string())
}

fn retryable_registry_error(error: PluginError) -> HandlerError {
    HandlerError::from(error)
}

fn boundary_must_be_declined(record: Option<&ProcessRecord>) -> bool {
    record.is_some_and(|record| record.wait.is_some())
}

fn missing_segment_is_superseded(
    requested_ordinal: u64,
    latest: Option<&lash_core::PersistedSegmentHandover>,
) -> bool {
    latest.is_some_and(|handover| handover.segment_ordinal > requested_ordinal)
}

fn validate_segment_program_hash(
    process_id: &str,
    persisted: lash_core::PersistedSegmentHandover,
) -> Result<lash_core::SegmentHandover, RuntimeError> {
    if persisted.handover.program_hash.as_deref() != Some(persisted.program_hash.as_str()) {
        return Err(RuntimeError::new(
            "restate_segment_program_hash_mismatch",
            format!(
                "process `{process_id}` segment {} handover program identity is inconsistent",
                persisted.segment_ordinal
            ),
        ));
    }
    Ok(persisted.handover)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAddress {
    pub workflow_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub classification: RestateDurableWaitClassification,
}

impl RestateDurableWaitAddress {
    /// Derive the exact Restate workflow address for a Lash await-event key.
    pub fn for_key(key: &AwaitEventKey) -> Self {
        Self {
            workflow_key: format!("{:x}", Sha256::digest(key.key_id.as_bytes())),
            session_id: key.scope.session_id().map(str::to_string),
            classification: if key.wait.is_turn_control() {
                RestateDurableWaitClassification::TurnControl
            } else {
                RestateDurableWaitClassification::DurableWait
            },
        }
    }

    /// Return the keyed virtual-object address that owns this wait's index state.
    pub fn index_key(&self) -> String {
        self.session_id
            .clone()
            .unwrap_or_else(|| format!("unscoped:{}", self.workflow_key))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestateDurableWaitClassification {
    #[default]
    DurableWait,
    TurnControl,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAwaitRequest {
    pub address: RestateDurableWaitAddress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitResolveRequest {
    pub address: RestateDurableWaitAddress,
    pub resolution: Resolution,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitIndexRequest {
    pub address: RestateDurableWaitAddress,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitSettleRequest {
    pub address: RestateDurableWaitAddress,
    pub resolution: Resolution,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAwakeableRequest {
    pub address: RestateDurableWaitAddress,
    pub awakeable_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum RestateDurableWaitRegistration {
    Registered,
    Resolved(Resolution),
    Revoked,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
struct RestateDurableWaitIndexState {
    revoked: bool,
    waits: Vec<RestateDurableWaitAddress>,
    #[serde(default)]
    awakeables: Vec<RestateDurableWaitAwakeableRequest>,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
struct RestateDurableWaitIndexMetadata {
    revoked: bool,
    #[serde(default)]
    awakeables: Vec<RestateDurableWaitAwakeableRequest>,
}

fn restate_process_terminal_await_key(process_id: &str) -> Result<AwaitEventKey, RuntimeError> {
    restate_await_event_key(
        &ExecutionScope::process(process_id.to_string()),
        AwaitEventWaitIdentity::Custom {
            key: "process_terminal".to_string(),
        },
    )
}

fn restate_process_terminal_resolution(
    output: &ProcessAwaitOutput,
) -> Result<Resolution, RuntimeError> {
    serde_json::to_value(output)
        .map(Resolution::Ok)
        .map_err(|err| RuntimeError::new("restate_process_terminal_encode", err.to_string()))
}

fn restate_process_terminal_output(
    process_id: &str,
    resolution: Resolution,
) -> Result<ProcessAwaitOutput, PluginError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value).map_err(|err| {
            PluginError::Session(format!(
                "invalid terminal output for process `{process_id}`: {err}"
            ))
        }),
        Resolution::Err(err) => Ok(ProcessAwaitOutput::Failure {
            class: lash_core::ToolFailureClass::Execution,
            code: err.code,
            message: err.message,
            raw: None,
            control: None,
        }),
        Resolution::Timeout => Ok(ProcessAwaitOutput::Failure {
            class: lash_core::ToolFailureClass::Execution,
            code: "process_await_timeout".to_string(),
            message: format!("awaiting process `{process_id}` timed out"),
            raw: None,
            control: None,
        }),
        Resolution::Cancelled => Ok(ProcessAwaitOutput::Failure {
            class: lash_core::ToolFailureClass::Execution,
            code: "process_await_cancelled".to_string(),
            message: format!("awaiting process `{process_id}` was cancelled"),
            raw: None,
            control: None,
        }),
    }
}

fn resolve_process_terminal_promise<'ctx, C>(
    context: &C,
    process_id: &str,
    output: &ProcessAwaitOutput,
) -> HandlerResult<()>
where
    C: ContextPromises<'ctx>,
{
    let key = restate_process_terminal_await_key(process_id)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    let resolution = restate_process_terminal_resolution(output)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    let payload = serde_json::to_string(&resolution)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    context.resolve_promise(&key.promise_key(), payload);
    Ok(())
}

fn resolve_process_cancel_signal<'ctx, C>(
    context: &C,
    signal: RestateProcessCancelSignal,
) -> HandlerResult<()>
where
    C: ContextPromises<'ctx>,
{
    let payload = serde_json::to_string(&signal)
        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
    context.resolve_promise(PROCESS_CANCEL_PROMISE_KEY, payload);
    Ok(())
}

async fn resolve_restate_await_event<'ctx, C>(
    context: &C,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<ResolveOutcome, RuntimeError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    context
        .resolve_event(RestateDurableWaitResolveRequest {
            address: RestateDurableWaitAddress::for_key(key),
            resolution,
        })
        .await
        .map_err(|err| RuntimeError::new("restate_await_event_resolve", err.to_string()))
}

fn restate_durable_wait_request(
    key: &AwaitEventKey,
    deadline: Option<std::time::Instant>,
    clock: &dyn lash_core::Clock,
) -> RestateDurableWaitAwaitRequest {
    let timeout_ms = deadline.map(|deadline| {
        u64::try_from(deadline.saturating_duration_since(clock.now()).as_millis())
            .unwrap_or(u64::MAX)
    });
    RestateDurableWaitAwaitRequest {
        address: RestateDurableWaitAddress::for_key(key),
        timeout_ms,
    }
}

fn restate_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    let Some(turn_id) = invocation.scope.turn_id.as_deref() else {
        return Ok(None);
    };
    let Some(scope) = turn_cancel_scope else {
        return Err(RuntimeEffectControllerError::new(
            "restate_turn_cancel_scope_missing",
            "turn effects that observe cancellation require a durable turn-cancel scope",
        ));
    };
    if matches!(scope, ExecutionScope::Process { .. }) {
        return Ok(None);
    }
    let scope @ ExecutionScope::Turn { .. } = scope else {
        return Err(RuntimeEffectControllerError::new(
            "restate_turn_cancel_scope_mismatch",
            "turn-cancel scope must be a matching turn scope or an explicit process scope",
        ));
    };
    scope
        .validate_for_durable_host()
        .map_err(RuntimeEffectControllerError::from)?;
    if scope.session_id() != Some(invocation.scope.session_id.as_str())
        || scope.turn_id() != Some(turn_id)
    {
        return Err(RuntimeEffectControllerError::new(
            "restate_turn_cancel_scope_mismatch",
            "turn-cancel scope must match the runtime effect invocation",
        ));
    }
    let key = restate_await_event_key(scope, AwaitEventWaitIdentity::TurnCancelGate)?;
    Ok(Some(restate_durable_wait_request(
        &key,
        None,
        &lash_core::SystemClock,
    )))
}

fn restate_timer_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(invocation, turn_cancel_scope)
}

fn restate_await_event_turn_cancel_wait_request(
    invocation: &RuntimeInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(invocation, turn_cancel_scope)
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
struct RecordedRuntimeEffect {
    envelope: Arc<CanonicalRuntimeEffectEnvelope>,
    outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

/// Error raised while bridging a Lash effect to Restate.
#[derive(Debug, thiserror::Error)]
pub enum RestateEffectError {
    #[error("Restate terminal error while running `{effect}`: {terminal}")]
    Terminal {
        effect: String,
        terminal: TerminalError,
    },
    #[error("Restate background scheduler error: {0}")]
    BackgroundScheduler(String),
}

impl RestateEffectError {
    fn into_plugin_error(self) -> PluginError {
        PluginError::Session(self.to_string())
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
/// let client = lash_http_transport::ReqwestClient::builder()
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
}

impl RestateConnection {
    pub fn new(ingress_url: impl Into<String>) -> Self {
        Self::with_transport(ingress_url, Arc::new(ReqwestHttpTransport::new()))
    }

    pub fn with_transport(
        ingress_url: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            ingress_url: ingress_url.into(),
            transport,
        }
    }

    pub fn with_client(ingress_url: impl Into<String>, client: ReqwestClient) -> Self {
        Self::with_transport(
            ingress_url,
            Arc::new(ReqwestHttpTransport::from_client(client)),
        )
    }

    pub fn ingress_url(&self) -> &str {
        &self.ingress_url
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

    async fn call_workflow_json_idempotent<T, R>(
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
        let response = send_request(&self.connection, "Restate workflow call", request).await?;
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
    ) -> Result<HttpResponse, RestateHttpError> {
        let body = serde_json::to_vec(body).map_err(|source| RestateHttpError::Encode {
            operation,
            url: url.to_string(),
            source,
        })?;
        send_request(
            &self.connection,
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
            "SELECT id, target, target_service_name, target_service_key, target_handler_name, status, completion_result, completion_failure FROM sys_invocation WHERE status IN ('pending', 'ready', 'running', 'backing-off', 'suspended') AND ({service_filter}) ORDER BY modified_at DESC"
        ))
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
        let response = send_request(&self.connection, "Restate SQL query", request).await?;
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
    response: HttpResponse,
) -> RestateHttpError {
    let status = response.status;
    let body = read_http_body_bytes(response.body, None, "Restate response body timed out")
        .await
        .map(|body| String::from_utf8_lossy(&body).into_owned())
        .unwrap_or_default();
    RestateHttpError::Status {
        operation,
        url,
        status,
        body,
    }
}

async fn send_request(
    connection: &RestateConnection,
    operation: &'static str,
    request: HttpRequest,
) -> Result<HttpResponse, RestateHttpError> {
    let url = request.url.clone();
    connection
        .transport
        .send(request, None)
        .await
        .map_err(|source| RestateHttpError::Request {
            operation,
            url,
            source,
        })
}

async fn decode_response<T: DeserializeOwned>(
    operation: &'static str,
    url: &str,
    response: HttpResponse,
) -> Result<T, RestateHttpError> {
    let body = read_http_body_bytes(response.body, None, "Restate response body timed out")
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

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn matches_restate_accepted_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("accepted") || status.eq_ignore_ascii_case("previouslyaccepted")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RestateProcessCancelRequest {
    pub process_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[async_trait::async_trait]
pub trait RestateProcessRunner: Send + Sync + 'static {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError>;

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError>;
}

#[derive(Clone)]
pub struct RestateCoreProcessRunner {
    worker: DurableProcessWorker,
}

impl RestateCoreProcessRunner {
    pub fn new(worker: DurableProcessWorker) -> Self {
        Self { worker }
    }

    pub fn worker(&self) -> &DurableProcessWorker {
        &self.worker
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for RestateCoreProcessRunner {
    async fn run_process_segment(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let execution_write_authority = execution_context
            .execution_write_authority
            .clone()
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "Restate process `{}` omitted its invocation execution identity",
                    registration.id
                ))
            })?;
        Box::pin(
            self.worker
                .run_process_segment_with_scoped_effect_controller(
                    registration,
                    execution_context,
                    execution_write_authority,
                    scoped_effect_controller,
                    cancellation,
                    handover,
                ),
        )
        .await
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.worker
            .request_process_cancel(&request.process_id, request.reason)
            .await
    }
}

/// Deployment-level Restate effect host for long-lived Lash cores.
///
/// Restate's real effect execution requires a handler context, so this host is
/// intentionally a durable boundary, not an executor. HTTP/API code should
/// enter a Restate workflow/object first and then pass
/// [`RestateRuntimeEffectController::scoped_effect_controller`] into Lash. If a
/// caller tries to execute through this deployment host directly, it fails
/// loudly instead of falling back to inline execution.
#[derive(Clone)]
pub struct RestateEffectHost {
    controller: Arc<RestateEffectHostController>,
}

impl RestateEffectHost {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        Self {
            controller: Arc::new(RestateEffectHostController {
                await_event_ingress: RestateAwaitEventIngress {
                    ingress: RestateIngressClient::new(connection),
                },
            }),
        }
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHost {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.controller.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.controller.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.controller.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }
}

impl EffectHost for RestateEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate_for_durable_host()?;
        ScopedEffectController::shared(self.controller.clone(), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scope.validate_for_durable_host()?;
        Ok(Some(ScopedEffectController::shared(
            self.controller.clone(),
            scope,
        )?))
    }
}

#[derive(Clone)]
struct RestateAwaitEventIngress {
    ingress: RestateIngressClient,
}

async fn resolve_restate_await_event_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<ResolveOutcome, RuntimeError> {
    let address = RestateDurableWaitAddress::for_key(key);
    let request = RestateDurableWaitResolveRequest {
        address,
        resolution,
    };
    let index_key = durable_wait_index_object_key(&request.address);
    let outcome = ingress
        .ingress
        .call_object_json::<_, ResolveOutcome>(
            "LashDurableWaitIndex",
            &index_key,
            "resolve",
            &request,
        )
        .await;
    outcome.map_err(|err| RuntimeError::new("restate_await_event_resolve", err.to_string()))
}

async fn update_restate_session_waits_via_ingress(
    ingress: &RestateAwaitEventIngress,
    session_id: &str,
    revoke: bool,
) -> Result<(), RuntimeError> {
    let handler = if revoke { "revoke_all" } else { "cancel_all" };
    ingress
        .ingress
        .call_object_empty("LashDurableWaitIndex", session_id, handler)
        .await
        .map_err(|err| RuntimeError::new("restate_await_event_session_update", err.to_string()))
}

async fn restate_session_is_revoked_via_ingress(
    ingress: &RestateAwaitEventIngress,
    session_id: &str,
) -> Result<bool, RuntimeError> {
    ingress
        .ingress
        .call_object_json::<_, bool>("LashDurableWaitIndex", session_id, "is_revoked", &())
        .await
        .map_err(|err| RuntimeError::new("restate_await_event_revocation_read", err.to_string()))
}

async fn ensure_restate_key_access_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
) -> Result<(), RuntimeError> {
    if !restate_await_event_key_is_valid(key) {
        return Err(restate_unknown_or_revoked());
    }
    if let Some(session_id) = key.scope.session_id()
        && restate_session_is_revoked_via_ingress(ingress, session_id).await?
    {
        return Err(restate_unknown_or_revoked());
    }
    Ok(())
}

async fn await_restate_await_event_via_ingress(
    ingress: &RestateAwaitEventIngress,
    key: &AwaitEventKey,
    cancel: tokio_util::sync::CancellationToken,
    deadline: Option<std::time::Instant>,
    effect_replay_key: Option<&str>,
) -> Result<Resolution, RuntimeError> {
    let request = restate_durable_wait_request(key, deadline, &lash_core::SystemClock);
    let workflow_key = request.address.workflow_key.clone();
    tokio::select! {
        result = async {
            match effect_replay_key {
                Some(replay_key) => ingress.ingress.call_workflow_json_idempotent::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &request,
                    replay_key,
                ).await,
                None => ingress.ingress.call_workflow_json::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &request,
                ).await,
            }
        } => result.map_err(|err| {
            RuntimeError::new("restate_await_event_await", err.to_string())
        }),
        _ = cancel.cancelled() => Err(RuntimeError::new(
            "restate_await_event_cancelled",
            "Restate await-event ingress observation was cancelled locally",
        )),
    }
}

struct RestateEffectHostController {
    await_event_ingress: RestateAwaitEventIngress,
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHostController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate_for_durable_host()?;
        let ingress = &self.await_event_ingress;
        if let Some(session_id) = scope.session_id()
            && restate_session_is_revoked_via_ingress(ingress, session_id).await?
        {
            return Err(restate_unknown_or_revoked());
        }
        restate_await_event_key(scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event_via_ingress(&self.await_event_ingress, key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        let ingress = &self.await_event_ingress;
        ensure_restate_key_access_via_ingress(ingress, key).await?;
        let workflow_key = RestateDurableWaitAddress::for_key(key).workflow_key;
        ingress
            .ingress
            .call_workflow_empty::<Option<Resolution>>(
                "LashDurableWaitWorkflow",
                &workflow_key,
                "peek",
            )
            .await
            .map_err(|err| RuntimeError::new("restate_await_event_peek", err.to_string()))
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        let ingress = &self.await_event_ingress;
        ensure_restate_key_access_via_ingress(ingress, key).await?;
        await_restate_await_event_via_ingress(ingress, key, cancel, deadline, None).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let ingress = &self.await_event_ingress;
        update_restate_session_waits_via_ingress(ingress, session_id, true).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let ingress = &self.await_event_ingress;
        update_restate_session_waits_via_ingress(ingress, session_id, false).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RestateEffectHostController {
    fn supports_concurrent_effects(&self) -> bool {
        false
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let effect_replay_key = envelope.stable_hash()?;
        if let RuntimeEffectCommand::AwaitEvent { key } = &envelope.command {
            if !restate_await_event_key_is_valid(key) {
                return Err(RuntimeEffectControllerError::from(
                    restate_unknown_or_revoked(),
                ));
            }
            let ingress = &self.await_event_ingress;
            let RuntimeAwaitEventOptions {
                cancellation,
                deadline,
                ..
            } = local_executor.into_await_event_options()?;
            let resolution = await_restate_await_event_via_ingress(
                ingress,
                key,
                cancellation,
                deadline,
                Some(&effect_replay_key),
            )
            .await
            .map_err(RuntimeEffectControllerError::from)?;
            return Ok(RuntimeEffectOutcome::AwaitEvent { resolution });
        }
        Err(RuntimeEffectControllerError::new(
            "restate_effect_host_requires_handler_scope",
            format!(
                "effect `{}` must enter a Restate handler and use RestateRuntimeEffectController::scoped_effect_controller",
                envelope
                    .invocation
                    .effect_id()
                    .unwrap_or_else(|| envelope.command.kind().as_str())
            ),
        ))
    }
}

/// [`ProcessRunHandle`] that drives pending processes by submitting their
/// `LashProcessWorkflow` through the Restate ingress instead of running them
/// in-process.
///
/// This is the controller-owned process work handle: a host-owned
/// [`ProcessWorkDriver`] calls it on ingress-relevant events. Per row, it POSTs
/// `LashProcessWorkflow/{process_id}/run/send` to the ingress. Restate
/// coalesces by workflow key, so duplicate submits are idempotent and no Lash
/// registry lease is needed at the Restate tier.
pub struct RestateProcessIngressRunner {
    ingress: RestateIngressClient,
    registry: Arc<dyn ProcessRegistry>,
}

impl RestateProcessIngressRunner {
    /// Build an ingress-client run handle over the given ingress base URL and
    /// process registry.
    pub fn new(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
    ) -> Self {
        Self {
            ingress: RestateIngressClient::new(connection),
            registry,
        }
    }

    async fn submit_record(&self, record: ProcessRecord) -> Result<(), PluginError> {
        let process_id = record.id.clone();
        // ExternallyOwned rows are never executed by Lash (ADR 0019). Defensively
        // refuse to POST a run for one even when reached directly, so both the
        // sweep and any direct caller are safe; their closure comes from an
        // external actor calling `complete_process` or a reconciled Abandon
        // Request (see `claim_and_run_pending`).
        if record.disposition == RecoveryDisposition::ExternallyOwned {
            return Ok(());
        }
        // The record may have reached a terminal state between the list and the submit.
        // Idempotent by process_id: never re-submit a finished process.
        if self
            .registry
            .get_process(&process_id)
            .await
            .is_some_and(|current| current.is_terminal())
        {
            return Ok(());
        }
        let latest_handover = self.registry.latest_segment_handover(&process_id).await?;
        let segment_ordinal = latest_handover
            .as_ref()
            .map_or(0, |handover| handover.segment_ordinal);
        let workflow_key = process_segment_workflow_key(&process_id, segment_ordinal);
        let registration = ProcessRegistration {
            id: record.id,
            input: record.input,
            disposition: record.disposition,
            max_attempts: record.max_attempts,
            identity: record.identity,
            event_types: record.event_types,
            provenance: record.provenance.clone(),
            env_ref: record.env_ref,
            wake_target: record.wake_target,
        };
        let execution_context = ProcessExecutionContext::default();
        let invocation_id = self
            .ingress
            .send_workflow_json(
                "LashProcessWorkflow",
                &workflow_key,
                "run",
                &RestateProcessWorkflowInput {
                    registration,
                    execution_context,
                    segment_ordinal,
                    // An ingress/sweep submission is a fresh invocation. For a
                    // mid-chain row the handler validates the durable handover,
                    // then binds this invocation as the next execution attempt.
                    execution_id: None,
                },
            )
            .await
            .map_err(|err| {
                RestateEffectError::BackgroundScheduler(format!(
                    "ingress submit for process `{process_id}` failed: {err}"
                ))
                .into_plugin_error()
            })?;
        // Record the durable backend reference so the process is observably
        // owned by Restate, mirroring `schedule_restate_process`.
        self.registry
            .set_external_ref(
                &process_id,
                ProcessExternalRef {
                    backend: "restate".to_string(),
                    id: format!("LashProcessWorkflow/{process_id}"),
                    metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
                },
            )
            .await
            .map(|_| ())
    }

    /// Reconcile a pending Abandon Request on an externally-owned row into an
    /// `Abandoned{ReconciledRequest}` terminal, mirroring the core sweep's
    /// `reconcile_externally_owned_abandon`.
    ///
    /// Lash never executed the row, so there is no execution owner to name
    /// (`owner: None`). The Restate tier holds no Lash lease — workflow-key
    /// coalescing is its single-writer discipline — so the terminal is written
    /// directly after re-checking the row is still non-terminal (it may have
    /// been completed between the worklist scan and here). The decorated
    /// registry emits the resulting terminal append through the event sink.
    async fn reconcile_externally_owned_abandon(
        &self,
        process_id: &str,
    ) -> Result<(), PluginError> {
        if self
            .registry
            .get_process(process_id)
            .await
            .is_some_and(|current| current.is_terminal())
        {
            return Ok(());
        }
        self.registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Abandoned {
                    evidence: Box::new(AbandonEvidence {
                        writer: AbandonWriter::ReconciledRequest,
                        owner: None,
                        epoch_ms: restate_now_ms(),
                    }),
                    control: None,
                },
                ProcessCompletionAuthority::ReconciledAbandon,
            )
            .await
            .map(|_| ())
    }
}

#[async_trait::async_trait]
impl ProcessRunHandle for RestateProcessIngressRunner {
    async fn claim_and_run_pending(&self) -> Result<(), PluginError> {
        for record in self.registry.list_non_terminal().await? {
            // ExternallyOwned rows are never submitted to ingress (ADR 0019):
            // Lash does not execute them at the Restate tier either. A pending
            // Abandon Request on such a row is reconciled into an Abandoned
            // terminal here, mirroring the core sweep's
            // `reconcile_externally_owned_abandon`; rows without a request are
            // left untouched for their external owner to complete.
            if record.disposition == RecoveryDisposition::ExternallyOwned {
                if record.abandon_request.is_some() {
                    self.reconcile_externally_owned_abandon(&record.id).await?;
                }
                continue;
            }
            self.submit_record(record).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProcessAttach for RestateProcessIngressRunner {
    async fn await_terminal(&self, process_id: &str) -> Result<ProcessAwaitOutput, PluginError> {
        let record = self.registry.try_get_process(process_id).await?;
        if let Some(output) = record
            .as_ref()
            .and_then(|record| record.status.await_output())
        {
            return Ok(output.clone());
        }
        self.ingress
            .call_workflow_json::<_, ProcessAwaitOutput>(
                "LashProcessWorkflow",
                process_id,
                "await_terminal",
                &RestateProcessAwaitRequest {
                    process_id: process_id.to_string(),
                },
            )
            .await
            .map_err(|err| {
                RestateEffectError::BackgroundScheduler(format!(
                    "ingress await for process `{process_id}` failed: {err}"
                ))
                .into_plugin_error()
            })
    }
}

/// Bundled Restate process deployment wiring for a Lash core.
///
/// Construct this once per deployment, pass [`process_work_driver`](Self::process_work_driver)
/// into `LashCoreBuilder::process_work_driver`, and bind
/// [`workflow`](Self::workflow) on the Restate endpoint.
pub struct RestateProcessDeployment {
    driver: ProcessWorkDriver,
    ingress: RestateIngressClient,
}

impl RestateProcessDeployment {
    pub fn new(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
    ) -> Self {
        Self::new_with_sink(connection, registry, None)
    }

    /// Like [`new`](Self::new), but installs a host-facing
    /// [`ProcessEventSink`] on the registry decorator this deployment wraps.
    ///
    /// The wrap happens inside the constructor, so the sink must be supplied
    /// here; each appended event is pushed best-effort after its durable write.
    /// See [`ProcessEventSink`] for the freshness-not-truth contract.
    pub fn new_with_sink(
        connection: impl Into<RestateConnection>,
        registry: Arc<dyn ProcessRegistry>,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        let connection = connection.into();
        let (registry, hub) = watch_process_registry_with_sink(registry, sink);
        let ingress_runner = Arc::new(RestateProcessIngressRunner::new(
            connection.clone(),
            Arc::clone(&registry),
        ));
        let run_handle: Arc<dyn ProcessRunHandle> = ingress_runner.clone();
        let attach: Arc<dyn ProcessAttach> = ingress_runner;
        let driver = ProcessWorkDriver::from_watched(registry, hub, run_handle).with_attach(attach);
        Self {
            driver,
            ingress: RestateIngressClient::new(connection),
        }
    }

    pub fn process_work_driver(&self) -> ProcessWorkDriver {
        self.driver.clone()
    }

    pub fn workflow(
        &self,
        worker: DurableProcessWorker,
    ) -> LashProcessWorkflowImpl<RestateCoreProcessRunner> {
        LashProcessWorkflowImpl::new(
            Arc::new(RestateCoreProcessRunner::new(worker)),
            self.driver.process_registry(),
            self.ingress.clone(),
        )
    }
}

#[restate_sdk::workflow]
pub trait LashProcessWorkflow {
    async fn run(
        input: Json<RestateProcessWorkflowInput>,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>>;

    #[shared]
    async fn complete_terminal(
        request: Json<RestateProcessCompleteRequest>,
    ) -> HandlerResult<Json<()>>;

    #[shared]
    async fn await_terminal(
        request: Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>>;

    #[shared]
    async fn cancel(request: Json<RestateProcessCancelRequest>) -> HandlerResult<Json<()>>;

    #[shared]
    async fn deliver_cancel(request: Json<RestateProcessCancelRequest>) -> HandlerResult<Json<()>>;

    #[shared]
    async fn await_cancel(
        request: Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<RestateProcessCancelSignal>>;
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessWorkflowInput {
    pub registration: ProcessRegistration,
    #[serde(default, skip_serializing_if = "ProcessExecutionContext::is_empty")]
    pub execution_context: ProcessExecutionContext,
    #[serde(default)]
    pub segment_ordinal: u64,
    /// Root Restate invocation id for this execution attempt. Segment
    /// successors carry it forward so a process chain remains one attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

fn segment_execution_authority(
    process_id: &str,
    segment_ordinal: u64,
    carried_execution_id: Option<&str>,
    invocation_id: &str,
    retained_start: Option<&lash_core::ProcessStarted>,
) -> Result<(String, lash_core::ProcessExecutionWriteAuthority), TerminalError> {
    if segment_ordinal == 0 {
        let execution_id = invocation_id.to_string();
        return Ok((
            execution_id.clone(),
            lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id),
        ));
    }

    let retained_start = retained_start.ok_or_else(|| {
        TerminalError::new(format!(
            "process `{process_id}` segment {segment_ordinal} has a handover without a retained execution start"
        ))
    })?;
    if let Some(carried_execution_id) = carried_execution_id {
        let retained_execution_id = retained_start
            .owner
            .restate_process_execution_id(process_id)
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "process `{process_id}` segment {segment_ordinal} retained a non-Restate execution owner"
                ))
            })?;
        if carried_execution_id != retained_execution_id {
            return Err(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} carried execution `{carried_execution_id}` but retained execution is `{retained_execution_id}`"
            )));
        }
        let execution_id = carried_execution_id.to_string();
        return Ok((
            execution_id.clone(),
            lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id),
        ));
    }

    let execution_id = invocation_id.to_string();
    Ok((
        execution_id.clone(),
        lash_core::ProcessExecutionWriteAuthority::invocation_resume(
            process_id,
            execution_id,
            retained_start.clone(),
        ),
    ))
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestateProcessWorkflowOutput {
    Terminal { output: Box<ProcessAwaitOutput> },
    SegmentChained { next_segment_ordinal: u64 },
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessCompleteRequest {
    pub process_id: String,
    pub output: ProcessAwaitOutput,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateProcessAwaitRequest {
    pub process_id: String,
}

/// Terminal value for one process segment's durable cancellation observer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestateProcessCancelSignal {
    /// The cancel endpoint accepted a request and wrote its durable registry event.
    CancelRequested,
    /// The segment ended normally, so its cancellation observer can retire.
    SegmentFinished,
}

pub struct LashProcessWorkflowImpl<R> {
    runner: Arc<R>,
    registry: Arc<dyn ProcessRegistry>,
    segment_duration_cap: Option<Duration>,
    segment_effect_budget: Arc<dyn Fn(&ProcessRegistration) -> u64 + Send + Sync>,
    cancel_ingress: Option<RestateIngressClient>,
    #[cfg(test)]
    cancel_read_failures: std::sync::atomic::AtomicUsize,
}

impl<R> LashProcessWorkflowImpl<R> {
    /// Build a Restate process workflow whose live cancellation observer calls
    /// back through the ingress to await its durable cancellation promise.
    pub fn new(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        cancel_ingress: RestateIngressClient,
    ) -> Self {
        Self::new_inner(runner, registry, Some(cancel_ingress))
    }

    #[cfg(test)]
    fn new_for_test(runner: Arc<R>, registry: Arc<dyn ProcessRegistry>) -> Self {
        Self::new_inner(runner, registry, None)
    }

    fn new_inner(
        runner: Arc<R>,
        registry: Arc<dyn ProcessRegistry>,
        cancel_ingress: Option<RestateIngressClient>,
    ) -> Self {
        Self {
            runner,
            registry,
            segment_duration_cap: None,
            segment_effect_budget: Arc::new(|_| 10_000),
            cancel_ingress,
            #[cfg(test)]
            cancel_read_failures: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn with_segment_duration_cap(mut self, cap: Duration) -> Self {
        self.segment_duration_cap = Some(cap);
        self
    }

    /// Select a deterministic completed-effect budget from immutable process
    /// registration data. This is primarily useful for conformance/e2e pairs
    /// that run the same artifact with and without forced segmentation; the
    /// production default remains 10,000 completed effects per incarnation.
    pub fn with_segment_effect_budget_selector(
        mut self,
        selector: impl Fn(&ProcessRegistration) -> u64 + Send + Sync + 'static,
    ) -> Self {
        self.segment_effect_budget = Arc::new(selector);
        self
    }

    fn cancellation_signal(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HandlerError>> + Send>> {
        let Some(ingress) = self.cancel_ingress.clone() else {
            return Box::pin(std::future::pending());
        };
        let workflow_key = process_segment_workflow_key(process_id, segment_ordinal);
        let request = RestateProcessAwaitRequest {
            process_id: process_id.to_string(),
        };
        Box::pin(async move {
            match ingress
                .call_workflow_json::<_, RestateProcessCancelSignal>(
                    "LashProcessWorkflow",
                    &workflow_key,
                    "await_cancel",
                    &request,
                )
                .await
                .map_err(HandlerError::from)
            {
                Ok(RestateProcessCancelSignal::CancelRequested) => Ok(()),
                Ok(RestateProcessCancelSignal::SegmentFinished) => {
                    std::future::pending::<Result<(), HandlerError>>().await
                }
                Err(error) => Err(error),
            }
        })
    }

    #[cfg(test)]
    fn fail_next_cancel_reads(&self, count: usize) {
        self.cancel_read_failures
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<R> LashProcessWorkflowImpl<R>
where
    R: RestateProcessRunner,
{
    async fn complete_with_stored_outcome(
        &self,
        process_id: &str,
        proposed: ProcessAwaitOutput,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let completion = self
            .registry
            .complete_process(process_id, proposed, workflow_key_authority(process_id))
            .await?;
        let record = match completion {
            lash_core::ProcessCompletionOutcome::Committed(record) => record,
            lash_core::ProcessCompletionOutcome::AlreadyApplied { stored }
            | lash_core::ProcessCompletionOutcome::Superseded { stored } => stored,
        };
        record.status.await_output().cloned().ok_or_else(|| {
            PluginError::Session(format!(
                "process `{process_id}` completion returned a non-terminal record"
            ))
        })
    }

    async fn run_registration<F>(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        segment_ordinal: u64,
        handover: Option<lash_core::SegmentHandover>,
        cancellation_signal: F,
    ) -> Result<lash_core::ProcessRunOutcome, HandlerError>
    where
        F: Future<Output = Result<(), HandlerError>>,
    {
        let process_id = registration.id.clone();
        if segment_ordinal > 0 && handover.is_none() {
            return Err(HandlerError::from(TerminalError::new(format!(
                "process `{process_id}` segment {segment_ordinal} omitted its validated handover"
            ))));
        }
        if self
            .process_cancel_requested(&process_id)
            .await
            .map_err(retryable_registry_error)?
        {
            let output = ProcessAwaitOutput::Cancelled {
                message: format!("process `{process_id}` was cancelled"),
                raw: None,
                control: None,
            };
            let output = self
                .complete_with_stored_outcome(&process_id, output)
                .await
                .map_err(retryable_registry_error)?;
            return Ok(output.into());
        }
        let cancellation = tokio_util::sync::CancellationToken::new();
        let runner = self.runner.run_process_segment(
            registration,
            execution_context,
            scoped_effect_controller,
            handover,
            cancellation.clone(),
        );
        tokio::pin!(runner);
        tokio::pin!(cancellation_signal);
        let outcome = tokio::select! {
            biased;
            signal = &mut cancellation_signal => {
                signal?;
                cancellation.cancel();
                self.confirm_process_cancel_requested(&process_id)
                    .await
                    .map_err(retryable_registry_error)?;
                let _ = runner.await;
                Ok(lash_core::ProcessRunOutcome::Terminal(Box::new(
                    ProcessAwaitOutput::Cancelled {
                        message: format!("process `{process_id}` was cancelled"),
                        raw: None,
                        control: None,
                    },
                )))
            }
            outcome = &mut runner => outcome
        };
        match outcome {
            Ok(lash_core::ProcessRunOutcome::Terminal(output)) => {
                let stored = self
                    .complete_with_stored_outcome(&process_id, (*output).clone())
                    .await
                    .map_err(retryable_registry_error)?;
                Ok(lash_core::ProcessRunOutcome::Terminal(Box::new(stored)))
            }
            Ok(boundary @ lash_core::ProcessRunOutcome::SegmentBoundary(_)) => Ok(boundary),
            Err(PluginError::ProcessAlreadyStarted { by, .. }) => {
                let output = self
                    .complete_with_stored_outcome(
                        &process_id,
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer: AbandonWriter::Sweep,
                                owner: Some(*by),
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        },
                    )
                    .await
                    .map_err(retryable_registry_error)?;
                Ok(output.into())
            }
            Err(PluginError::ProcessAttemptsExhausted { .. }) => {
                let owner = self
                    .registry
                    .try_get_process(&process_id)
                    .await
                    .map_err(retryable_registry_error)?
                    .and_then(|record| record.first_started.map(|started| started.owner.clone()));
                let output = self
                    .complete_with_stored_outcome(
                        &process_id,
                        ProcessAwaitOutput::Abandoned {
                            evidence: Box::new(AbandonEvidence {
                                writer: AbandonWriter::EngineGaveUp,
                                owner,
                                epoch_ms: restate_now_ms(),
                            }),
                            control: None,
                        },
                    )
                    .await
                    .map_err(retryable_registry_error)?;
                Ok(output.into())
            }
            Err(err) => Err(retryable_registry_error(err)),
        }
    }

    async fn process_cancel_requested(&self, process_id: &str) -> Result<bool, PluginError> {
        #[cfg(test)]
        if self
            .cancel_read_failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(PluginError::Session(
                "simulated transient cancel registry read failure".to_string(),
            ));
        }
        Ok(self
            .registry
            .events_after(process_id, 0)
            .await?
            .iter()
            .any(|event| event.event_type == "process.cancel_requested"))
    }

    async fn confirm_process_cancel_requested(&self, process_id: &str) -> Result<(), PluginError> {
        let mut last_error = None;
        for attempt in 0..=PROCESS_CANCEL_CONFIRM_RETRIES {
            match self.process_cancel_requested(process_id).await {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    return Err(PluginError::Session(format!(
                        "process `{process_id}` cancellation promise resolved without a durable process.cancel_requested event"
                    )));
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < PROCESS_CANCEL_CONFIRM_RETRIES {
                        tokio::time::sleep(PROCESS_CANCEL_CONFIRM_RETRY_DELAY).await;
                    }
                }
            }
        }
        Err(last_error.expect("cancel confirmation attempted at least once"))
    }

    async fn record_cancel_requested(
        &self,
        request: &RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.registry
            .append_event(
                &request.process_id,
                lash_core::ProcessEventAppendRequest::cancel_requested(
                    &request.process_id,
                    request.reason.clone(),
                ),
            )
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    async fn cancel_registration(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), PluginError> {
        self.record_cancel_requested(&request).await?;
        self.runner.request_process_cancel(request).await
    }
}

impl<R> LashProcessWorkflow for LashProcessWorkflowImpl<R>
where
    R: RestateProcessRunner,
{
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<RestateProcessWorkflowInput>,
    ) -> HandlerResult<Json<RestateProcessWorkflowOutput>> {
        let process_id = input.registration.id.clone();
        let record = self
            .registry
            .try_get_process(&process_id)
            .await
            .map_err(HandlerError::from)?
            .ok_or_else(|| {
                HandlerError::from(TerminalError::new(format!(
                    "unknown process `{process_id}`"
                )))
            })?;
        if let Some(output) = record.status.await_output().cloned() {
            self.registry
                .delete_segment_handovers(&process_id)
                .await
                .map_err(HandlerError::from)?;
            if terminal_completion_workflow_key(&process_id, input.segment_ordinal).is_none() {
                resolve_process_terminal_promise(&ctx, &process_id, &output)?;
            } else {
                let request: restate_sdk::context::Request<
                    '_,
                    Json<RestateProcessCompleteRequest>,
                    Json<()>,
                > = ContextClient::request(
                    &ctx,
                    RequestTarget::workflow(
                        "LashProcessWorkflow",
                        process_id.clone(),
                        "complete_terminal",
                    ),
                    Json(RestateProcessCompleteRequest {
                        process_id: process_id.clone(),
                        output: output.clone(),
                    }),
                );
                request.call().await?;
            }
            return Ok(Json(RestateProcessWorkflowOutput::Terminal {
                output: Box::new(output),
            }));
        }
        let mut handover = if input.segment_ordinal == 0 {
            None
        } else {
            let persisted = self
                .registry
                .get_segment_handover(&process_id, input.segment_ordinal)
                .await
                .map_err(HandlerError::from)?;
            let Some(persisted) = persisted else {
                let latest = self
                    .registry
                    .latest_segment_handover(&process_id)
                    .await
                    .map_err(HandlerError::from)?;
                if missing_segment_is_superseded(input.segment_ordinal, latest.as_ref()) {
                    tracing::debug!(
                        process_id,
                        segment_ordinal = input.segment_ordinal,
                        latest_segment_ordinal = latest.as_ref().map(|value| value.segment_ordinal),
                        "ignoring retried superseded process segment"
                    );
                    return Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
                        next_segment_ordinal: latest
                            .expect("superseded classification requires latest handover")
                            .segment_ordinal,
                    }));
                }
                return Err(HandlerError::from(TerminalError::new(format!(
                    "missing persisted handover for process `{process_id}` segment {}",
                    input.segment_ordinal
                ))));
            };
            match validate_segment_program_hash(&process_id, persisted) {
                Ok(handover) => Some(handover),
                Err(err) => {
                    let output = ProcessAwaitOutput::Failure {
                        class: lash_core::ToolFailureClass::Execution,
                        code: err.code.to_string(),
                        message: err.message.clone(),
                        raw: None,
                        control: None,
                    };
                    let output = self
                        .complete_with_stored_outcome(&process_id, output)
                        .await
                        .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
                    self.registry
                        .delete_segment_handovers(&process_id)
                        .await
                        .map_err(HandlerError::from)?;
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCompleteRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        &ctx,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "complete_terminal",
                        ),
                        Json(RestateProcessCompleteRequest {
                            process_id: process_id.clone(),
                            output: output.clone(),
                        }),
                    );
                    request.call().await?;
                    return Ok(Json(RestateProcessWorkflowOutput::Terminal {
                        output: Box::new(output),
                    }));
                }
            }
        };
        if input.segment_ordinal == 0 && input.execution_id.is_some() {
            tracing::warn!(
                process_id,
                presented_execution_id = input.execution_id.as_deref(),
                invocation_id = %ctx.invocation_id(),
                verdict = "ignored",
                "segment-zero execution identity derives exclusively from the Restate invocation"
            );
        }
        let (execution_id, execution_write_authority) = segment_execution_authority(
            &process_id,
            input.segment_ordinal,
            input.execution_id.as_deref(),
            ctx.invocation_id(),
            record.first_started.as_deref(),
        )
        .map_err(HandlerError::from)?;
        let mut options = RestateEffectControllerOptions::default()
            .segment_effect_budget((self.segment_effect_budget)(&input.registration));
        if let Some(cap) = self.segment_duration_cap {
            options = options.segment_duration_cap(cap);
        }
        let controller = RestateRuntimeEffectController::with_options(ctx, options);
        let outcome = loop {
            let scoped_effect_controller = controller
                .scoped_effect_controller(ExecutionScope::process(process_id.clone()))
                .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
            let cancel_signal = self.cancellation_signal(&process_id, input.segment_ordinal);
            let outcome = self
                .run_registration(
                    input.registration.clone(),
                    input
                        .execution_context
                        .clone()
                        .with_execution_write_authority(execution_write_authority.clone()),
                    scoped_effect_controller,
                    input.segment_ordinal,
                    handover,
                    cancel_signal,
                )
                .await?;
            if let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = &outcome {
                let current = self
                    .registry
                    .try_get_process(&process_id)
                    .await
                    .map_err(retryable_registry_error)?;
                if boundary_must_be_declined(current.as_ref()) {
                    handover = Some(boundary.clone());
                    continue;
                }
            }
            break outcome;
        };
        resolve_process_cancel_signal(
            controller.context(),
            RestateProcessCancelSignal::SegmentFinished,
        )?;
        match outcome {
            lash_core::ProcessRunOutcome::Terminal(output) => {
                let output = *output;
                self.registry
                    .delete_segment_handovers(&process_id)
                    .await
                    .map_err(HandlerError::from)?;
                if terminal_completion_workflow_key(&process_id, input.segment_ordinal).is_none() {
                    resolve_process_terminal_promise(controller.context(), &process_id, &output)?;
                } else {
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCompleteRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        controller.context(),
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "complete_terminal",
                        ),
                        Json(RestateProcessCompleteRequest {
                            process_id: process_id.clone(),
                            output: output.clone(),
                        }),
                    );
                    request.call().await?;
                }
                Ok(Json(RestateProcessWorkflowOutput::Terminal {
                    output: Box::new(output),
                }))
            }
            lash_core::ProcessRunOutcome::SegmentBoundary(handover) => {
                let next_segment_ordinal = input.segment_ordinal.saturating_add(1);
                self.registry
                    .put_segment_handover(
                        &process_id,
                        lash_core::PersistedSegmentHandover {
                            segment_ordinal: next_segment_ordinal,
                            program_hash: handover.program_hash.clone().ok_or_else(|| {
                                HandlerError::from(TerminalError::new(format!(
                                    "process `{process_id}` segment handover omitted its program identity"
                                )))
                            })?,
                            handover,
                        },
                    )
                    .await
                    .map_err(HandlerError::from)?;
                if self
                    .process_cancel_requested(&process_id)
                    .await
                    .map_err(HandlerError::from)?
                {
                    let output = ProcessAwaitOutput::Cancelled {
                        message: format!("process `{process_id}` was cancelled between segments"),
                        raw: None,
                        control: None,
                    };
                    let output = self
                        .complete_with_stored_outcome(&process_id, output)
                        .await
                        .map_err(HandlerError::from)?;
                    self.registry
                        .delete_segment_handovers(&process_id)
                        .await
                        .map_err(HandlerError::from)?;
                    if terminal_completion_workflow_key(&process_id, input.segment_ordinal)
                        .is_none()
                    {
                        resolve_process_terminal_promise(
                            controller.context(),
                            &process_id,
                            &output,
                        )?;
                    } else {
                        let request: restate_sdk::context::Request<
                            '_,
                            Json<RestateProcessCompleteRequest>,
                            Json<()>,
                        > = ContextClient::request(
                            controller.context(),
                            RequestTarget::workflow(
                                "LashProcessWorkflow",
                                process_id.clone(),
                                "complete_terminal",
                            ),
                            Json(RestateProcessCompleteRequest {
                                process_id: process_id.clone(),
                                output: output.clone(),
                            }),
                        );
                        request.call().await?;
                    }
                    return Ok(Json(RestateProcessWorkflowOutput::Terminal {
                        output: Box::new(output),
                    }));
                }
                let successor_key = process_segment_workflow_key(&process_id, next_segment_ordinal);
                let request: restate_sdk::context::Request<
                    '_,
                    Json<RestateProcessWorkflowInput>,
                    Json<RestateProcessWorkflowOutput>,
                > = ContextClient::request(
                    controller.context(),
                    RequestTarget::workflow("LashProcessWorkflow", successor_key, "run"),
                    Json(RestateProcessWorkflowInput {
                        registration: input.registration,
                        execution_context: input.execution_context,
                        segment_ordinal: next_segment_ordinal,
                        execution_id: Some(execution_id),
                    }),
                );
                let _ = request.send().invocation_id().await?;
                Ok(Json(RestateProcessWorkflowOutput::SegmentChained {
                    next_segment_ordinal,
                }))
            }
        }
    }

    async fn complete_terminal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCompleteRequest>,
    ) -> HandlerResult<Json<()>> {
        resolve_process_terminal_promise(&ctx, &request.process_id, &request.output)?;
        Ok(Json(()))
    }

    async fn cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        self.record_cancel_requested(&request)
            .await
            .map_err(retryable_registry_error)?;
        resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::CancelRequested)?;

        if let Some(handover) = self
            .registry
            .latest_segment_handover(&request.process_id)
            .await
            .map_err(retryable_registry_error)?
            && handover.segment_ordinal > 0
        {
            let deliver: restate_sdk::context::Request<
                '_,
                Json<RestateProcessCancelRequest>,
                Json<()>,
            > = ContextClient::request(
                &ctx,
                RequestTarget::workflow(
                    "LashProcessWorkflow",
                    process_segment_workflow_key(&request.process_id, handover.segment_ordinal),
                    "deliver_cancel",
                ),
                Json(request.clone()),
            );
            let Json(()) = deliver.call().await?;
        }
        self.runner
            .request_process_cancel(request)
            .await
            .map_err(retryable_registry_error)?;
        Ok(Json(()))
    }

    async fn deliver_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(_request): Json<RestateProcessCancelRequest>,
    ) -> HandlerResult<Json<()>> {
        resolve_process_cancel_signal(&ctx, RestateProcessCancelSignal::CancelRequested)?;
        Ok(Json(()))
    }

    async fn await_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(_request): Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<RestateProcessCancelSignal>> {
        let payload = ctx.promise::<String>(PROCESS_CANCEL_PROMISE_KEY).await?;
        let signal = serde_json::from_str(&payload)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        Ok(Json(signal))
    }

    async fn await_terminal(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateProcessAwaitRequest>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>> {
        let key = restate_process_terminal_await_key(&request.process_id)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        let promise_key = key.promise_key();
        let payload = ctx.promise::<String>(&promise_key).await?;
        let resolution = serde_json::from_str(&payload)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        let output = restate_process_terminal_output(&request.process_id, resolution)
            .map_err(|err| HandlerError::from(TerminalError::from_error(err)))?;
        Ok(Json(output))
    }
}

/// One durable Restate workflow per Lash await-event identity.
///
/// Bind [`LashDurableWaitWorkflowImpl::serve`] on every endpoint that runs a
/// [`RestateRuntimeEffectController`]. The workflow key is a stable digest of
/// the full Lash [`AwaitEventKey`], so all execution-scope variants share the
/// same exact-address resolution path.
#[restate_sdk::workflow]
pub trait LashDurableWaitWorkflow {
    #[shared]
    async fn await_resolution(
        request: Json<RestateDurableWaitAwaitRequest>,
    ) -> HandlerResult<Json<Resolution>>;

    #[shared]
    async fn peek() -> HandlerResult<Json<Option<Resolution>>>;

    #[shared]
    async fn resolve(
        request: Json<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Json<ResolveOutcome>>;

    #[shared]
    async fn sleep_or_turn_cancel(
        request: Json<RestateTurnSleepWaitRequest>,
    ) -> HandlerResult<Json<RestateSleepRaceOutcome>>;

    #[shared]
    async fn await_event_or_turn_cancel(
        request: Json<RestateTurnAwaitEventWaitRequest>,
    ) -> HandlerResult<Json<RestateAwaitEventRaceOutcome>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LashDurableWaitWorkflowImpl;

impl LashDurableWaitWorkflow for LashDurableWaitWorkflowImpl {
    async fn await_resolution(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateDurableWaitAwaitRequest>,
    ) -> HandlerResult<Json<Resolution>> {
        let index_key = durable_wait_index_object_key(&request.address);
        let registration: restate_sdk::context::Request<
            '_,
            Json<RestateDurableWaitIndexRequest>,
            Json<RestateDurableWaitRegistration>,
        > = ContextClient::request(
            &ctx,
            RequestTarget::object("LashDurableWaitIndex", index_key.clone(), "register"),
            Json(RestateDurableWaitIndexRequest {
                address: request.address.clone(),
            }),
        );
        let Json(registration) = registration.call().await?;
        match registration {
            RestateDurableWaitRegistration::Resolved(resolution) => {
                return Ok(Json(resolution));
            }
            RestateDurableWaitRegistration::Revoked => {
                return Ok(Json(Resolution::Cancelled));
            }
            RestateDurableWaitRegistration::Registered => {}
        }

        let resolution = if let Some(payload) =
            ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await?
        {
            serde_json::from_str(&payload).map_err(TerminalError::from_error)?
        } else if let Some(timeout_ms) = request.timeout_ms {
            let promise = ctx.promise::<String>(DURABLE_WAIT_PROMISE_KEY);
            let timer =
                restate_sdk::context::ContextTimers::sleep(&ctx, Duration::from_millis(timeout_ms));
            restate_sdk::select! {
                payload = promise => {
                    let payload = payload?;
                    serde_json::from_str(&payload).map_err(TerminalError::from_error)?
                },
                _ = timer => {
                    let payload = serde_json::to_string(&Resolution::Timeout)
                        .map_err(TerminalError::from_error)?;
                    ctx.resolve_promise(DURABLE_WAIT_PROMISE_KEY, payload);
                    Resolution::Timeout
                },
                on_cancel => {
                    let payload = serde_json::to_string(&Resolution::Cancelled)
                        .map_err(TerminalError::from_error)?;
                    ctx.resolve_promise(DURABLE_WAIT_PROMISE_KEY, payload);
                    Resolution::Cancelled
                }
            }
        } else {
            let payload = ctx.promise::<String>(DURABLE_WAIT_PROMISE_KEY).await?;
            serde_json::from_str(&payload).map_err(TerminalError::from_error)?
        };

        let settle: restate_sdk::context::Request<
            '_,
            Json<RestateDurableWaitSettleRequest>,
            Json<()>,
        > = ContextClient::request(
            &ctx,
            RequestTarget::object("LashDurableWaitIndex", index_key, "settle"),
            Json(RestateDurableWaitSettleRequest {
                address: request.address,
                resolution: resolution.clone(),
            }),
        );
        let Json(()) = settle.call().await?;
        Ok(Json(resolution))
    }

    async fn peek(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> HandlerResult<Json<Option<Resolution>>> {
        let resolution = match ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
            Some(payload) => {
                Some(serde_json::from_str(&payload).map_err(TerminalError::from_error)?)
            }
            None => None,
        };
        Ok(Json(resolution))
    }

    async fn resolve(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Json<ResolveOutcome>> {
        if let Some(payload) = ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
            let terminal = serde_json::from_str(&payload).map_err(TerminalError::from_error)?;
            return Ok(Json(ResolveOutcome::AlreadyResolved { terminal }));
        }
        let payload =
            serde_json::to_string(&request.resolution).map_err(TerminalError::from_error)?;
        ctx.resolve_promise(DURABLE_WAIT_PROMISE_KEY, payload);
        Ok(Json(ResolveOutcome::Accepted))
    }

    async fn sleep_or_turn_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateTurnSleepWaitRequest>,
    ) -> HandlerResult<Json<RestateSleepRaceOutcome>> {
        let cancellation = ctx.promise::<String>(DURABLE_WAIT_PROMISE_KEY);
        let timer = restate_sdk::context::ContextTimers::sleep(
            &ctx,
            Duration::from_millis(request.duration_ms),
        );
        restate_sdk::select! {
            result = cancellation => {
                let _ = result?;
                Ok(Json(RestateSleepRaceOutcome::Cancelled))
            },
            result = timer => {
                result?;
                Ok(Json(RestateSleepRaceOutcome::Slept))
            }
        }
    }

    async fn await_event_or_turn_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateTurnAwaitEventWaitRequest>,
    ) -> HandlerResult<Json<RestateAwaitEventRaceOutcome>> {
        let event_address = request.event.address.clone();
        let event: restate_sdk::context::Request<
            '_,
            Json<RestateDurableWaitAwaitRequest>,
            Json<Resolution>,
        > = ContextClient::request(
            &ctx,
            RequestTarget::workflow(
                "LashDurableWaitWorkflow",
                event_address.workflow_key.clone(),
                "await_resolution",
            ),
            Json(request.event),
        );
        let event = event.call();
        let cancellation = ctx.promise::<String>(DURABLE_WAIT_PROMISE_KEY);
        let outcome = restate_sdk::select! {
            result = event => {
                let Json(resolution) = result?;
                RestateAwaitEventRaceOutcome::Event(resolution)
            },
            result = cancellation => {
                let _ = result?;
                let target = RequestTarget::object(
                    "LashDurableWaitIndex",
                    durable_wait_index_object_key(&event_address),
                    "resolve",
                );
                let resolve: restate_sdk::context::Request<
                    '_,
                    Json<RestateDurableWaitResolveRequest>,
                    Json<ResolveOutcome>,
                > = ContextClient::request(
                    &ctx,
                    target,
                    Json(RestateDurableWaitResolveRequest {
                        address: event_address,
                        resolution: Resolution::Cancelled,
                    }),
                );
                let Json(_) = resolve.call().await?;
                RestateAwaitEventRaceOutcome::TurnCancelled
            }
        };
        Ok(Json(outcome))
    }
}

/// Durable session-to-wait index used by cancellation and session deletion.
///
/// Bind [`LashDurableWaitIndexImpl::serve`] alongside
/// [`LashDurableWaitWorkflowImpl`]. Object serialization makes registration,
/// cancellation, and revocation atomic for one session.
#[restate_sdk::object]
pub trait LashDurableWaitIndex {
    async fn is_revoked(request: Json<()>) -> HandlerResult<Json<bool>>;
    async fn register(
        request: Json<RestateDurableWaitIndexRequest>,
    ) -> HandlerResult<Json<RestateDurableWaitRegistration>>;
    async fn settle(request: Json<RestateDurableWaitSettleRequest>) -> HandlerResult<Json<()>>;
    async fn register_awakeable(
        request: Json<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Json<RestateDurableWaitRegistration>>;
    async fn unregister_awakeable(
        request: Json<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Json<()>>;
    async fn resolve(
        request: Json<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Json<ResolveOutcome>>;
    async fn cancel_all() -> HandlerResult<Json<()>>;
    async fn revoke_all() -> HandlerResult<Json<()>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LashDurableWaitIndexImpl;

fn durable_wait_index_state_key(address: &RestateDurableWaitAddress) -> String {
    let classification = match address.classification {
        RestateDurableWaitClassification::DurableWait => "durable",
        RestateDurableWaitClassification::TurnControl => "control",
    };
    format!(
        "{DURABLE_WAIT_INDEX_WAIT_PREFIX}{classification}/{}",
        address.workflow_key
    )
}

fn durable_wait_index_resolution_key(address: &RestateDurableWaitAddress) -> String {
    format!(
        "{DURABLE_WAIT_INDEX_RESOLUTION_PREFIX}{}",
        address.workflow_key
    )
}

fn durable_wait_index_object_key(address: &RestateDurableWaitAddress) -> String {
    address.index_key()
}

fn durable_wait_address_from_state_key(
    object_key: &str,
    state_key: &str,
) -> Option<RestateDurableWaitAddress> {
    let suffix = state_key.strip_prefix(DURABLE_WAIT_INDEX_WAIT_PREFIX)?;
    let (classification, workflow_key) = suffix.split_once('/')?;
    if workflow_key.is_empty() || workflow_key.contains('/') {
        return None;
    }
    let classification = match classification {
        "durable" => RestateDurableWaitClassification::DurableWait,
        "control" => RestateDurableWaitClassification::TurnControl,
        _ => return None,
    };
    Some(RestateDurableWaitAddress {
        workflow_key: workflow_key.to_string(),
        session_id: Some(object_key.to_string()),
        classification,
    })
}

fn migrate_legacy_durable_wait_index(
    legacy: RestateDurableWaitIndexState,
) -> (
    RestateDurableWaitIndexMetadata,
    Vec<(String, RestateDurableWaitAddress)>,
) {
    let waits = legacy
        .waits
        .into_iter()
        .map(|address| (durable_wait_index_state_key(&address), address))
        .collect();
    (
        RestateDurableWaitIndexMetadata {
            revoked: legacy.revoked,
            awakeables: legacy.awakeables,
        },
        waits,
    )
}

/// Load the v1 index metadata, migrating the pre-v1 aggregate state on miss.
///
/// Restate object state is not part of an invocation's replayed journal: these
/// index handlers are short-lived single calls, so changing their command
/// sequence does not alter an in-flight multi-call journal. Object state does,
/// however, survive a deployment upgrade. The versioned metadata miss is
/// therefore the compatibility boundary: it reads the legacy `waits` value
/// once, expands its wait vector into v1 per-wait entries, and writes the v1
/// metadata marker so later calls never consult the legacy layout again.
async fn load_durable_wait_index_metadata(
    ctx: &ObjectContext<'_>,
) -> Result<RestateDurableWaitIndexMetadata, TerminalError> {
    if let Some(Json(metadata)) = ctx
        .get::<Json<RestateDurableWaitIndexMetadata>>(DURABLE_WAIT_INDEX_METADATA_KEY)
        .await?
    {
        return Ok(metadata);
    }

    let legacy = ctx
        .get::<Json<RestateDurableWaitIndexState>>(LEGACY_DURABLE_WAIT_INDEX_STATE_KEY)
        .await?
        .map(|Json(state)| state)
        .unwrap_or_default();
    let (metadata, waits) = migrate_legacy_durable_wait_index(legacy);
    for (state_key, address) in waits {
        ctx.set(&state_key, Json(address));
    }
    ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata.clone()));
    ctx.clear(LEGACY_DURABLE_WAIT_INDEX_STATE_KEY);
    Ok(metadata)
}

async fn load_indexed_waits(
    ctx: &ObjectContext<'_>,
) -> Result<Vec<RestateDurableWaitAddress>, TerminalError> {
    Ok(ctx
        .get_keys()
        .await?
        .into_iter()
        .filter_map(|state_key| durable_wait_address_from_state_key(ctx.key(), &state_key))
        .collect())
}

async fn resolve_indexed_waits(
    ctx: &ObjectContext<'_>,
    waits: Vec<RestateDurableWaitAddress>,
) -> HandlerResult<()> {
    for address in waits {
        let workflow_key = address.workflow_key.clone();
        let resolve: restate_sdk::context::Request<
            '_,
            Json<RestateDurableWaitResolveRequest>,
            Json<ResolveOutcome>,
        > = ContextClient::request(
            ctx,
            RequestTarget::workflow("LashDurableWaitWorkflow", workflow_key, "resolve"),
            Json(RestateDurableWaitResolveRequest {
                address,
                resolution: Resolution::Cancelled,
            }),
        );
        let _ = resolve.call().await?;
    }
    Ok(())
}

fn split_cancellable_waits(
    waits: Vec<RestateDurableWaitAddress>,
) -> (
    Vec<RestateDurableWaitAddress>,
    Vec<RestateDurableWaitAddress>,
) {
    waits
        .into_iter()
        .partition(|wait| wait.classification == RestateDurableWaitClassification::DurableWait)
}

impl LashDurableWaitIndex for LashDurableWaitIndexImpl {
    async fn is_revoked(
        &self,
        ctx: ObjectContext<'_>,
        Json(()): Json<()>,
    ) -> HandlerResult<Json<bool>> {
        if let Some(Json(metadata)) = ctx
            .get::<Json<RestateDurableWaitIndexMetadata>>(DURABLE_WAIT_INDEX_METADATA_KEY)
            .await?
        {
            return Ok(Json(metadata.revoked));
        }
        let revoked = ctx
            .get::<Json<RestateDurableWaitIndexState>>(LEGACY_DURABLE_WAIT_INDEX_STATE_KEY)
            .await?
            .is_some_and(|Json(state)| state.revoked);
        Ok(Json(revoked))
    }

    async fn register(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitIndexRequest>,
    ) -> HandlerResult<Json<RestateDurableWaitRegistration>> {
        let metadata = load_durable_wait_index_metadata(&ctx).await?;
        if metadata.revoked {
            return Ok(Json(RestateDurableWaitRegistration::Revoked));
        }
        if let Some(Json(resolution)) = ctx
            .get::<Json<Resolution>>(&durable_wait_index_resolution_key(&request.address))
            .await?
        {
            return Ok(Json(RestateDurableWaitRegistration::Resolved(resolution)));
        }
        ctx.set(
            &durable_wait_index_state_key(&request.address),
            Json(request.address),
        );
        Ok(Json(RestateDurableWaitRegistration::Registered))
    }

    async fn settle(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitSettleRequest>,
    ) -> HandlerResult<Json<()>> {
        let _metadata = load_durable_wait_index_metadata(&ctx).await?;
        ctx.set(
            &durable_wait_index_resolution_key(&request.address),
            Json(request.resolution),
        );
        if request.address.classification == RestateDurableWaitClassification::DurableWait {
            ctx.clear(&durable_wait_index_state_key(&request.address));
        }
        Ok(Json(()))
    }

    async fn register_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Json<RestateDurableWaitRegistration>> {
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        if metadata.revoked {
            return Ok(Json(RestateDurableWaitRegistration::Revoked));
        }
        if let Some(Json(resolution)) = ctx
            .get::<Json<Resolution>>(&durable_wait_index_resolution_key(&request.address))
            .await?
        {
            ctx.resolve_awakeable(&request.awakeable_id, Json(resolution));
            return Ok(Json(RestateDurableWaitRegistration::Registered));
        }
        let peek: restate_sdk::context::Request<'_, (), Json<Option<Resolution>>> =
            ContextClient::request(
                &ctx,
                RequestTarget::workflow(
                    "LashDurableWaitWorkflow",
                    request.address.workflow_key.clone(),
                    "peek",
                ),
                (),
            );
        let Json(resolution) = peek.call().await?;
        if let Some(resolution) = resolution {
            ctx.resolve_awakeable(&request.awakeable_id, Json(resolution));
        } else if !metadata
            .awakeables
            .iter()
            .any(|entry| entry.awakeable_id == request.awakeable_id)
        {
            metadata.awakeables.push(request);
            ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata));
        }
        Ok(Json(RestateDurableWaitRegistration::Registered))
    }

    async fn unregister_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Json<()>> {
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        metadata
            .awakeables
            .retain(|entry| entry.awakeable_id != request.awakeable_id);
        ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata));
        Ok(Json(()))
    }

    async fn resolve(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Json<ResolveOutcome>> {
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        if metadata.revoked {
            return Ok(Json(ResolveOutcome::UnknownOrRevoked));
        }
        let resolution_key = durable_wait_index_resolution_key(&request.address);
        if let Some(Json(terminal)) = ctx.get::<Json<Resolution>>(&resolution_key).await? {
            return Ok(Json(ResolveOutcome::AlreadyResolved { terminal }));
        }
        let resolution = request.resolution.clone();
        let resolve: restate_sdk::context::Request<
            '_,
            Json<RestateDurableWaitResolveRequest>,
            Json<ResolveOutcome>,
        > = ContextClient::request(
            &ctx,
            RequestTarget::workflow(
                "LashDurableWaitWorkflow",
                request.address.workflow_key.clone(),
                "resolve",
            ),
            Json(request.clone()),
        );
        let Json(outcome) = resolve.call().await?;
        let terminal = match &outcome {
            ResolveOutcome::AlreadyResolved { terminal } => terminal.clone(),
            ResolveOutcome::Accepted => resolution,
            ResolveOutcome::UnknownOrRevoked => return Ok(Json(outcome)),
        };
        ctx.set(&resolution_key, Json(terminal.clone()));
        let mut retained = Vec::with_capacity(metadata.awakeables.len());
        for entry in std::mem::take(&mut metadata.awakeables) {
            if entry.address == request.address {
                ctx.resolve_awakeable(&entry.awakeable_id, Json(terminal.clone()));
            } else {
                retained.push(entry);
            }
        }
        metadata.awakeables = retained;
        ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata));
        Ok(Json(outcome))
    }

    async fn cancel_all(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        let _metadata = load_durable_wait_index_metadata(&ctx).await?;
        let (waits, _controls) = split_cancellable_waits(load_indexed_waits(&ctx).await?);
        for address in &waits {
            ctx.clear(&durable_wait_index_state_key(address));
            ctx.set(
                &durable_wait_index_resolution_key(address),
                Json(Resolution::Cancelled),
            );
        }
        resolve_indexed_waits(&ctx, waits).await?;
        Ok(Json(()))
    }

    async fn revoke_all(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        let waits = load_indexed_waits(&ctx).await?;
        let awakeables = std::mem::take(&mut metadata.awakeables);
        metadata.revoked = true;
        ctx.clear_all();
        ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata));
        for entry in awakeables {
            ctx.resolve_awakeable(&entry.awakeable_id, Json(Resolution::Cancelled));
        }
        resolve_indexed_waits(&ctx, waits).await?;
        Ok(Json(()))
    }
}

/// Restate ingress attachment to a turn's reserved terminal keyed promise.
#[derive(Clone)]
pub struct RestateTurnAttach {
    ingress: RestateIngressClient,
}

impl RestateTurnAttach {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        Self {
            ingress: RestateIngressClient::new(connection),
        }
    }
}

#[async_trait::async_trait]
impl TurnAttach for RestateTurnAttach {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError> {
        address.execution_scope().validate_for_durable_host()?;
        let key = restate_await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )?;
        let durable_address = RestateDurableWaitAddress::for_key(&key);
        let workflow_key = durable_address.workflow_key.clone();
        let resolution = self
            .ingress
            .call_workflow_json::<_, Resolution>(
                "LashDurableWaitWorkflow",
                &workflow_key,
                "await_resolution",
                &RestateDurableWaitAwaitRequest {
                    address: durable_address,
                    timeout_ms: None,
                },
            )
            .await
            .map_err(|err| RuntimeError::new("restate_turn_terminal_attach", err.to_string()))?;
        match resolution {
            Resolution::Ok(value) => serde_json::from_value(value)
                .map_err(|err| RuntimeError::new("restate_turn_terminal_decode", err.to_string())),
            Resolution::Cancelled => Err(RuntimeError::new(
                "turn_control_unknown_or_revoked",
                format!(
                    "terminal promise for turn `{}` in session `{}` was revoked",
                    address.turn_id, address.session_id
                ),
            )),
            other => Err(RuntimeError::new(
                "restate_turn_terminal_invalid_resolution",
                format!(
                    "terminal promise for turn `{}` in session `{}` resolved with {other:?}",
                    address.turn_id, address.session_id
                ),
            )),
        }
    }
}

/// Bundled Restate wiring for foreground-turn control.
///
/// Use the returned effect host to configure Lash turn execution and the
/// returned driver for out-of-process cancellation/terminal attachment. Bind
/// `LashDurableWaitWorkflowImpl` and `LashDurableWaitIndexImpl` on the endpoint;
/// no Restate Admin API access is involved.
pub struct RestateTurnDeployment {
    effect_host: Arc<RestateEffectHost>,
    driver: TurnWorkDriver,
    attach: Arc<RestateTurnAttach>,
}

impl RestateTurnDeployment {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        let connection = connection.into();
        let effect_host = Arc::new(RestateEffectHost::new(connection.clone()));
        let attach = Arc::new(RestateTurnAttach::new(connection));
        let driver = TurnWorkDriver::new(effect_host.clone()).with_attach(attach.clone());
        Self {
            effect_host,
            driver,
            attach,
        }
    }

    pub fn effect_host(&self) -> Arc<RestateEffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub fn turn_work_driver(&self) -> TurnWorkDriver {
        self.driver.clone()
    }

    pub fn turn_attach(&self) -> Arc<RestateTurnAttach> {
        Arc::clone(&self.attach)
    }
}

/// Configuration for [`RestateRuntimeEffectController`].
#[derive(Clone)]
pub struct RestateEffectControllerOptions {
    run_retry_policy: Option<RunRetryPolicy>,
    segment_duration_cap: Option<Duration>,
    segment_effect_budget: u64,
}

impl Default for RestateEffectControllerOptions {
    fn default() -> Self {
        Self {
            run_retry_policy: None,
            segment_duration_cap: None,
            segment_effect_budget: 10_000,
        }
    }
}

impl RestateEffectControllerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a Restate retry policy for recorded `ctx.run` effects.
    ///
    /// Lash provider/tool errors are recorded as Lash data, so this policy is
    /// used only when the recorded closure itself fails before producing a
    /// serializable effect result.
    pub fn run_retry_policy(mut self, policy: RunRetryPolicy) -> Self {
        self.run_retry_policy = Some(policy);
        self
    }

    /// Request a segment boundary once this handler incarnation has lived for
    /// at least `cap`. The actual cut remains the engine's quiescent post-effect
    /// point, shared with journal-budget boundaries.
    pub fn segment_duration_cap(mut self, cap: Duration) -> Self {
        self.segment_duration_cap = Some(cap);
        self
    }

    /// Set the deterministic maximum number of completed effects in one
    /// Restate invocation. Replay observes the same progress and cuts at the
    /// same post-effect point.
    pub fn segment_effect_budget(mut self, effects: u64) -> Self {
        self.segment_effect_budget = effects.max(1);
        self
    }
}

impl fmt::Debug for RestateEffectControllerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestateEffectControllerOptions")
            .field("run_retry_policy", &self.run_retry_policy)
            .field("segment_duration_cap", &self.segment_duration_cap)
            .field("segment_effect_budget", &self.segment_effect_budget)
            .finish()
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Serialize, serde::Deserialize)]
pub enum RestateSleepRaceOutcome {
    Slept,
    Cancelled,
}

#[doc(hidden)]
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum RestateAwaitEventRaceOutcome {
    Event(Resolution),
    TurnCancelled,
}

#[doc(hidden)]
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateTurnSleepWaitRequest {
    duration_ms: u64,
}

#[doc(hidden)]
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateTurnAwaitEventWaitRequest {
    event: RestateDurableWaitAwaitRequest,
}

#[doc(hidden)]
pub trait RestateControllerContext<'ctx>: Send + Sync + 'ctx {
    fn sleep_send<'run>(
        &'run self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn sleep_or_turn_cancel<'run>(
        &'run self,
        duration: Duration,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<RestateSleepRaceOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async move {
            let Some(turn_cancel) = turn_cancel else {
                return tokio::select! {
                    result = self.sleep_send(duration) => {
                        result.map(|()| RestateSleepRaceOutcome::Slept)
                    }
                    _ = cancellation.cancelled() => Ok(RestateSleepRaceOutcome::Cancelled),
                };
            };
            tokio::select! {
                result = self.sleep_send(duration) => {
                    result.map(|()| RestateSleepRaceOutcome::Slept)
                }
                result = self.await_event(
                    turn_cancel,
                    tokio_util::sync::CancellationToken::new(),
                ) => {
                    result.map(|_| RestateSleepRaceOutcome::Cancelled)
                }
                _ = cancellation.cancelled() => Ok(RestateSleepRaceOutcome::Cancelled),
            }
        })
    }

    fn run_json_send<'run, T, Fut>(
        &'run self,
        effect_name: String,
        retry_policy: Option<RunRetryPolicy>,
        future: Fut,
    ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
        Fut: Future<Output = T> + Send + 'run;

    fn start_process_workflow<'run>(
        &'run self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn request_process_workflow_cancel<'run>(
        &'run self,
        request: RestateProcessCancelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_event<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_event_or_turn_cancel<'run>(
        &'run self,
        request: RestateDurableWaitAwaitRequest,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Pin<
        Box<dyn Future<Output = Result<RestateAwaitEventRaceOutcome, TerminalError>> + Send + 'run>,
    >
    where
        'ctx: 'run,
    {
        Box::pin(async move {
            let Some(turn_cancel) = turn_cancel else {
                return self
                    .await_event(request, cancellation)
                    .await
                    .map(RestateAwaitEventRaceOutcome::Event);
            };
            tokio::select! {
                result = self.await_event(request, cancellation.clone()) => {
                    result.map(RestateAwaitEventRaceOutcome::Event)
                }
                result = self.await_event(
                    turn_cancel,
                    tokio_util::sync::CancellationToken::new(),
                ) => {
                    result.map(|_| RestateAwaitEventRaceOutcome::TurnCancelled)
                }
            }
        })
    }

    fn peek_event<'run>(
        &'run self,
        address: RestateDurableWaitAddress,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn await_process_terminal<'run>(
        &'run self,
        process_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn resolve_event<'run>(
        &'run self,
        request: RestateDurableWaitResolveRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn update_session_waits<'run>(
        &'run self,
        session_id: String,
        revoke: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
    where
        'ctx: 'run;

    fn session_is_revoked<'run>(
        &'run self,
        _session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
    where
        'ctx: 'run,
    {
        Box::pin(async { Ok(false) })
    }
}

macro_rules! impl_restate_controller_context {
    ($($context:ident),+ $(,)?) => {
        $(
            impl<'ctx> RestateControllerContext<'ctx> for $context<'ctx> {
                fn sleep_send<'run>(
                    &'run self,
                    duration: Duration,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        restate_sdk::context::ContextTimers::sleep(self, duration).await
                    })
                }

                fn sleep_or_turn_cancel<'run>(
                    &'run self,
                    duration: Duration,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    cancellation: tokio_util::sync::CancellationToken,
                ) -> Pin<Box<dyn Future<Output = Result<RestateSleepRaceOutcome, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            let timer = guard_restate_context_future(
                                restate_sdk::context::ContextTimers::sleep(self, duration),
                            );
                            tokio::pin!(timer);
                            return tokio::select! {
                                result = &mut timer => {
                                    result.map(|()| RestateSleepRaceOutcome::Slept)
                                }
                                _ = cancellation.cancelled() => Ok(RestateSleepRaceOutcome::Cancelled),
                            };
                        };

                        let Some(session_id) = turn_cancel.address.session_id.clone() else {
                            return Err(TerminalError::new(
                                "turn cancellation gate is missing its session id",
                            ));
                        };
                        let (awakeable_id, awakeable) = self.awakeable::<Json<Resolution>>();
                        let registration_request = RestateDurableWaitAwakeableRequest {
                            address: turn_cancel.address,
                            awakeable_id,
                        };
                        let register: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitAwakeableRequest>,
                            Json<RestateDurableWaitRegistration>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id.clone(),
                                "register_awakeable",
                            ),
                            Json(registration_request.clone()),
                        );
                        let Json(registration) = register.call().await?;
                        if registration == RestateDurableWaitRegistration::Revoked {
                            return Ok(RestateSleepRaceOutcome::Cancelled);
                        }

                        let timer = restate_sdk::context::ContextTimers::sleep(self, duration);
                        restate_sdk::select! {
                            result = timer => {
                                result?;
                                let unregister: restate_sdk::context::Request<
                                    '_,
                                    Json<RestateDurableWaitAwakeableRequest>,
                                    Json<()>,
                                > = ContextClient::request(
                                    self,
                                    RequestTarget::object(
                                        "LashDurableWaitIndex",
                                        session_id,
                                        "unregister_awakeable",
                                    ),
                                    Json(registration_request),
                                );
                                let Json(()) = unregister.call().await?;
                                Ok(RestateSleepRaceOutcome::Slept)
                            },
                            result = awakeable => {
                                let _ = result?;
                                Ok(RestateSleepRaceOutcome::Cancelled)
                            }
                        }
                    })
                }

                fn run_json_send<'run, T, Fut>(
                    &'run self,
                    effect_name: String,
                    retry_policy: Option<RunRetryPolicy>,
                    future: Fut,
                ) -> Pin<Box<dyn Future<Output = Result<Json<T>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                    T: Serialize + DeserializeOwned + Send + 'static,
                    Fut: Future<Output = T> + Send + 'run,
                {
                    Box::pin(async move {
                        let run = restate_sdk::context::ContextSideEffects::run(self, move || async move {
                            Ok::<Json<T>, HandlerError>(Json(future.await))
                        });
                        let run = restate_sdk::context::RunFuture::name(run, effect_name);
                        let run = match retry_policy {
                            Some(policy) => restate_sdk::context::RunFuture::retry_policy(run, policy),
                            None => run,
                        };
                        run.await
                    })
                }

                fn start_process_workflow<'run>(
                    &'run self,
                    registration: ProcessRegistration,
                    execution_context: ProcessExecutionContext,
                ) -> Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let workflow_key = registration.id.clone();
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessWorkflowInput>,
                        Json<ProcessAwaitOutput>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            workflow_key.clone(),
                            "run",
                        ),
                        Json(RestateProcessWorkflowInput {
                            registration,
                            execution_context,
                            segment_ordinal: 0,
                            execution_id: None,
                        }),
                    );
                    let handle = request.send();
                    Box::pin(async move { handle.invocation_id().await })
                }

                fn request_process_workflow_cancel<'run>(
                    &'run self,
                    request: RestateProcessCancelRequest,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let workflow_key = request.process_id.clone();
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessCancelRequest>,
                        Json<()>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            workflow_key.clone(),
                            "cancel",
                        ),
                        Json(request),
                    );
                    let call = request.call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }

                fn await_event<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    _cancellation: tokio_util::sync::CancellationToken,
                ) -> Pin<Box<dyn Future<Output = Result<Resolution, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let start: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitAwaitRequest>,
                            Json<Resolution>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::workflow(
                                "LashDurableWaitWorkflow",
                                request.address.workflow_key.clone(),
                                "await_resolution",
                            ),
                            Json(request.clone()),
                        );
                        let call = start.call();
                        restate_sdk::select! {
                            result = call => {
                                let Json(resolution) = result?;
                                Ok(resolution)
                            },
                            on_cancel => {
                                let address = request.address;
                                let target = RequestTarget::object(
                                    "LashDurableWaitIndex",
                                    durable_wait_index_object_key(&address),
                                    "resolve",
                                );
                                let resolve_request: restate_sdk::context::Request<
                                    '_,
                                    Json<RestateDurableWaitResolveRequest>,
                                    Json<ResolveOutcome>,
                                > = ContextClient::request(
                                    self,
                                    target,
                                    Json(RestateDurableWaitResolveRequest {
                                        address,
                                        resolution: Resolution::Cancelled,
                                    }),
                                );
                                let Json(outcome) = resolve_request.call().await?;
                                Ok(match outcome {
                                    ResolveOutcome::AlreadyResolved { terminal } => terminal,
                                    ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked => {
                                        Resolution::Cancelled
                                    }
                                })
                            }
                        }
                    })
                }

                fn await_event_or_turn_cancel<'run>(
                    &'run self,
                    request: RestateDurableWaitAwaitRequest,
                    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
                    cancellation: tokio_util::sync::CancellationToken,
                ) -> Pin<
                    Box<
                        dyn Future<Output = Result<RestateAwaitEventRaceOutcome, TerminalError>>
                            + Send
                            + 'run,
                    >,
                >
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let Some(turn_cancel) = turn_cancel else {
                            return self
                                .await_event(request, cancellation)
                                .await
                                .map(RestateAwaitEventRaceOutcome::Event);
                        };

                        let cancel_workflow_key = turn_cancel.address.workflow_key;
                        let race: restate_sdk::context::Request<
                            '_,
                            Json<RestateTurnAwaitEventWaitRequest>,
                            Json<RestateAwaitEventRaceOutcome>,
                        > = ContextClient::request(
                            self,
                            RequestTarget::workflow(
                                "LashDurableWaitWorkflow",
                                cancel_workflow_key,
                                "await_event_or_turn_cancel",
                            ),
                            Json(RestateTurnAwaitEventWaitRequest { event: request }),
                        );
                        let Json(outcome) = race.call().await?;
                        Ok(outcome)
                    })
                }

                fn peek_event<'run>(
                    &'run self,
                    address: RestateDurableWaitAddress,
                ) -> Pin<Box<dyn Future<Output = Result<Option<Resolution>, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request: restate_sdk::context::Request<
                        '_,
                        (),
                        Json<Option<Resolution>>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashDurableWaitWorkflow",
                            address.workflow_key,
                            "peek",
                        ),
                        (),
                    );
                    Box::pin(async move {
                        let Json(resolution) = request.call().await?;
                        Ok(resolution)
                    })
                }

                fn await_process_terminal<'run>(
                    &'run self,
                    process_id: String,
                ) -> Pin<Box<dyn Future<Output = Result<ProcessAwaitOutput, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request: restate_sdk::context::Request<
                        '_,
                        Json<RestateProcessAwaitRequest>,
                        Json<ProcessAwaitOutput>,
                    > = ContextClient::request(
                        self,
                        RequestTarget::workflow(
                            "LashProcessWorkflow",
                            process_id.clone(),
                            "await_terminal",
                        ),
                        Json(RestateProcessAwaitRequest { process_id }),
                    );
                    let call = request.call();
                    Box::pin(async move {
                        let Json(output) = call.await?;
                        Ok(output)
                    })
                }

                fn resolve_event<'run>(
                    &'run self,
                    request: RestateDurableWaitResolveRequest,
                ) -> Pin<Box<dyn Future<Output = Result<ResolveOutcome, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    Box::pin(async move {
                        let target = RequestTarget::object(
                            "LashDurableWaitIndex",
                            durable_wait_index_object_key(&request.address),
                            "resolve",
                        );
                        let resolve: restate_sdk::context::Request<
                            '_,
                            Json<RestateDurableWaitResolveRequest>,
                            Json<ResolveOutcome>,
                        > = ContextClient::request(self, target, Json(request));
                        let Json(outcome) = resolve.call().await?;
                        Ok(outcome)
                    })
                }

                fn update_session_waits<'run>(
                    &'run self,
                    session_id: String,
                    revoke: bool,
                ) -> Pin<Box<dyn Future<Output = Result<(), TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let handler = if revoke { "revoke_all" } else { "cancel_all" };
                    // Zero-input handlers require an empty payload; `()`
                    // serializes to empty bytes while `Json(())` would send a
                    // JSON `null` body that Restate's input validation rejects.
                    let request: restate_sdk::context::Request<'_, (), Json<()>> =
                        ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id,
                                handler,
                            ),
                            (),
                        );
                    let call = request.call();
                    Box::pin(async move {
                        let Json(()) = call.await?;
                        Ok(())
                    })
                }

                fn session_is_revoked<'run>(
                    &'run self,
                    session_id: String,
                ) -> Pin<Box<dyn Future<Output = Result<bool, TerminalError>> + Send + 'run>>
                where
                    'ctx: 'run,
                {
                    let request: restate_sdk::context::Request<'_, Json<()>, Json<bool>> =
                        ContextClient::request(
                            self,
                            RequestTarget::object(
                                "LashDurableWaitIndex",
                                session_id,
                                "is_revoked",
                            ),
                            Json(()),
                        );
                    let call = request.call();
                    Box::pin(async move {
                        let Json(revoked) = call.await?;
                        Ok(revoked)
                    })
                }
            }
        )+
    };
}

impl_restate_controller_context!(
    RestateContext,
    SharedObjectContext,
    ObjectContext,
    SharedWorkflowContext,
    WorkflowContext,
);

/// Lash [`RuntimeEffectController`] and [`EffectHost`] backed by a Restate handler context.
///
/// This type is intentionally handler-scoped. Create one inside the Restate
/// handler that owns the Lash operation, then pass
/// [`RestateRuntimeEffectController::scoped_effect_controller`] to Lash's
/// scoped API with a stable [`ExecutionScope`].
pub struct RestateRuntimeEffectController<'ctx, C> {
    context: C,
    options: RestateEffectControllerOptions,
    _ctx: PhantomData<&'ctx ()>,
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C> {
    pub fn new(context: C) -> Self {
        Self::with_options(context, RestateEffectControllerOptions::default())
    }

    pub fn with_options(context: C, options: RestateEffectControllerOptions) -> Self {
        Self {
            context,
            options,
            _ctx: PhantomData,
        }
    }

    pub fn context(&self) -> &C {
        &self.context
    }

    pub fn options(&self) -> &RestateEffectControllerOptions {
        &self.options
    }
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    pub fn scoped_effect_controller<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate_for_durable_host()?;
        ScopedEffectController::borrowed(self, scope)
    }

    async fn record_effect<'run, T>(
        &'run self,
        metadata: RuntimeInvocation,
        // Keep the full journaled-effect executor behind one allocation. The
        // Restate SDK stores this future in its ctx.run state machine, so
        // accepting it inline here makes every composed turn carry the whole
        // executor frame through the durable adapter.
        future: Pin<Box<dyn Future<Output = T> + Send + 'run>>,
    ) -> Result<T, RestateEffectError>
    where
        'ctx: 'run,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        let effect_name = restate_effect_name(&metadata);
        let run_retry_policy = self.options.run_retry_policy.clone();
        let Json(value) = self
            .context
            .run_json_send(effect_name.clone(), run_retry_policy, future)
            .await
            .map_err(|source| RestateEffectError::Terminal {
                effect: effect_name,
                terminal: source,
            })?;
        Ok(value)
    }
}

impl<C> fmt::Debug for RestateRuntimeEffectController<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestateRuntimeEffectController")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<'ctx, C> AwaitEventResolver for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate_for_durable_host()?;
        if let Some(session_id) = scope.session_id()
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new("restate_await_event_revocation_read", err.to_string())
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
        restate_await_event_key(scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event(&self.context, key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Err(restate_unknown_or_revoked());
        }
        if let Some(session_id) = key.scope.session_id()
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new("restate_await_event_revocation_read", err.to_string())
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
        self.context
            .peek_event(RestateDurableWaitAddress::for_key(key))
            .await
            .map_err(|err| RuntimeError::new("restate_effect_controller", err.to_string()))
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        if !restate_await_event_key_is_valid(key) {
            return Err(restate_unknown_or_revoked());
        }
        if let Some(session_id) = key.scope.session_id()
            && self
                .context
                .session_is_revoked(session_id.to_string())
                .await
                .map_err(|err| {
                    RuntimeError::new("restate_await_event_revocation_read", err.to_string())
                })?
        {
            return Err(restate_unknown_or_revoked());
        }
        let clock = lash_core::SystemClock;
        self.context
            .await_event(restate_durable_wait_request(key, deadline, &clock), cancel)
            .await
            .map_err(|err| RuntimeError::new("restate_effect_controller", err.to_string()))
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(session_id.to_string(), true)
            .await
            .map_err(|err| RuntimeError::new("restate_await_event_revoke", err.to_string()))
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.context
            .update_session_waits(session_id.to_string(), false)
            .await
            .map_err(|err| RuntimeError::new("restate_await_event_cancel", err.to_string()))
    }
}

impl<'ctx, C> EffectHost for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx> + Sync,
{
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.scoped_effect_controller(scope)
    }
}

#[async_trait::async_trait]
impl<'ctx, C> RuntimeEffectController for RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn supports_concurrent_effects(&self) -> bool {
        false
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        (progress.effects_executed >= self.options.segment_effect_budget)
            .then_some(lash_core::BoundaryReason::JournalBudget)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match restate_effect_execution(&envelope.command) {
            RestateEffectExecution::DirectProcess => {
                let RuntimeEffectCommand::Process { command } = envelope.command else {
                    unreachable!("direct process execution is only selected for process effects");
                };
                execute_restate_process_command(&self.context, *command, local_executor)
                    .await
                    .map(|result| RuntimeEffectOutcome::Process { result })
            }
            RestateEffectExecution::DirectLocal => local_executor.execute(envelope).await,
            RestateEffectExecution::Timer => {
                let RuntimeEffectCommand::Sleep { duration_ms } = &envelope.command else {
                    unreachable!("timer execution is only selected for sleep effects");
                };
                let duration = Duration::from_millis(*duration_ms);
                let RuntimeSleepOptions {
                    cancellation,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_sleep_options();
                let turn_cancel = restate_timer_turn_cancel_wait_request(
                    &envelope.invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                match self
                    .context
                    .sleep_or_turn_cancel(duration, turn_cancel, cancellation.clone())
                    .await
                {
                    Ok(RestateSleepRaceOutcome::Slept) => {}
                    Ok(RestateSleepRaceOutcome::Cancelled) => {
                        cancellation.cancel();
                        return Err(RuntimeEffectControllerError::new(
                            "runtime_effect_sleep_cancelled",
                            "runtime effect sleep was cancelled",
                        ));
                    }
                    Err(err) => {
                        tracing_sleep_error(&envelope.invocation, &err);
                        return Err(RuntimeEffectControllerError::new(
                            "restate_effect_controller",
                            err.to_string(),
                        ));
                    }
                }
                Ok(RuntimeEffectOutcome::Sleep)
            }
            RestateEffectExecution::AwaitEvent => {
                let RuntimeEffectCommand::AwaitEvent { key } = envelope.command else {
                    unreachable!("await-event execution is only selected for event waits");
                };
                let RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    observe_turn_cancel,
                    turn_cancel_scope,
                } = local_executor.into_await_event_options()?;
                let turn_cancel = restate_await_event_turn_cancel_wait_request(
                    &envelope.invocation,
                    observe_turn_cancel,
                    turn_cancel_scope.as_ref(),
                )?;
                match self
                    .context
                    .await_event_or_turn_cancel(
                        restate_durable_wait_request(&key, deadline, clock.as_ref()),
                        turn_cancel,
                        cancellation.clone(),
                    )
                    .await
                {
                    Ok(RestateAwaitEventRaceOutcome::Event(resolution)) => {
                        Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
                    }
                    Ok(RestateAwaitEventRaceOutcome::TurnCancelled) => {
                        cancellation.cancel();
                        Ok(RuntimeEffectOutcome::AwaitEvent {
                            resolution: Resolution::Cancelled,
                        })
                    }
                    Err(err) => Err(RuntimeEffectControllerError::new(
                        "restate_effect_controller",
                        err.to_string(),
                    )),
                }
            }
            RestateEffectExecution::PeekAwaitEvent => {
                let RuntimeEffectCommand::PeekAwaitEvent { key } = envelope.command else {
                    unreachable!("peek execution is only selected for await-event reads");
                };
                self.peek_await_event(&key)
                    .await
                    .map(|resolution| RuntimeEffectOutcome::PeekAwaitEvent { resolution })
                    .map_err(RuntimeEffectControllerError::from)
            }
            RestateEffectExecution::JournaledRun => {
                let reconstructed_envelope = envelope.canonical_form()?;
                let replay_trace = local_executor.replay_validation_trace().cloned();
                let invocation = envelope.invocation.clone();
                let recorded_envelope = Arc::new(reconstructed_envelope.clone());
                let recorded = self
                    .record_effect(
                        invocation,
                        Box::pin(async move {
                            let outcome =
                                execute_restate_journaled_effect(envelope, local_executor).await;
                            RecordedRuntimeEffect {
                                envelope: recorded_envelope,
                                outcome,
                            }
                        }),
                    )
                    .await
                    .map_err(|err| {
                        RuntimeEffectControllerError::new(
                            "restate_effect_controller",
                            err.to_string(),
                        )
                    })?;
                validate_recorded_effect_envelope(
                    recorded,
                    &reconstructed_envelope,
                    replay_trace.as_ref(),
                )?
            }
        }
    }
}

async fn execute_restate_journaled_effect(
    envelope: RuntimeEffectEnvelope,
    local_executor: RuntimeEffectLocalExecutor<'_>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    let RuntimeEffectEnvelope {
        invocation,
        command,
    } = envelope;
    match command {
        RuntimeEffectCommand::Trigger { command } => {
            local_executor.execute_trigger(invocation, *command).await
        }
        command => {
            local_executor
                .execute(RuntimeEffectEnvelope {
                    invocation,
                    command,
                })
                .await
        }
    }
}

async fn execute_restate_process_command<'ctx, C>(
    context: &C,
    command: ProcessCommand,
    local_executor: RuntimeEffectLocalExecutor<'_>,
) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let execution = local_executor.into_process()?;
    let registry = execution.registry;
    match command {
        ProcessCommand::Start {
            registration,
            grant,
            execution_context,
        } => {
            let record = schedule_restate_process(
                registry,
                registration,
                grant,
                *execution_context,
                context,
            )
            .await?;
            Ok(ProcessEffectOutcome::Start {
                record: Box::new(record),
            })
        }
        ProcessCommand::List {
            session_scope,
            mode,
        } => {
            let entries = match mode {
                lash_core::ProcessListMode::Live => {
                    registry.list_live_handle_grants(&session_scope).await?
                }
                lash_core::ProcessListMode::All => {
                    registry.list_handle_grants(&session_scope).await?
                }
            };
            Ok(ProcessEffectOutcome::List { entries })
        }
        ProcessCommand::Transfer {
            from_scope,
            to_scope,
            process_ids,
        } => {
            registry
                .transfer_handle_grants(&from_scope, &to_scope, &process_ids)
                .await?;
            Ok(ProcessEffectOutcome::Transfer)
        }
        ProcessCommand::DeleteSession { session_id } => {
            let report = registry.delete_session_process_state(&session_id).await?;
            Ok(ProcessEffectOutcome::DeleteSession { report })
        }
        ProcessCommand::Await { process_id } => {
            if registry.get_process(&process_id).await.is_none() {
                return Err(PluginError::Session(format!("unknown process `{process_id}`")).into());
            }
            let output = context
                .await_process_terminal(process_id.clone())
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new("restate_process_await", err.to_string())
                })?;
            Ok(ProcessEffectOutcome::Await {
                output: Box::new(output),
            })
        }
        ProcessCommand::Cancel { process_id, reason } => {
            let record = registry
                .get_process(&process_id)
                .await
                .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
            registry
                .append_event(
                    &process_id,
                    lash_core::ProcessEventAppendRequest::cancel_requested(
                        &process_id,
                        reason.clone(),
                    ),
                )
                .await?;
            context
                .request_process_workflow_cancel(RestateProcessCancelRequest { process_id, reason })
                .await
                .map_err(|err| {
                    RestateEffectError::BackgroundScheduler(err.to_string()).into_plugin_error()
                })?;
            Ok(ProcessEffectOutcome::Cancel {
                record: Box::new(record),
            })
        }
        ProcessCommand::Signal {
            process_id,
            signal_name,
            request,
            ..
        } => {
            let result = registry.append_event(&process_id, request).await?;
            let ordinal = signal_ordinal_for_event(
                registry.as_ref(),
                &process_id,
                result.event.event_type.as_str(),
                result.event.sequence,
            )
            .await?;
            let key = restate_await_event_key(
                &ExecutionScope::process(process_id.clone()),
                AwaitEventWaitIdentity::process_signal(process_id.clone(), signal_name, ordinal),
            )
            .map_err(|err| PluginError::Session(err.to_string()))?;
            context
                .resolve_event(RestateDurableWaitResolveRequest {
                    address: RestateDurableWaitAddress::for_key(&key),
                    resolution: Resolution::Ok(result.event.payload.clone()),
                })
                .await
                .map_err(|err| {
                    RestateEffectError::BackgroundScheduler(err.to_string()).into_plugin_error()
                })?;
            Ok(ProcessEffectOutcome::Signal {
                event: Box::new(result.event),
            })
        }
    }
}

async fn signal_ordinal_for_event(
    registry: &dyn ProcessRegistry,
    process_id: &str,
    event_type: &str,
    sequence: u64,
) -> Result<u64, PluginError> {
    // COUNT at the store, not a full log fetch: per-signal cost must stay
    // flat for long-lived processes that accumulate large event histories.
    registry
        .count_events_through(process_id, event_type, sequence)
        .await
}

async fn schedule_restate_process<'ctx, C>(
    registry: Arc<dyn ProcessRegistry>,
    registration: lash_core::ProcessRegistration,
    grant: Option<lash_core::ProcessStartGrant>,
    execution_context: lash_core::ProcessExecutionContext,
    context: &C,
) -> Result<ProcessRecord, PluginError>
where
    C: RestateControllerContext<'ctx> + ?Sized,
{
    let process_id = registration.id.clone();
    registry.register_process(registration.clone()).await?;
    if let Some(grant) = grant {
        registry
            .grant_handle(&grant.session_scope, &process_id, grant.descriptor)
            .await?;
    }
    let invocation_id = context
        .start_process_workflow(registration, execution_context)
        .await
        .map_err(|err| {
            RestateEffectError::BackgroundScheduler(err.to_string()).into_plugin_error()
        })?;
    registry
        .set_external_ref(
            &process_id,
            ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!("LashProcessWorkflow/{process_id}"),
                metadata: Some(serde_json::json!({ "invocation_id": invocation_id })),
            },
        )
        .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestateEffectExecution {
    DirectProcess,
    DirectLocal,
    Timer,
    AwaitEvent,
    PeekAwaitEvent,
    JournaledRun,
}

fn restate_effect_execution(command: &RuntimeEffectCommand) -> RestateEffectExecution {
    match command {
        RuntimeEffectCommand::Process { .. } => RestateEffectExecution::DirectProcess,
        RuntimeEffectCommand::ToolBatch { .. } | RuntimeEffectCommand::ExecCode { .. } => {
            RestateEffectExecution::DirectLocal
        }
        RuntimeEffectCommand::Sleep { .. } => RestateEffectExecution::Timer,
        RuntimeEffectCommand::AwaitEvent { .. } => RestateEffectExecution::AwaitEvent,
        RuntimeEffectCommand::PeekAwaitEvent { .. } => RestateEffectExecution::PeekAwaitEvent,
        RuntimeEffectCommand::LlmCall { .. }
        | RuntimeEffectCommand::Direct { .. }
        | RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::Trigger { .. }
        | RuntimeEffectCommand::Checkpoint { .. }
        | RuntimeEffectCommand::SyncExecutionEnvironment { .. }
        | RuntimeEffectCommand::DurableStep { .. } => RestateEffectExecution::JournaledRun,
    }
}

fn restate_effect_name(invocation: &RuntimeInvocation) -> String {
    if let Some(replay_key) = invocation.replay_key() {
        format!("lash:{replay_key}")
    } else if let (Some(kind), Some(effect_id)) = (invocation.effect_kind(), invocation.effect_id())
    {
        format!("lash:{}:{effect_id}", kind.as_str())
    } else {
        "lash:runtime-invocation".to_string()
    }
}

fn validate_recorded_effect_envelope(
    recorded: RecordedRuntimeEffect,
    reconstructed: &CanonicalRuntimeEffectEnvelope,
    trace: Option<&lash_core::RuntimeEffectReplayTrace>,
) -> Result<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>, RuntimeEffectControllerError>
{
    validate_replayed_effect_envelope(
        recorded.envelope.as_ref(),
        reconstructed,
        "restate_effect_hash_mismatch",
        trace,
    )?;
    Ok(recorded.outcome)
}

fn tracing_sleep_error(invocation: &RuntimeInvocation, err: &TerminalError) {
    tracing::warn!(
        session_id = %invocation.scope.session_id,
        effect_id = invocation.effect_id().unwrap_or(""),
        effect_kind = %RuntimeEffectKind::Sleep.as_str(),
        error = %err,
        "Restate durable sleep failed"
    );
}

#[cfg(test)]
mod tests;
