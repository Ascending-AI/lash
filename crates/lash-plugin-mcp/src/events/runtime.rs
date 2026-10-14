//! The host's handle on MCP event subscriptions, and the port to the core
//! it reaches them through.
//!
//! A subscription is a process of the subscription engine
//! ([`MCP_EVENTS_ENGINE`]). Whoever creates one — a host through
//! [`McpEvents::subscribe`], or a model through `subscribe_event` — starts
//! the same keyed process; cancelling it is the unsubscribe. Its status is
//! the projection core records with each of its transitions, read without
//! running it.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use lash_core::{
    ArgsMismatch, ArgsMode, EngineLaunch, EngineStatus, KeyName, LifetimeDecision,
    ProcessDefinition, ProcessId, ProcessOriginator, ProcessStartRequest, Resolution,
    ResolveAnswer, SessionCreateRequest, SessionId, TurnId, TurnInput,
};
use serde_json::{Map, Value};

use super::client::McpEventsCallError;
use super::config::McpDeliveryMode;
use super::engine::{MCP_EVENTS_ENGINE, MCP_EVENTS_RESUME_KEY};
use super::spec::{McpReactionTarget, McpSubscriptionSpec, McpWorkflowReaction};
use super::status::McpSubscriptionStatus;
use crate::pool::McpConnectionPool;

/// How long a subscribe waits for the server's event listing.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the core refused or failed an operation a subscription asked of it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpEventsPortError {
    /// No core is bound to the handle yet, or any more.
    #[error("no lash core is bound to the MCP events handle")]
    Unbound,
    /// The key already names different content.
    #[error("the key already names different content: {message}")]
    Conflict { message: String },
    /// Refused for good.
    #[error("refused: {message}")]
    Refused { message: String },
    /// Failed now; a later attempt may succeed.
    #[error("unavailable: {message}")]
    Unavailable { message: String },
}

/// What subscriptions need of a lash core: the facade implements it over
/// `LashCore` and binds it with [`McpEvents::bind`].
#[async_trait::async_trait]
pub trait McpEventsCore: Send + Sync {
    /// The launch a subscription process runs under: an environment that
    /// links no effect, since the engine performs none of its own.
    async fn subscription_launch(&self) -> Result<EngineLaunch, McpEventsPortError>;

    /// The lifetime that ends a process when `session` ends.
    async fn session_lifetime(
        &self,
        session: &SessionId,
    ) -> Result<LifetimeDecision, McpEventsPortError>;

    /// Start `request`, keyed by the caller: a repeat of the same start
    /// answers the process the first one started.
    async fn start(&self, request: ProcessStartRequest) -> Result<ProcessId, McpEventsPortError>;

    /// Request `process`'s cancellation.
    async fn cancel(&self, process: &ProcessId) -> Result<(), McpEventsPortError>;

    /// Queue `input` as turn `turn` in `session`: identical content under
    /// the same id is accepted once; different content is a conflict.
    async fn send(
        &self,
        session: &SessionId,
        turn: TurnId,
        input: TurnInput,
    ) -> Result<(), McpEventsPortError>;

    /// Check `args` against `definition`'s authoritative signature.
    async fn check_args(
        &self,
        definition: &ProcessDefinition,
        args: &Map<String, Value>,
        mode: ArgsMode,
    ) -> Result<Result<(), ArgsMismatch>, McpEventsPortError>;

    /// The subscription processes, observed by `observer` when given: the
    /// live ones, and the ended ones too with `ended`.
    async fn subscriptions(
        &self,
        observer: Option<&SessionId>,
        ended: bool,
    ) -> Result<Vec<ProcessId>, McpEventsPortError>;

    /// The status `process`'s engine projected at its last transition.
    async fn engine_status(
        &self,
        process: &ProcessId,
    ) -> Result<Option<EngineStatus>, McpEventsPortError>;

    /// The keys `process`'s engine pinned and still waits on.
    async fn pinned_keys(
        &self,
        process: &ProcessId,
    ) -> Result<Vec<lash_core::waits::PinnedEngineKey>, McpEventsPortError>;

    /// Resolve the wait `key` names, first writer wins.
    async fn resolve(
        &self,
        key: &str,
        resolution: Resolution,
    ) -> Result<ResolveAnswer, McpEventsPortError>;
}

