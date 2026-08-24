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
//!
//! Identity epoch 4 is a hard cutover: every wait request and indexed state
//! value carries the full [`AwaitEventKey`] preimage, and handlers derive scope,
//! classification, and workflow address locally. Deployments must drain and
//! recreate both durable-wait services before upgrading; there is no tolerant
//! decoder, address migration, or overlap window for pre-epoch-4 state.

use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome, RuntimeError,
};
use restate_sdk::context::{
    ContextAwakeables, ContextClient, ContextPromises, ContextReadState, ContextWriteState,
    ObjectContext, SharedWorkflowContext,
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
pub(crate) const DURABLE_WAIT_INDEX_IDENTITY_EPOCH: u8 = 4;
const DURABLE_WAIT_INDEX_EPOCH_KEY: &str = "wait-index/v2/identity-epoch";
pub(crate) const DURABLE_WAIT_INDEX_METADATA_KEY: &str = "wait-index/v2/metadata";
const DURABLE_WAIT_INDEX_WAIT_PREFIX: &str = "wait-index/v2/wait/";
const DURABLE_WAIT_INDEX_RESOLUTION_PREFIX: &str = "wait-index/v2/resolution/";
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RestateDurableWaitAddress {
    pub workflow_key: String,
    pub(crate) scope: RestateDurableWaitScope,
    pub(crate) classification: RestateDurableWaitClassification,
}

impl RestateDurableWaitAddress {
    /// Derive the exact Restate workflow address for a Lash await-event key.
    pub fn for_key(key: &AwaitEventKey) -> Self {
        Self {
            workflow_key: format!("{:x}", Sha256::digest(key.key_id.as_bytes())),
            scope: match key.scope.session_id() {
                Some(session_id) => RestateDurableWaitScope::Session(session_id.to_string()),
                None => RestateDurableWaitScope::Unscoped,
            },
            classification: if key.wait.is_turn_control() {
                RestateDurableWaitClassification::TurnControl
            } else {
                RestateDurableWaitClassification::DurableWait
            },
        }
    }

    /// Return the keyed virtual-object address that owns this wait's index state.
    pub fn index_key(&self) -> String {
        self.scope.index_key(&self.workflow_key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RestateDurableWaitScope {
    Session(String),
    Unscoped,
}

impl RestateDurableWaitScope {
    pub fn index_key(&self, workflow_key: &str) -> String {
        match self {
            Self::Session(session_id) => session_id.clone(),
            Self::Unscoped => format!("unscoped:{workflow_key}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RestateDurableWaitClassification {
    DurableWait,
    TurnControl,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAwaitRequest {
    pub key: AwaitEventKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[cfg(test)]
impl RestateDurableWaitAwaitRequest {
    pub(crate) fn address(&self) -> RestateDurableWaitAddress {
        RestateDurableWaitAddress::for_key(&self.key)
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitResolveRequest {
    pub key: AwaitEventKey,
    pub resolution: Resolution,
}

#[cfg(test)]
impl RestateDurableWaitResolveRequest {
    pub(crate) fn address(&self) -> RestateDurableWaitAddress {
        RestateDurableWaitAddress::for_key(&self.key)
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitIndexRequest {
    pub key: AwaitEventKey,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitSettleRequest {
    pub key: AwaitEventKey,
    pub resolution: Resolution,
}

/// One turn-cancel gate entry: the awakeable the index resolves when this
/// session's turn-control wait settles or the session is revoked.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAwakeableRequest {
    pub key: AwaitEventKey,
    pub awakeable_id: String,
}

/// Why a registered turn-cancel gate awakeable fired.
///
/// Every gate — sleep, await-event, process await — takes this one payload, so
/// the index resolves a gate entry without knowing which wait registered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestateTurnCancelWake {
    TurnCancelled,
    SessionRevoked,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum RestateDurableWaitRegistration {
    Registered,
    Resolved(Resolution),
    Revoked,
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub(crate) struct RestateDurableWaitIndexMetadata {
    revoked: bool,
    #[serde(default)]
    awakeables: Vec<RestateDurableWaitAwakeableRequest>,
}
/// Fire a gate entry because the turn-control wait it guards has settled.
///
/// The wait's own `Resolution` is deliberately not forwarded: a gate entry only
/// ever guards a turn-control address, so any settlement of it is the turn
/// being cancelled, and the waiter needs no more than that.
fn resolve_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
) {
    ctx.resolve_awakeable(
        &request.awakeable_id,
        Json(RestateTurnCancelWake::TurnCancelled),
    );
}

/// Fire a gate entry because the whole session was revoked out from under it.
fn revoke_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
) {
    ctx.resolve_awakeable(
        &request.awakeable_id,
        Json(RestateTurnCancelWake::SessionRevoked),
    );
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
        key: key.clone(),
        timeout_ms,
    }
}
/// How one wait raced against durable turn cancellation ended.
///
/// Every wait that can be cut short by turn cancellation — a timer, an
/// await-event, a process terminal wait — reports through this one type, so a
/// caller reads the same three answers whatever it was waiting on. `T` is
/// whatever the wait produces when it wins its own race.
#[doc(hidden)]
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum RestateTurnCancelRaceOutcome<T> {
    /// The wait completed on its own terms.
    Completed(T),
    /// The turn was cancelled while the wait was parked.
    TurnCancelled,
    /// The session was revoked, so the turn has no ground left to stand on.
    SessionRevoked { session_id: String },
}

/// The verdict [`register_turn_cancel_gate`] returns for one gate entry.
pub(crate) enum RestateTurnCancelGate {
    /// The entry is live; retire it with [`retire_turn_cancel_gate`] if the
    /// guarded wait wins.
    Registered(RestateDurableWaitAwakeableRequest),
    /// The session was already revoked, so no entry was created and the caller
    /// must unwind instead of parking.
    Revoked,
}

/// Register this invocation's turn-cancel gate entry against the session index.
///
/// `awakeable_id` is created by the caller, never here: the awakeable and the
/// wait it guards are journaled commands whose relative order is part of the
/// deployed journal shape (FIG-790), so only the call site may decide when each
/// is emitted. This helper owns the one step that is identical everywhere —
/// the `register_awakeable` call and its revocation verdict.
pub(crate) async fn register_turn_cancel_gate<'ctx, C>(
    context: &C,
    session_id: &str,
    key: AwaitEventKey,
    awakeable_id: String,
) -> Result<RestateTurnCancelGate, TerminalError>
where
    C: ContextClient<'ctx>,
{
    let entry = RestateDurableWaitAwakeableRequest { key, awakeable_id };
    let register = context
        .object_client::<LashDurableWaitIndexClient>(session_id)
        .register_awakeable(Json(entry.clone()));
    let Json(registration) = register.call().await?;
    Ok(match registration {
        RestateDurableWaitRegistration::Revoked => RestateTurnCancelGate::Revoked,
        RestateDurableWaitRegistration::Registered
        | RestateDurableWaitRegistration::Resolved(_) => RestateTurnCancelGate::Registered(entry),
    })
}

/// Drop a gate entry whose guarded wait won its race.
///
/// Leaving the entry behind would strand an awakeable the index still believes
/// it owes a wake to, so every winning branch must retire its gate.
pub(crate) async fn retire_turn_cancel_gate<'ctx, C>(
    context: &C,
    session_id: &str,
    entry: RestateDurableWaitAwakeableRequest,
) -> Result<(), TerminalError>
where
    C: ContextClient<'ctx>,
{
    let unregister = context
        .object_client::<LashDurableWaitIndexClient>(session_id)
        .unregister_awakeable(Json(entry));
    let Json(()) = unregister.call().await?;
    Ok(())
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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LashDurableWaitWorkflowImpl;

impl LashDurableWaitWorkflow for LashDurableWaitWorkflowImpl {
    async fn await_resolution(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<RestateDurableWaitAwaitRequest>,
    ) -> HandlerResult<Json<Resolution>> {
        let address = verify_durable_wait_workflow_key(ctx.key(), &request.key)?;
        let index_key = durable_wait_index_object_key(&address);
        let registration = ctx
            .object_client::<LashDurableWaitIndexClient>(index_key.clone())
            .register(Json(RestateDurableWaitIndexRequest {
                key: request.key.clone(),
            }));
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
        let settle = ctx
            .object_client::<LashDurableWaitIndexClient>(index_key)
            .settle(Json(RestateDurableWaitSettleRequest {
                key: request.key,
                resolution: resolution.clone(),
            }));
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
        let _address = verify_durable_wait_workflow_key(ctx.key(), &request.key)?;
        if let Some(payload) = ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
            let terminal = serde_json::from_str(&payload).map_err(TerminalError::from_error)?;
            return Ok(Json(ResolveOutcome::AlreadyResolved { terminal }));
        }
        let payload =
            serde_json::to_string(&request.resolution).map_err(TerminalError::from_error)?;
        ctx.resolve_promise(DURABLE_WAIT_PROMISE_KEY, payload);
        Ok(Json(ResolveOutcome::Accepted))
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
pub(crate) fn durable_wait_index_state_key(address: &RestateDurableWaitAddress) -> String {
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

fn verify_durable_wait_workflow_key(
    workflow_key: &str,
    key: &AwaitEventKey,
) -> Result<RestateDurableWaitAddress, TerminalError> {
    if !restate_await_event_key_is_valid(key) {
        return Err(TerminalError::new(
            "inconsistent durable-wait key preimage: scope, wait, key_id, and signature must agree",
        ));
    }
    let address = RestateDurableWaitAddress::for_key(key);
    if address.workflow_key == workflow_key {
        return Ok(address);
    }
    Err(TerminalError::new(format!(
        "durable-wait workflow key mismatch: request derives {}, invocation addresses {workflow_key}",
        address.workflow_key
    )))
}

fn derive_durable_wait_index_address(
    object_key: &str,
    key: &AwaitEventKey,
) -> Result<RestateDurableWaitAddress, TerminalError> {
    if !restate_await_event_key_is_valid(key) {
        return Err(TerminalError::new(
            "inconsistent durable-wait key preimage: scope, wait, key_id, and signature must agree",
        ));
    }
    let address = RestateDurableWaitAddress::for_key(key);
    let expected = address.index_key();
    if expected == object_key {
        return Ok(address);
    }
    Err(TerminalError::new(format!(
        "durable-wait index key mismatch: request derives {expected}, invocation addresses {object_key}"
    )))
}

pub(crate) fn durable_wait_address_from_state_key(
    key: &AwaitEventKey,
    state_key: &str,
) -> Option<RestateDurableWaitAddress> {
    let suffix = state_key.strip_prefix(DURABLE_WAIT_INDEX_WAIT_PREFIX)?;
    let (classification, workflow_key) = suffix.split_once('/')?;
    if workflow_key.is_empty() || workflow_key.contains('/') {
        return None;
    }
    let state_classification = match classification {
        "durable" => RestateDurableWaitClassification::DurableWait,
        "control" => RestateDurableWaitClassification::TurnControl,
        _ => return None,
    };
    let address = RestateDurableWaitAddress::for_key(key);
    (address.workflow_key == workflow_key && address.classification == state_classification)
        .then_some(address)
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

/// Open the v2 wait index only inside the await-event v4 identity epoch.
///
/// Restate object state is not part of an invocation's replayed journal: these
/// index handlers are short-lived single calls, so changing their command
/// sequence does not alter an in-flight multi-call journal. Object state does,
/// however, survive a deployment upgrade. Any object with pre-cutover state
/// but no matching epoch marker is rejected with a recreate instruction; old
/// wait addresses are never migrated into the v4 identity world.
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

async fn load_indexed_waits(ctx: &ObjectContext<'_>) -> Result<Vec<AwaitEventKey>, TerminalError> {
    let mut waits = Vec::new();
    for state_key in ctx
        .get_keys()
        .await?
        .into_iter()
        .filter(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_WAIT_PREFIX))
    {
        let Json(key) = ctx
            .get::<Json<AwaitEventKey>>(&state_key)
            .await?
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "durable-wait index entry {state_key} has no key preimage"
                ))
            })?;
        let address = durable_wait_address_from_state_key(&key, &state_key).ok_or_else(|| {
            TerminalError::new(format!(
                "durable-wait index entry {state_key} does not match its key preimage"
            ))
        })?;
        let expected_object_key = address.index_key();
        if expected_object_key != ctx.key() {
            return Err(TerminalError::new(format!(
                "durable-wait index entry {state_key} derives object {expected_object_key}, but is stored under {}",
                ctx.key()
            )));
        }
        waits.push(key);
    }
    Ok(waits)
}

async fn resolve_indexed_waits(
    ctx: &ObjectContext<'_>,
    waits: Vec<AwaitEventKey>,
    mirror_outcomes: bool,
) -> HandlerResult<()> {
    for key in waits {
        let address = RestateDurableWaitAddress::for_key(&key);
        let workflow_key = address.workflow_key.clone();
        let resolution = Resolution::Cancelled;
        let resolve = ctx
            .workflow_client::<LashDurableWaitWorkflowClient>(workflow_key)
            .resolve(Json(RestateDurableWaitResolveRequest {
                key,
                resolution: resolution.clone(),
            }));
        let Json(outcome) = resolve.call().await?;
        if mirror_outcomes {
            mirror_resolve_outcome(ctx, &address, resolution, &outcome);
        }
    }
    Ok(())
}

fn mirror_resolve_outcome(
    ctx: &ObjectContext<'_>,
    address: &RestateDurableWaitAddress,
    accepted_terminal: Resolution,
    outcome: &ResolveOutcome,
) {
    let terminal = match outcome {
        ResolveOutcome::AlreadyResolved { terminal } => terminal.clone(),
        ResolveOutcome::Accepted => accepted_terminal,
        ResolveOutcome::UnknownOrRevoked => return,
    };
    ctx.set(&durable_wait_index_resolution_key(address), Json(terminal));
}

pub(crate) fn split_cancellable_waits(
    waits: Vec<AwaitEventKey>,
) -> (Vec<AwaitEventKey>, Vec<AwaitEventKey>) {
    waits
        .into_iter()
        .partition(|key| !key.wait.is_turn_control())
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
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let metadata = load_durable_wait_index_metadata(&ctx).await?;
        if metadata.revoked {
            return Ok(Json(RestateDurableWaitRegistration::Revoked));
        }
        if let Some(Json(resolution)) = ctx
            .get::<Json<Resolution>>(&durable_wait_index_resolution_key(&address))
            .await?
        {
            return Ok(Json(RestateDurableWaitRegistration::Resolved(resolution)));
        }
        ctx.set(&durable_wait_index_state_key(&address), Json(request.key));
        Ok(Json(RestateDurableWaitRegistration::Registered))
    }

    async fn settle(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitSettleRequest>,
    ) -> HandlerResult<Json<()>> {
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let _metadata = load_durable_wait_index_metadata(&ctx).await?;
        ctx.set(
            &durable_wait_index_resolution_key(&address),
            Json(request.resolution),
        );
        if address.classification == RestateDurableWaitClassification::DurableWait {
            ctx.clear(&durable_wait_index_state_key(&address));
        }
        Ok(Json(()))
    }

    async fn register_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Json<RestateDurableWaitRegistration>> {
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        if metadata.revoked {
            return Ok(Json(RestateDurableWaitRegistration::Revoked));
        }
        if ctx
            .get::<Json<Resolution>>(&durable_wait_index_resolution_key(&address))
            .await?
            .is_some()
        {
            resolve_durable_wait_awakeable(&ctx, &request);
            return Ok(Json(RestateDurableWaitRegistration::Registered));
        }
        let peek = ctx
            .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key)
            .peek();
        let Json(resolution) = peek.call().await?;
        if resolution.is_some() {
            resolve_durable_wait_awakeable(&ctx, &request);
        } else if !metadata
            .awakeables
            .iter()
            .any(|entry| entry.key == request.key && entry.awakeable_id == request.awakeable_id)
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
        let _address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        metadata
            .awakeables
            .retain(|entry| entry.key != request.key || entry.awakeable_id != request.awakeable_id);
        ctx.set(DURABLE_WAIT_INDEX_METADATA_KEY, Json(metadata));
        Ok(Json(()))
    }

    async fn resolve(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Json<ResolveOutcome>> {
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let mut metadata = load_durable_wait_index_metadata(&ctx).await?;
        if metadata.revoked {
            return Ok(Json(ResolveOutcome::UnknownOrRevoked));
        }
        let resolution_key = durable_wait_index_resolution_key(&address);
        if let Some(Json(terminal)) = ctx.get::<Json<Resolution>>(&resolution_key).await? {
            return Ok(Json(ResolveOutcome::AlreadyResolved { terminal }));
        }
        let resolution = request.resolution.clone();
        let resolve = ctx
            .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key.clone())
            .resolve(Json(request.clone()));
        let Json(outcome) = resolve.call().await?;
        mirror_resolve_outcome(&ctx, &address, resolution, &outcome);
        if outcome == ResolveOutcome::UnknownOrRevoked {
            return Ok(Json(outcome));
        }
        let mut retained = Vec::with_capacity(metadata.awakeables.len());
        for entry in std::mem::take(&mut metadata.awakeables) {
            if entry.key == request.key {
                resolve_durable_wait_awakeable(&ctx, &entry);
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
        for key in &waits {
            ctx.clear(&durable_wait_index_state_key(
                &RestateDurableWaitAddress::for_key(key),
            ));
        }
        resolve_indexed_waits(&ctx, waits, true).await?;
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
        resolve_indexed_waits(&ctx, waits, false).await?;
        Ok(Json(()))
    }
}
