//! Await-event identity and the two durable-wait Restate services.
//!
//! One responsibility: every Lash await-event key is turned into an exact
//! Restate address here, and the two services that own that address live here
//! too — `LashDurableWaitWorkflow` owns the promise and its deadline timer,
//! `LashDurableWaitIndex` owns the session-to-wait index that cancellation,
//! revocation, and session deletion resolve through.
//!
//! Handler-scoped key derivation is deliberately pure: it validates and derives
//! identity without probing the session tombstone. The server-side durable-wait
//! gates and the session lease fence enforce revocation, and the handler
//! controller performs its defense-in-depth probe at the unconditional await
//! boundary so journal shape is replay-deterministic. This intentionally differs
//! from ingress-side `RestateEffectHostController::await_event_key`, which is not
//! executing inside a Restate journal and still refuses revoked sessions eagerly.

use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome, RuntimeError,
};
use restate_sdk::context::{
    ContextAwakeables, ContextClient, ContextPromises, ContextReadState, ContextWriteState,
    ObjectContext, RequestTarget, SharedWorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub(crate) fn restate_await_event_key(
    scope: &ExecutionScope,
    wait: AwaitEventWaitIdentity,
) -> Result<AwaitEventKey, RuntimeError> {
    let key_id = lash_core::facade_support::promise_semantics::derive_key_id(scope, &wait)?;
    Ok(AwaitEventKey {
        scope: scope.clone(),
        wait,
        key_id,
        signature: "restate-handler".to_string(),
    })
}

pub(crate) fn restate_await_event_key_is_valid(key: &AwaitEventKey) -> bool {
    let Ok(expected) = restate_await_event_key(&key.scope, key.wait.clone()) else {
        return false;
    };
    lash_core::facade_support::promise_semantics::constant_time_eq(
        expected.key_id.as_bytes(),
        key.key_id.as_bytes(),
    ) && lash_core::facade_support::promise_semantics::constant_time_eq(
        expected.signature.as_bytes(),
        key.signature.as_bytes(),
    )
}

pub(crate) fn restate_unknown_or_revoked() -> RuntimeError {
    RuntimeError::new(
        lash_core::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
        "await-event key is invalid or revoked",
    )
}
const DURABLE_WAIT_PROMISE_KEY: &str = "resolution";
pub(crate) const DURABLE_WAIT_INDEX_IDENTITY_EPOCH: u8 = 3;
const DURABLE_WAIT_INDEX_EPOCH_KEY: &str = "wait-index/v2/identity-epoch";
pub(crate) const DURABLE_WAIT_INDEX_METADATA_KEY: &str = "wait-index/v2/metadata";
const DURABLE_WAIT_INDEX_WAIT_PREFIX: &str = "wait-index/v2/wait/";
const DURABLE_WAIT_INDEX_RESOLUTION_PREFIX: &str = "wait-index/v2/resolution/";
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
    #[serde(
        default,
        skip_serializing_if = "RestateDurableWaitAwakeableKind::is_resolution"
    )]
    pub(crate) kind: RestateDurableWaitAwakeableKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestateDurableWaitAwakeableKind {
    #[default]
    Resolution,
    ProcessAwait,
}