/// Why a subscription operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpEventsError {
    /// The server's events could not be read.
    #[error(transparent)]
    Server(#[from] McpEventsCallError),
    /// The server lists no event type of this name.
    #[error("MCP server `{server}` lists no event `{event}`")]
    UnknownEvent { server: String, event: String },
    /// None of the host's modes is one the event type offers.
    #[error(
        "MCP server `{server}` offers event `{event}` by {offered:?}, none of the host's modes"
    )]
    NoMode {
        server: String,
        event: String,
        offered: Vec<String>,
    },
    /// The reaction is not one this caller may choose.
    #[error("the reaction is not allowed: {0}")]
    Reaction(String),
    /// The workflow's fixed arguments do not fit its definition.
    #[error("the workflow's arguments do not fit its definition (`{code}`): {message}")]
    Arguments { code: String, message: String },
    /// The id does not spell a process id.
    #[error("`{0}` is not a subscription id")]
    InvalidId(String),
    /// The process is not an MCP event subscription.
    #[error("process `{0}` is not an MCP event subscription")]
    NotSubscription(ProcessId),
    /// The core refused or failed.
    #[error(transparent)]
    Core(#[from] McpEventsPortError),
}

/// A subscription a host or model creates.
#[derive(Clone, Debug)]
pub struct McpSubscribeRequest {
    /// The server, by its configured name.
    pub server: String,
    /// The event type's name.
    pub event: String,
    /// The subscription arguments.
    pub arguments: Map<String, Value>,
    /// Where each event reacts.
    pub target: McpReactionTarget,
    /// The start key: the same key and request answer the same subscription.
    pub key: Vec<u8>,
    /// The delivery generation the keys name.
    pub generation: u64,
    /// The sessions that may list and address the subscription.
    pub observers: Vec<SessionId>,
    /// What ends it, besides an unsubscribe.
    pub lifetime: LifetimeDecision,
    /// Who created it.
    pub originator: ProcessOriginator,
}

impl McpSubscribeRequest {
    /// A host subscription under `key`: detached, unobserved, generation 0.
    pub fn host(
        server: impl Into<String>,
        event: impl Into<String>,
        arguments: Map<String, Value>,
        target: McpReactionTarget,
        key: impl AsRef<[u8]>,
    ) -> Self {
        let server = server.into();
        Self {
            originator: ProcessOriginator::host_scoped(format!("mcp-events:{server}")),
            server,
            event: event.into(),
            arguments,
            target,
            key: key.as_ref().to_vec(),
            generation: 0,
            observers: Vec::new(),
            lifetime: LifetimeDecision::Detached,
        }
    }

    /// The sessions that may list and address it.
    #[must_use]
    pub fn with_observers(mut self, observers: impl IntoIterator<Item = SessionId>) -> Self {
        self.observers = observers.into_iter().collect();
        self
    }

    /// A delivery generation: the same occurrences react again under a new
    /// one.
    #[must_use]
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }
}

/// One subscription as a list shows it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct McpSubscription {
    /// The subscription's process.
    pub id: ProcessId,
    /// Its status at its last transition; `None` before its first.
    pub status: Option<McpSubscriptionStatus>,
}

/// The reactions a host registers for a model's subscriptions to name.
#[derive(Default)]
struct HostReactions {
    isolated: Option<SessionCreateRequest>,
    workflows: BTreeMap<String, McpWorkflowReaction>,
}

/// The fetches a handle's subscriptions have in flight against their
/// servers: what a host's metrics, or a simulated clock that must wait out
/// work off the runtime, read.
#[derive(Debug, Default)]
pub struct McpEventsActivity {
    in_flight: std::sync::atomic::AtomicUsize,
    ended: std::sync::atomic::AtomicUsize,
}

impl McpEventsActivity {
    /// Fetches running now.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Fetches that ended so far.
    pub fn ended(&self) -> usize {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Count one fetch in flight until the guard drops.
    pub(crate) fn fetch(self: &Arc<Self>) -> FetchGuard {
        self.in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        FetchGuard(Arc::clone(self))
    }
}

/// One fetch in flight; it ends when dropped, also when its body is.
pub(crate) struct FetchGuard(Arc<McpEventsActivity>);

impl Drop for FetchGuard {
    fn drop(&mut self) {
        self.0
            .ended
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

struct EventsInner {
    pool: Arc<McpConnectionPool>,
    activity: Arc<McpEventsActivity>,
    core: RwLock<Option<Arc<dyn McpEventsCore>>>,
    reactions: RwLock<HostReactions>,
}

/// The host's handle on MCP event subscriptions
/// ([`McpPluginFactory::events`](crate::McpPluginFactory::events)). Clones
/// share one binding.
#[derive(Clone)]
pub struct McpEvents {
    inner: Arc<EventsInner>,
}

impl std::fmt::Debug for McpEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpEvents")
            .field("bound", &self.core().is_ok())
            .finish_non_exhaustive()
    }
}

impl McpEvents {
    pub(crate) fn new(pool: Arc<McpConnectionPool>) -> Self {
        Self {
            inner: Arc::new(EventsInner {
                pool,
                activity: Arc::default(),
                core: RwLock::new(None),
                reactions: RwLock::new(HostReactions::default()),
            }),
        }
    }

    /// Bind the core subscriptions start, deliver and read through. Until a
    /// core is bound, a delivery waits and a subscribe is refused.
    pub fn bind(&self, core: Arc<dyn McpEventsCore>) {
        if let Ok(mut slot) = self.inner.core.write() {
            *slot = Some(core);
        }
    }

    /// Drop the bound core: the plugin's shutdown does, so the core and the
    /// plugin it holds do not keep each other alive.
    pub fn unbind(&self) {
        if let Ok(mut slot) = self.inner.core.write() {
            *slot = None;
        }
    }

    /// The child session a model's isolated reaction creates per event.
    pub fn set_isolated_reaction(&self, child: SessionCreateRequest) {
        if let Ok(mut reactions) = self.inner.reactions.write() {
            reactions.isolated = Some(child);
        }
    }

    /// A workflow reaction a model may name as `name`, where the server's
    /// reaction profile lists it. Its tools come from its launch, never from
    /// the model that names it.
    pub fn register_workflow_reaction(
        &self,
        name: impl Into<String>,
        reaction: McpWorkflowReaction,
    ) {
        if let Ok(mut reactions) = self.inner.reactions.write() {
            reactions.workflows.insert(name.into(), reaction);
        }
    }

    pub(crate) fn isolated_reaction(&self) -> Option<SessionCreateRequest> {
        self.inner
            .reactions
            .read()
            .ok()
            .and_then(|reactions| reactions.isolated.clone())
    }

    pub(crate) fn workflow_reaction(&self, name: &str) -> Option<McpWorkflowReaction> {
        self.inner
            .reactions
            .read()
            .ok()
            .and_then(|reactions| reactions.workflows.get(name).cloned())
    }

    /// The fetches this handle's subscriptions have in flight.
    pub fn activity(&self) -> Arc<McpEventsActivity> {
        Arc::clone(&self.inner.activity)
    }

    pub(crate) fn pool(&self) -> &Arc<McpConnectionPool> {
        &self.inner.pool
    }

    pub(crate) fn core(&self) -> Result<Arc<dyn McpEventsCore>, McpEventsPortError> {
        self.inner
            .core
            .read()
            .ok()
            .and_then(|slot| slot.clone())
            .ok_or(McpEventsPortError::Unbound)
    }

    /// The event types `server` lists.
    ///
    /// # Errors
    ///
    /// The server's events are off, unreachable or refused.
    pub async fn list_events(
        &self,
        server: &str,
    ) -> Result<Vec<super::wire::McpEventDescriptor>, McpEventsError> {
        Ok(self.inner.pool.list_events(server, LIST_TIMEOUT).await?)
    }

    /// Subscribe: start the subscription's keyed process, which reacts to
    /// each event from then on. A repeat under the same key answers the same
    /// subscription.
    ///
    /// # Errors
    ///
    /// The server's events are off or refused, the event offers none of the
    /// host's modes, a workflow's fixed arguments do not fit, or the core
    /// refused the start.
    pub async fn subscribe(
        &self,
        request: McpSubscribeRequest,
    ) -> Result<ProcessId, McpEventsError> {
        let core = self.core()?;
        let Some((events, _)) = self.inner.pool.events_connection(&request.server) else {
            return Err(McpEventsCallError::UnknownServer {
                server: request.server,
            }
            .into());
        };
        let Some(config) = events.enabled().cloned() else {
            return Err(McpEventsCallError::Disabled {
                server: request.server,
            }
            .into());
        };
        let listed = self.list_events(&request.server).await?;
        let Some(descriptor) = listed.into_iter().find(|event| event.name == request.event) else {
            return Err(McpEventsError::UnknownEvent {
                server: request.server,
                event: request.event,
            });
        };
        let mode = choose_mode(&config.modes, &descriptor.delivery).ok_or_else(|| {
            McpEventsError::NoMode {
                server: request.server.clone(),
                event: request.event.clone(),
                offered: descriptor.delivery.clone(),
            }
        })?;
        if let McpReactionTarget::Workflow(workflow) = &request.target {
            check_partial(core.as_ref(), workflow).await?;
        }
        let spec = McpSubscriptionSpec {
            server: request.server,
            event: request.event,
            arguments: request.arguments,
            mode,
            target: request.target,
            bounds: config.bounds,
            generation: request.generation,
        };
        let payload = serde_json::to_value(&spec).map_err(|error| McpEventsPortError::Refused {
            message: error.to_string(),
        })?;
        let start = ProcessStartRequest::engine(
            MCP_EVENTS_ENGINE,
            payload,
            core.subscription_launch().await?,
            request.originator,
            request.lifetime,
        )
        .with_host_start_key(&request.key)
        .with_observers(request.observers);
        Ok(core.start(start).await?)
    }