impl RestateDurableWaitAwakeableKind {
    fn is_resolution(&self) -> bool {
        *self == Self::Resolution
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestateProcessAwaitWake {
    TurnCancelled,
    SessionRevoked,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum RestateDurableWaitRegistration {
    Registered,
    Resolved(Resolution),
    Revoked,
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub(crate) struct RestateDurableWaitIndexState {
    pub(crate) revoked: bool,
    pub(crate) waits: Vec<RestateDurableWaitAddress>,
    #[serde(default)]
    pub(crate) awakeables: Vec<RestateDurableWaitAwakeableRequest>,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub(crate) struct RestateDurableWaitIndexMetadata {
    revoked: bool,
    #[serde(default)]
    awakeables: Vec<RestateDurableWaitAwakeableRequest>,
}
fn resolve_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
    resolution: Resolution,
) {
    match request.kind {
        RestateDurableWaitAwakeableKind::Resolution => {
            ctx.resolve_awakeable(&request.awakeable_id, Json(resolution));
        }
        RestateDurableWaitAwakeableKind::ProcessAwait => {
            ctx.resolve_awakeable(
                &request.awakeable_id,
                Json(RestateProcessAwaitWake::TurnCancelled),
            );
        }
    }
}

fn revoke_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
) {
    match request.kind {
        RestateDurableWaitAwakeableKind::Resolution => {
            ctx.resolve_awakeable(&request.awakeable_id, Json(Resolution::Cancelled));
        }
        RestateDurableWaitAwakeableKind::ProcessAwait => {
            ctx.resolve_awakeable(
                &request.awakeable_id,
                Json(RestateProcessAwaitWake::SessionRevoked),
            );
        }
    }
}
pub(crate) fn restate_durable_wait_request(
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
    pub(crate) event: RestateDurableWaitAwaitRequest,
}
/// One durable Restate workflow per Lash await-event identity.
///
/// Bind [`LashDurableWaitWorkflowImpl::serve`] on every endpoint that runs a
/// [`RestateRuntimeEffectController`](crate::RestateRuntimeEffectController). The
/// workflow key is a stable digest of the full Lash [`AwaitEventKey`], so all
/// execution-scope variants share the same exact-address resolution path.
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

        // A workflow that wakes after a deployment upgrade must cross the
        // index epoch gate before it can return a resolution. Restate replays
        // the old registration command, so this settle call is the first new
        // command a previously parked invocation can execute. Fully parked v2
        // invocations never reach it; the module and migration docs therefore
        // require a pre-cutover drain/purge rather than claiming self-healing.
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

pub(crate) fn durable_wait_index_object_key(address: &RestateDurableWaitAddress) -> String {
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

pub(crate) fn validate_durable_wait_index_epoch(
    stored_epoch: Option<u8>,
    existing_keys: &[String],
) -> Result<(), String> {
    match stored_epoch {
        Some(DURABLE_WAIT_INDEX_IDENTITY_EPOCH) => Ok(()),
        Some(epoch) => Err(format!(
            "Lash Restate await-event identity epoch {epoch} is incompatible with epoch {DURABLE_WAIT_INDEX_IDENTITY_EPOCH}; drain and recreate LashDurableWaitIndex and LashDurableWaitWorkflow state before opening this deployment"
        )),
        None if existing_keys.is_empty() => Ok(()),
        None => Err(format!(
            "pre-cutover Lash Restate await-event state was found without identity epoch {DURABLE_WAIT_INDEX_IDENTITY_EPOCH}; drain and recreate LashDurableWaitIndex and LashDurableWaitWorkflow state before opening this deployment"
        )),
    }
}

async fn open_durable_wait_index_epoch(ctx: &ObjectContext<'_>) -> Result<(), TerminalError> {
    let stored_epoch = ctx
        .get::<Json<u8>>(DURABLE_WAIT_INDEX_EPOCH_KEY)
        .await?
        .map(|Json(epoch)| epoch);
    let existing_keys = if stored_epoch == Some(DURABLE_WAIT_INDEX_IDENTITY_EPOCH) {
        Vec::new()
    } else {
        ctx.get_keys().await?
    };
    validate_durable_wait_index_epoch(stored_epoch, &existing_keys).map_err(TerminalError::new)?;
    if stored_epoch.is_none() {
        ctx.set(
            DURABLE_WAIT_INDEX_EPOCH_KEY,
            Json(DURABLE_WAIT_INDEX_IDENTITY_EPOCH),
        );
    }
    Ok(())
}

/// Open the v2 wait index only inside the await-event v3 identity epoch.
///
/// Restate object state is not part of an invocation's replayed journal: these
/// index handlers are short-lived single calls, so changing their command
/// sequence does not alter an in-flight multi-call journal. Object state does,
/// however, survive a deployment upgrade. Any object with pre-cutover state
/// but no matching epoch marker is rejected with a recreate instruction; old
/// wait addresses are never migrated into the v3 identity world.
async fn load_durable_wait_index_metadata(
    ctx: &ObjectContext<'_>,
) -> Result<RestateDurableWaitIndexMetadata, TerminalError> {
    open_durable_wait_index_epoch(ctx).await?;
    if let Some(Json(metadata)) = ctx
        .get::<Json<RestateDurableWaitIndexMetadata>>(DURABLE_WAIT_INDEX_METADATA_KEY)
        .await?
    {
        return Ok(metadata);
    }

    let metadata = RestateDurableWaitIndexMetadata::default();
    ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata.clone()));
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

pub(crate) fn split_cancellable_waits(
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
        Ok(Json(load_durable_wait_index_metadata(&ctx).await?.revoked))
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
            resolve_durable_wait_awakeable(&ctx, &request, resolution);
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
            resolve_durable_wait_awakeable(&ctx, &request, resolution);
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
                resolve_durable_wait_awakeable(&ctx, &entry, terminal.clone());
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
        ctx.set(
            DURABLE_WAIT_INDEX_EPOCH_KEY,
            Json(DURABLE_WAIT_INDEX_IDENTITY_EPOCH),
        );
        ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata));
        for entry in awakeables {
            revoke_durable_wait_awakeable(&ctx, &entry);
        }
        resolve_indexed_waits(&ctx, waits).await?;
        Ok(Json(()))
    }
}