    /// Unsubscribe: cancel the subscription's process. Once the cancel
    /// commits, it dispatches nothing more, and an open stream is cancelled
    /// at the server.
    ///
    /// # Errors
    ///
    /// The process is not a subscription, or the core refused.
    pub async fn unsubscribe(&self, id: &ProcessId) -> Result<(), McpEventsError> {
        let core = self.core()?;
        self.require_subscription(core.as_ref(), id).await?;
        Ok(core.cancel(id).await?)
    }

    /// The live subscriptions, observed by `observer` when given.
    ///
    /// # Errors
    ///
    /// The core refused the read.
    pub async fn list(
        &self,
        observer: Option<&SessionId>,
    ) -> Result<Vec<McpSubscription>, McpEventsError> {
        let core = self.core()?;
        let mut out = Vec::new();
        for id in core.subscriptions(observer, false).await? {
            let status = core.engine_status(&id).await?.and_then(decode_status);
            out.push(McpSubscription { id, status });
        }
        Ok(out)
    }

    /// Whether `observer` observes subscription `id`, live or ended.
    pub(crate) async fn observes(
        &self,
        observer: &SessionId,
        id: &ProcessId,
    ) -> Result<bool, McpEventsError> {
        let core = self.core()?;
        Ok(core
            .subscriptions(Some(observer), true)
            .await?
            .iter()
            .any(|observed| observed == id))
    }

    /// A subscription's status: its gaps, delivery failures and park
    /// reason, as of its last transition, or `None` before its first. It
    /// survives any node's restart.
    ///
    /// # Errors
    ///
    /// The process is not a subscription, or the core refused the read.
    pub async fn status(
        &self,
        id: &ProcessId,
    ) -> Result<Option<McpSubscriptionStatus>, McpEventsError> {
        let core = self.core()?;
        self.require_subscription(core.as_ref(), id).await
    }

    /// Resume a parked subscription: resolve its resume key, so it fetches
    /// again with its failures cleared. Answers whether it was parked.
    ///
    /// # Errors
    ///
    /// The process is not a subscription, or the core refused.
    pub async fn resume(&self, id: &ProcessId) -> Result<bool, McpEventsError> {
        let core = self.core()?;
        self.require_subscription(core.as_ref(), id).await?;
        let name = KeyName(MCP_EVENTS_RESUME_KEY.to_owned());
        let Some(pinned) = core
            .pinned_keys(id)
            .await?
            .into_iter()
            .find(|pinned| pinned.name == name)
        else {
            return Ok(false);
        };
        let answer = core
            .resolve(
                pinned.key.as_str(),
                Resolution::Ok(serde_json::json!({ "resumed": true })),
            )
            .await?;
        Ok(matches!(
            answer,
            ResolveAnswer::Resolved | ResolveAnswer::AlreadyResolved
        ))
    }

    /// `id`'s subscription status, or `None` before its first transition
    /// records one.
    async fn require_subscription(
        &self,
        core: &dyn McpEventsCore,
        id: &ProcessId,
    ) -> Result<Option<McpSubscriptionStatus>, McpEventsError> {
        match core.engine_status(id).await? {
            None => Ok(None),
            Some(status) => decode_status(status)
                .map(Some)
                .ok_or_else(|| McpEventsError::NotSubscription(id.clone())),
        }
    }
}

/// The subscription status a recorded engine status holds, when it is one.
fn decode_status(status: EngineStatus) -> Option<McpSubscriptionStatus> {
    if status.format.kind != MCP_EVENTS_ENGINE {
        return None;
    }
    serde_json::from_value(status.status).ok()
}

/// The first of the host's modes the event type offers.
fn choose_mode(modes: &[McpDeliveryMode], offered: &[String]) -> Option<McpDeliveryMode> {
    modes
        .iter()
        .copied()
        .find(|mode| offered.iter().any(|offered| offered == mode.wire()))
}

/// Check a workflow reaction's fixed arguments at registration.
pub(crate) async fn check_partial(
    core: &dyn McpEventsCore,
    workflow: &McpWorkflowReaction,
) -> Result<(), McpEventsError> {
    if let Some(argument) = workflow
        .mapping
        .from_event
        .keys()
        .find(|argument| workflow.mapping.fixed.contains_key(*argument))
    {
        return Err(McpEventsError::Reaction(format!(
            "argument `{argument}` is both fixed and read from the event"
        )));
    }
    core.check_args(
        &workflow.definition,
        &workflow.mapping.partial(),
        ArgsMode::Partial,
    )
    .await?
    .map_err(|refusal| McpEventsError::Arguments {
        code: refusal.code().to_owned(),
        message: refusal.to_string(),
    })
}
