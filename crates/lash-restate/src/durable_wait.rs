#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! Await-event identity and the two durable-wait Restate services.
//!
//! One responsibility: every Lash await-event key is turned into an exact
//! Restate address here, and the two services that own that address live here
//! too — `LashDurableWaitWorkflow` owns the promise and its deadline timer,
//! `LashDurableWaitRegistry` owns the session-to-wait index that cancellation,
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
//! Every externally minted wait request and indexed state value carries the
//! full authority-bound [`AwaitEventKey`] preimage, and handlers derive scope,
//! classification, and workflow address locally.

use lash_sansio::SessionId;
use std::time::Duration;

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome, RuntimeError,
};
use restate_sdk::context::{
    ContextAwakeables, ContextClient, ContextPromises, ContextReadState, ContextWriteState,
    ObjectContext, SharedObjectContext, SharedWorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::Serialize;
use sha2::{Digest, Sha256};

mod root_retirement;

use self::root_retirement::closed_root_cancel_prefix;
use crate::compat::{Call, Reply};
use crate::ingress::RestateAuthorityId;
use crate::object_state::{self, FleetView, ObjectFamily, StoredValueFormats, StoredValueWriter};

pub(crate) const LASH_REPLAY_KEY_HEADER: &str = "x-lash-replay-key";

pub(crate) fn restate_await_event_key(
    scope: &ExecutionScope,
    wait: AwaitEventWaitIdentity,
) -> Result<AwaitEventKey, RuntimeError> {
    let base_key_id = lash_core::facade_support::await_event_identity::derive_key_id(scope, &wait)?;
    Ok(AwaitEventKey {
        scope: scope.clone(),
        wait,
        key_id: base_key_id,
        signature: "restate-handler".to_string(),
    })
}

pub(crate) fn restate_await_event_key_for_authority(
    authority_id: &RestateAuthorityId,
    scope: &ExecutionScope,
    wait: AwaitEventWaitIdentity,
) -> Result<AwaitEventKey, RuntimeError> {
    let base_key_id = lash_core::facade_support::await_event_identity::derive_key_id(scope, &wait)?;
    let mut preimage = Vec::with_capacity(authority_id.binding_id().len() + base_key_id.len() + 1);
    preimage.extend_from_slice(authority_id.binding_id().as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(base_key_id.as_bytes());
    let key_id = format!("{:x}", Sha256::digest(preimage));
    Ok(AwaitEventKey {
        scope: scope.clone(),
        wait,
        key_id,
        signature: authority_id.binding_id().to_string(),
    })
}

pub(crate) fn restate_await_event_key_is_valid(key: &AwaitEventKey) -> bool {
    if key.signature == "restate-handler" {
        let Ok(expected) = restate_await_event_key(&key.scope, key.wait.clone()) else {
            return false;
        };
        return lash_core::facade_support::await_event_identity::constant_time_eq(
            expected.key_id.as_bytes(),
            key.key_id.as_bytes(),
        );
    }
    let Some(authority_id) = RestateAuthorityId::from_binding_id(&key.signature) else {
        return false;
    };
    let Ok(expected) =
        restate_await_event_key_for_authority(&authority_id, &key.scope, key.wait.clone())
    else {
        return false;
    };
    lash_core::facade_support::await_event_identity::constant_time_eq(
        expected.key_id.as_bytes(),
        key.key_id.as_bytes(),
    ) && lash_core::facade_support::await_event_identity::constant_time_eq(
        expected.signature.as_bytes(),
        key.signature.as_bytes(),
    )
}

pub(crate) fn restate_await_event_key_is_valid_for_authority(
    authority_id: &RestateAuthorityId,
    key: &AwaitEventKey,
) -> bool {
    RestateAuthorityId::from_binding_id(&key.signature).as_ref() == Some(authority_id)
        && restate_await_event_key_is_valid(key)
}

pub(crate) fn restate_authority_id_for_key(key: &AwaitEventKey) -> Option<RestateAuthorityId> {
    RestateAuthorityId::from_binding_id(&key.signature)
        .filter(|_| restate_await_event_key_is_valid(key))
}

pub(crate) fn restate_unknown_or_revoked() -> RuntimeError {
    RuntimeError::new(
        lash_core::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
        "await-event key is invalid or revoked",
    )
}
pub(crate) const DURABLE_WAIT_PROMISE_KEY: &str = "resolution";
/// Current wire version of a deadline carried by a durable-wait request.
///
/// Version 1 was the unversioned `timeout_ms` field. Version 2 carries the
/// absolute deadline first journaled by the invoking handler. The request
/// decoder rejects the version-1 field instead of silently granting a fresh
/// relative timeout after a worker replacement.
pub const DURABLE_WAIT_REQUEST_VERSION: u8 = 2;
/// The stored format every value the durable-wait index keeps under its
/// `wait-index/v2/` keys stamps into its object-state envelope (FIG-3814):
/// metadata, wait, resolution, marker, and membership rows alike. It is also
/// the family format of every `LashDurableWaitIndex` object's `_compat`
/// record (ADR 0115 §3.2). Bump it when a stored shape under those keys
/// changes; the previous format reads through the N-1 upcaster slot in
/// [`DURABLE_WAIT_REGISTRY_FORMATS`].
pub const DURABLE_WAIT_REGISTRY_FORMAT_VERSION: u16 = 1;
/// The wait registry's stored-format table: the family's registered surface
/// and descriptor, plus the N-1 upcaster hooks (empty while the first
/// stamped layout is the baseline).
pub(crate) const DURABLE_WAIT_REGISTRY_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "durable-wait registry",
    surface: lash_core::surface_format!(DURABLE_WAIT_REGISTRY_FORMAT_VERSION),
    upcast_n1: &[],
};

/// The object family whose `_compat` record every handler admits first
/// (ADR 0115 §3.2).
pub(crate) const DURABLE_WAIT_REGISTRY_FAMILY: ObjectFamily = ObjectFamily {
    component: lash_core_store::compat::ComponentId::RESTATE_DURABLE_WAIT_REGISTRY,
    formats: &DURABLE_WAIT_REGISTRY_FORMATS,
};
pub(crate) const DURABLE_WAIT_INDEX_METADATA_KEY: &str = "wait-index/v2/metadata";
const DURABLE_WAIT_INDEX_WAIT_PREFIX: &str = "wait-index/v2/wait/";
const DURABLE_WAIT_INDEX_RESOLUTION_PREFIX: &str = "wait-index/v2/resolution/";
/// An effect executing under the scope inside a handler, keyed by replay
/// key: recorded at start, cleared at completion (FIG-2499 quiescence).
const DURABLE_WAIT_INDEX_EFFECT_PREFIX: &str = "wait-index/v2/effect/";
/// An effect group opened under the scope, keyed by group key; cleared once
/// the group's index reports no unsettled child.
const DURABLE_WAIT_INDEX_GROUP_PREFIX: &str = "wait-index/v2/group/";
/// A group child's replay-key-to-group binding, keyed by replay key: the
/// membership a §4 boundary commit resolves its group from (FIG-3409).
const DURABLE_WAIT_INDEX_GROUP_CHILD_PREFIX: &str = "wait-index/v2/group-child/";
const DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX: &str = "wait-index/v2/closure-participant/";

#[cfg(test)]
mod wait_registration_witness {
    use super::*;

    type WaitRegistrationWitness = tokio::sync::oneshot::Sender<RestateDurableWaitRegistration>;

    static WAIT_REGISTRATION_WITNESSES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, WaitRegistrationWitness>>,
    > = std::sync::LazyLock::new(Default::default);

    /// The receiver fires from the index handler, so an unfinished ingress task is
    /// never mistaken for durable registration. The workflow key keeps concurrent
    /// live tests independent.
    pub(crate) fn arm_wait_registration_witness(
        key: &AwaitEventKey,
    ) -> tokio::sync::oneshot::Receiver<RestateDurableWaitRegistration> {
        let address = RestateDurableWaitAddress::for_key(key);
        let (send, receive) = tokio::sync::oneshot::channel();
        WAIT_REGISTRATION_WITNESSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(address.workflow_key, send);
        receive
    }

    pub(super) fn observe_wait_registration(
        key: &AwaitEventKey,
        registration: &RestateDurableWaitRegistration,
    ) {
        let address = RestateDurableWaitAddress::for_key(key);
        if let Some(witness) = WAIT_REGISTRATION_WITNESSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&address.workflow_key)
        {
            let _ = witness.send(registration.clone());
        }
    }
}

#[cfg(test)]
pub(crate) use wait_registration_witness::arm_wait_registration_witness;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RestateDurableWaitAddress {
    pub workflow_key: String,
    pub(crate) scope: RestateDurableWaitScope,
    pub(crate) classification: RestateDurableWaitClassification,
}

impl RestateDurableWaitAddress {
    pub fn for_key(key: &AwaitEventKey) -> Self {
        Self {
            workflow_key: format!("{:x}", Sha256::digest(key.key_id.as_bytes())),
            scope: RestateDurableWaitScope::for_scope(&key.scope),
            classification: if key.wait.is_turn_control() {
                RestateDurableWaitClassification::TurnControl
            } else {
                RestateDurableWaitClassification::DurableWait
            },
        }
    }

    pub fn index_key(&self) -> String {
        self.scope.index_key(&self.workflow_key)
    }
}

/// Which `LashDurableWaitRegistry` object owns a wait: the session's object for
/// a session-bearing scope, or the object of the exact non-session scope
/// (a process or runtime operation, keyed by its journal identity). One
/// object per scope is what lets a scope-exact retirement revoke every wait
/// the scope owns and fence later mints in one keyed handler, exactly as a
/// session's object does for session revocation (FIG-2499).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RestateDurableWaitScope {
    Session(String),
    Scope(String),
}

impl RestateDurableWaitScope {
    /// The index scope of an execution scope. Validated scopes always form a
    /// journal identity; a scope that does not still gets a stable key from
    /// its identity fields so no address can collide with a real scope.
    pub fn for_scope(scope: &ExecutionScope) -> Self {
        match scope.session_id() {
            Some(session_id) => Self::Session(session_id.to_string()),
            None => Self::Scope(
                scope
                    .journal_identity()
                    .map(|identity| identity.key().to_string())
                    .unwrap_or_else(|_| format!("invalid:{}", scope.id())),
            ),
        }
    }

    pub fn index_key(&self, _workflow_key: &str) -> String {
        match self {
            Self::Session(session_id) => session_id.clone(),
            Self::Scope(scope_key) => format!("scope:{scope_key}"),
        }
    }
}

/// The `LashDurableWaitRegistry` object key that owns every wait of `scope`.
pub(crate) fn durable_wait_index_key_for_scope(scope: &ExecutionScope) -> String {
    RestateDurableWaitScope::for_scope(scope).index_key("")
}

mod messages;
pub use messages::*;

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub(crate) struct RestateDurableWaitIndexMetadata {
    revoked: bool,
    #[serde(default)]
    awakeables: Vec<RestateDurableWaitAwakeableRequest>,
    /// Completion keys their owning group child's cancel decision closed, by
    /// the key's authority-free identity (`await_event_identity::derive_key_id`),
    /// so the group index that decides the child can name them (ADR 0099 §4,
    /// W17). Turn fences include their root so CloseRootScope can retire
    /// them; older unscoped ids remain readable. The set is absent from the
    /// encoding while empty.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    cancel_decided: std::collections::BTreeSet<String>,
}

impl RestateDurableWaitIndexMetadata {
    fn is_cancel_decided(
        &self,
        scope: &ExecutionScope,
        wait: &AwaitEventWaitIdentity,
    ) -> Result<bool, TerminalError> {
        if self.cancel_decided.is_empty() {
            return Ok(false);
        }
        let id = cancel_decided_id(scope, wait)?;
        Ok(self.cancel_decided.contains(&id)
            || scope.turn_id().is_some_and(|turn_id| {
                let root = lash_core::store::PhysicalTurn::split_turn_id(turn_id).0;
                self.cancel_decided
                    .contains(&format!("{}{id}", closed_root_cancel_prefix(&root)))
            }))
    }
}

fn cancel_decided_id(
    scope: &ExecutionScope,
    wait: &AwaitEventWaitIdentity,
) -> Result<String, TerminalError> {
    lash_core::facade_support::await_event_identity::derive_key_id(scope, wait)
        .map_err(|error| TerminalError::new(error.to_string()))
}
/// Fire a gate entry because the turn-control wait it guards has settled.
///
/// The wait's own `Resolution` is not forwarded whole: a gate entry only ever
/// guards a turn-control address, so the waiter needs to know that the turn
/// was asked to stop and in which mode, and nothing more.
fn resolve_durable_wait_awakeable(
    ctx: &ObjectContext<'_>,
    request: &RestateDurableWaitAwakeableRequest,
    resolution: &Resolution,
) {
    ctx.resolve_awakeable(
        &request.awakeable_id,
        Json(RestateTurnCancelWake::for_gate_resolution(resolution)),
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
    let deadline = deadline.map(|deadline| {
        let remaining_ms =
            u64::try_from(deadline.saturating_duration_since(clock.now()).as_millis())
                .unwrap_or(u64::MAX);
        RestateDurableWaitDeadline {
            version: DURABLE_WAIT_REQUEST_VERSION,
            unix_epoch_ms: clock.timestamp_ms().saturating_add(remaining_ms),
        }
    });
    RestateDurableWaitAwaitRequest {
        key: key.clone(),
        deadline,
    }
}
/// How one wait raced against durable turn cancellation ended.
///
/// Every wait that can be cut short by turn cancellation — a timer, an
/// await-event, a process terminal wait — reports through this one type, so a
/// caller reads the same answers whatever it was waiting on. `T` is
/// whatever the wait produces when it wins its own race.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum RestateTurnCancelRaceOutcome<T> {
    /// The wait completed on its own terms.
    Completed(T),
    /// The turn was cancelled while the wait was parked.
    TurnCancelled,
    /// The session was revoked, so the turn has no ground left to stand on.
    SessionRevoked { session_id: SessionId },
    /// A process drive's wait that observes no turn lost to the process
    /// segment's own durable cancel promise (FIG-3673).
    ProcessCancelled,
}

impl<T> RestateTurnCancelRaceOutcome<T> {
    /// The same answer over what the wait produced when it won.
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> RestateTurnCancelRaceOutcome<U> {
        match self {
            Self::Completed(value) => RestateTurnCancelRaceOutcome::Completed(map(value)),
            Self::TurnCancelled => RestateTurnCancelRaceOutcome::TurnCancelled,
            Self::SessionRevoked { session_id } => {
                RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
            }
            Self::ProcessCancelled => RestateTurnCancelRaceOutcome::ProcessCancelled,
        }
    }
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

/// `awakeable_id` is created by the caller, never here: the awakeable and the
/// wait it guards are journaled commands whose relative order is part of the
/// deployed journal shape (FIG-790), so only the call site may decide when each
/// is emitted. This helper owns the one step that is identical everywhere —
/// the `register_awakeable` call and its revocation verdict.
pub(crate) async fn register_turn_cancel_gate<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
    key: AwaitEventKey,
    awakeable_id: String,
) -> Result<RestateTurnCancelGate, TerminalError>
where
    C: ContextClient<'ctx>,
{
    let entry = RestateDurableWaitAwakeableRequest { key, awakeable_id };
    let replay_key = entry.key.key_id.clone();
    let register = namespace
        .durable_wait_registry(context, session_id)
        .register_awakeable(entry.clone())
        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
    let registration = register.call().await?.into_body();
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
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
    entry: RestateDurableWaitAwakeableRequest,
) -> Result<(), TerminalError>
where
    C: ContextClient<'ctx>,
{
    let replay_key = entry.key.key_id.clone();
    let unregister = namespace
        .durable_wait_registry(context, session_id)
        .unregister_awakeable(entry)
        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
    unregister.call().await?;
    Ok(())
}

/// One durable Restate workflow per Lash await-event identity.
///
/// The workflow key is a stable digest of the full Lash [`AwaitEventKey`], so all
/// execution-scope variants share the same exact-address resolution path.
#[restate_sdk::workflow]
pub trait LashDurableWaitWorkflow {
    #[shared]
    async fn await_resolution(
        call: Call<RestateDurableWaitAwaitInput>,
    ) -> HandlerResult<Reply<Resolution>>;

    #[shared]
    async fn peek(call: Call<()>) -> HandlerResult<Reply<Option<Resolution>>>;

    #[shared]
    async fn resolve(
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<ResolveOutcome>>;
}

/// [`LashDurableWaitWorkflow`] in one deployment's namespace (FIG-3898).
#[derive(Clone, Debug, Default)]
pub(crate) struct LashDurableWaitWorkflowImpl {
    namespace: crate::RestateNamespace,
}

impl LashDurableWaitWorkflowImpl {
    pub(crate) fn new(namespace: crate::RestateNamespace) -> Self {
        Self { namespace }
    }
}

impl LashDurableWaitWorkflow for LashDurableWaitWorkflowImpl {
    async fn await_resolution(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<RestateDurableWaitAwaitInput>,
    ) -> HandlerResult<Reply<Resolution>> {
        let (wire, input) = call.open()?;
        let request = match input {
            RestateDurableWaitAwaitInput::Current(request) => request,
            RestateDurableWaitAwaitInput::Predecessor { .. } => {
                return Err(
                    incompatible_durable_wait_request("predecessor field `timeout_ms`").into(),
                );
            }
        };
        if let Some(deadline) = request.deadline {
            deadline.validate()?;
        }
        let address = verify_durable_wait_workflow_key(ctx.key(), &request.key)?;
        let index_key = durable_wait_index_object_key(&address);
        let replay_key = request.key.key_id.clone();
        let registration = self
            .namespace
            .durable_wait_registry(&ctx, index_key.clone())
            .register(RestateDurableWaitIndexRequest {
                key: request.key.clone(),
            })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key.clone());
        let registration = registration.call().await?.into_body();
        match registration {
            RestateDurableWaitRegistration::Resolved(resolution) => {
                return Ok(Reply::at(wire, resolution));
            }
            RestateDurableWaitRegistration::Revoked => {
                return Ok(Reply::at(wire, Resolution::Cancelled));
            }
            RestateDurableWaitRegistration::Registered => {}
        }

        let resolution =
            if let Some(payload) = ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
                serde_json::from_str(&payload).map_err(TerminalError::from_error)?
            } else if let Some(deadline) = request.deadline {
                let promise = ctx.promise::<String>(DURABLE_WAIT_PROMISE_KEY);
                let remaining = deadline.remaining(crate::system_clock().timestamp_ms())?;
                // The workflow input is the stable absolute deadline. Restate's
                // SleepCommand deliberately excludes its calculated wake time from
                // replay comparison, so deriving only the remaining delay here
                // preserves the original budget without another compared payload.
                let timer = restate_sdk::context::ContextTimers::sleep(&ctx, remaining);
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

        // A workflow that wakes after a deployment upgrade replays the old
        // registration command, so this settle call is the first new command
        // a previously parked invocation can execute. Fully parked v2
        // invocations never reach it; pre-stamp object state is instead
        // refused typed at the registry's stamped-state gate.
        let settle = self
            .namespace
            .durable_wait_registry(&ctx, index_key)
            .settle(RestateDurableWaitSettleRequest {
                key: request.key,
                resolution: resolution.clone(),
            })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
        settle.call().await?;
        Ok(Reply::at(wire, resolution))
    }

    async fn peek(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<Option<Resolution>>> {
        let (wire, ()) = call.open()?;
        let resolution = match ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
            Some(payload) => {
                Some(serde_json::from_str(&payload).map_err(TerminalError::from_error)?)
            }
            None => None,
        };
        Ok(Reply::at(wire, resolution))
    }

    async fn resolve(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<ResolveOutcome>> {
        let (wire, request) = call.open()?;
        let _address = verify_durable_wait_workflow_key(ctx.key(), &request.key)?;
        if let Some(payload) = ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
            let terminal = serde_json::from_str(&payload).map_err(TerminalError::from_error)?;
            return Ok(Reply::at(
                wire,
                ResolveOutcome::AlreadyResolved { terminal },
            ));
        }
        let payload =
            serde_json::to_string(&request.resolution).map_err(TerminalError::from_error)?;
        ctx.resolve_promise(DURABLE_WAIT_PROMISE_KEY, payload);
        Ok(Reply::at(wire, ResolveOutcome::Accepted))
    }
}
/// Durable session-to-wait index used by cancellation and session deletion.
///
/// Object serialization makes registration, cancellation, and revocation atomic for one
/// session.
// The registered service keeps its `LashDurableWaitIndex` name (FIG-3814):
// the trait flavor of the object macro takes the name from `#[name]`, not
// from the macro's own arguments.
#[restate_sdk::object]
#[name = "LashDurableWaitIndex"]
pub trait LashDurableWaitRegistry {
    async fn is_revoked(call: Call<()>) -> HandlerResult<Reply<bool>>;
    /// A turn cancellation gate's peek: the session's revocation and the
    /// gate's terminal in one read that never queues on the exclusive
    /// handlers (FIG-3978). Every write to a gate goes through this object's
    /// `resolve`, and every workflow waiter reaches its terminal only after
    /// `settle` mirrored it here, so the index's copy is the gate's answer.
    #[shared]
    async fn peek_turn_gate(
        call: Call<RestateDurableWaitIndexRequest>,
    ) -> HandlerResult<Reply<RestateTurnGatePeek>>;
    /// Read registered waits that have no retained terminal.
    async fn outstanding(call: Call<()>) -> HandlerResult<Reply<Vec<AwaitEventKey>>>;
    async fn register(
        call: Call<RestateDurableWaitIndexRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitRegistration>>;
    async fn settle(call: Call<RestateDurableWaitSettleRequest>) -> HandlerResult<Reply<()>>;
    async fn retire_root(call: Call<RestateDurableWaitRootRequest>) -> HandlerResult<Reply<()>>;
    async fn register_awakeable(
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitRegistration>>;
    async fn unregister_awakeable(
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<()>>;
    async fn resolve(
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitResolveResponse>>;
    /// Close a cancel-decided group child's completion key: every resolve of
    /// it from now on is refused, typed, and writes nothing (ADR 0099 §4,
    /// W17). A waiter that already holds a terminal keeps it; the key is
    /// named by its authority-free identity, because the group index that
    /// decides the child does not hold the minting authority.
    async fn fence_cancel_decided(
        call: Call<RestateDurableWaitCancelDecidedRequest>,
    ) -> HandlerResult<Reply<()>>;
    /// Wake any current waiter and retain this resolution for every later
    /// registration, even when the wait workflow had an earlier notification.
    async fn retain_resolution(
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<()>>;
    async fn cancel_all(call: Call<()>) -> HandlerResult<Reply<()>>;
    async fn revoke_all(call: Call<()>) -> HandlerResult<Reply<()>>;
    /// [`revoke_all`](Self::revoke_all) only when no durable wait or awakeable
    /// under this index is still unresolved, answering whether it revoked.
    /// Object serialization makes the proof and the revocation one step.
    async fn revoke_all_if_quiescent(call: Call<()>) -> HandlerResult<Reply<bool>>;
    /// Retire a physical scope under either retirement gate while still
    /// refusing a registered cancellation-closure catalog participant.
    async fn retire_scope(call: Call<()>) -> HandlerResult<Reply<bool>>;
    /// Lift a revocation because the scope's owner is registered again: a
    /// pruned process id the host reuses (ADR 0049). State stays cleared; only
    /// the fence goes.
    async fn reinstate(call: Call<()>) -> HandlerResult<Reply<()>>;
    /// Record an effect starting under this scope inside a handler, answering
    /// whether the scope admits it (`false` once revoked). While recorded,
    /// the scope is not quiescent (FIG-2499).
    async fn begin_effect(
        call: Call<RestateDurableWaitEffectRequest>,
    ) -> HandlerResult<Reply<bool>>;
    async fn end_effect(call: Call<RestateDurableWaitEffectRequest>) -> HandlerResult<Reply<()>>;
    /// Record an effect group opened under this scope, answering whether the
    /// scope admits it (`false` once revoked). The scope is not quiescent
    /// while the group's index still reports an unsettled child.
    async fn record_group(call: Call<RestateDurableWaitGroupRequest>)
    -> HandlerResult<Reply<bool>>;
    /// Record one group child's durable membership under this scope,
    /// answering whether the scope admits it (`false` once revoked).
    async fn record_group_child(
        call: Call<RestateDurableWaitGroupChildRequest>,
    ) -> HandlerResult<Reply<bool>>;
    /// The group `replay_key` is a committed member of under this scope, or
    /// `None` when no dispatch admitted one — the §4 boundary's answer to
    /// "which group's index owns this child's final" (FIG-3409).
    async fn group_child_membership(
        call: Call<RestateDurableWaitGroupChildMembershipRequest>,
    ) -> HandlerResult<Reply<Option<String>>>;
    async fn register_closure_participant(
        call: Call<RestateTurnCancelClosureParticipantRequest>,
    ) -> HandlerResult<Reply<bool>>;
    async fn release_closure_participant(
        call: Call<RestateTurnCancelClosureParticipantRequest>,
    ) -> HandlerResult<Reply<()>>;
}

/// [`LashDurableWaitRegistry`] in one deployment's namespace (FIG-3898).
#[derive(Clone, Debug, Default)]
pub(crate) struct LashDurableWaitRegistryImpl {
    namespace: crate::RestateNamespace,
    /// Where the handlers read the fleet epoch their writes are stamped at.
    fleet: FleetView,
}

impl LashDurableWaitRegistryImpl {
    pub(crate) fn new(namespace: crate::RestateNamespace, fleet: FleetView) -> Self {
        Self { namespace, fleet }
    }

    /// The registry's `_compat` gate for a handler that may write.
    async fn admit(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<object_state::AdmittedObject, TerminalError> {
        object_state::admit_exclusive(
            ctx,
            &DURABLE_WAIT_REGISTRY_FAMILY,
            self.fleet.fleet_format(),
        )
        .await
    }
}

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

/// Load the index's metadata, initializing it for a pristine object.
///
/// Restate object state is not part of an invocation's replayed journal: these
/// index handlers are short-lived single calls, so changing their command
/// sequence does not alter an in-flight multi-call journal. Object state does,
/// however, survive a deployment upgrade, so every handler has already
/// admitted the object through its `_compat` record before this reads it.
async fn load_durable_wait_index_metadata(
    ctx: &ObjectContext<'_>,
    writer: StoredValueWriter,
) -> Result<RestateDurableWaitIndexMetadata, TerminalError> {
    if let Some(metadata) = object_state::get_stamped(
        ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?
    {
        return Ok(metadata);
    }

    let metadata = RestateDurableWaitIndexMetadata::default();
    object_state::set_stamped(
        ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        writer,
        metadata.clone(),
    );
    Ok(metadata)
}

/// The index's metadata when the object holds any value, `None` when the
/// object is pristine: it holds nothing but its `_compat` record. Values
/// without a metadata row read as a default one, the same answer the epoch
/// era gave a marked object whose metadata had not yet been written.
async fn read_durable_wait_index_metadata(
    ctx: &ObjectContext<'_>,
) -> Result<Option<RestateDurableWaitIndexMetadata>, TerminalError> {
    if let Some(metadata) = object_state::get_stamped(
        ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        &DURABLE_WAIT_REGISTRY_FORMATS,
    )
    .await?
    {
        return Ok(Some(metadata));
    }
    if !ctx
        .get_keys()
        .await?
        .iter()
        .any(|key| object_state::is_value_key(key))
    {
        return Ok(None);
    }
    Ok(Some(RestateDurableWaitIndexMetadata::default()))
}

async fn load_indexed_waits(ctx: &ObjectContext<'_>) -> Result<Vec<AwaitEventKey>, TerminalError> {
    let mut waits = Vec::new();
    for state_key in ctx
        .get_keys()
        .await?
        .into_iter()
        .filter(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_WAIT_PREFIX))
    {
        let key: AwaitEventKey =
            object_state::get_stamped(ctx, &state_key, &DURABLE_WAIT_REGISTRY_FORMATS)
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

async fn read_outstanding_waits(
    ctx: &ObjectContext<'_>,
) -> Result<Vec<AwaitEventKey>, TerminalError> {
    let Some(metadata) = read_durable_wait_index_metadata(ctx).await? else {
        return Ok(Vec::new());
    };
    if metadata.revoked {
        return Ok(Vec::new());
    }

    let mut outstanding = Vec::new();
    for key in load_indexed_waits(ctx).await? {
        let address = RestateDurableWaitAddress::for_key(&key);
        if object_state::get_stamped::<Resolution>(
            ctx,
            &durable_wait_index_resolution_key(&address),
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        .is_none()
            && !metadata.is_cancel_decided(&key.scope, &key.wait)?
        {
            outstanding.push(key);
        }
    }
    outstanding.sort_unstable_by(|left, right| left.key_id.cmp(&right.key_id));
    Ok(outstanding)
}

async fn resolve_indexed_waits(
    ctx: &ObjectContext<'_>,
    writer: StoredValueWriter,
    namespace: &crate::RestateNamespace,
    waits: Vec<AwaitEventKey>,
    mirror_outcomes: bool,
) -> HandlerResult<()> {
    for key in waits {
        let replay_key = key.key_id.clone();
        let address = RestateDurableWaitAddress::for_key(&key);
        let workflow_key = address.workflow_key.clone();
        let resolution = Resolution::Cancelled;
        let resolve = namespace
            .durable_wait_workflow(ctx, workflow_key)
            .resolve(RestateDurableWaitResolveRequest {
                key: key.clone(),
                resolution: resolution.clone(),
            })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
        let outcome = resolve.call().await?.into_body();
        if mirror_outcomes {
            mirror_resolve_outcome(ctx, writer, &key, &address, resolution, &outcome);
        }
    }
    Ok(())
}

fn mirror_resolve_outcome(
    ctx: &ObjectContext<'_>,
    writer: StoredValueWriter,
    key: &AwaitEventKey,
    address: &RestateDurableWaitAddress,
    accepted_terminal: Resolution,
    outcome: &ResolveOutcome,
) {
    let terminal = match outcome {
        ResolveOutcome::AlreadyResolved { terminal } => terminal.clone(),
        ResolveOutcome::Accepted => accepted_terminal,
        ResolveOutcome::UnknownOrRevoked => return,
    };
    retain_turn_wait_preimage(ctx, writer, key, address);
    object_state::set_stamped(
        ctx,
        &durable_wait_index_resolution_key(address),
        writer,
        terminal,
    );
}

fn retain_turn_wait_preimage(
    ctx: &ObjectContext<'_>,
    writer: StoredValueWriter,
    key: &AwaitEventKey,
    address: &RestateDurableWaitAddress,
) {
    if matches!(key.scope, ExecutionScope::Turn { .. }) {
        object_state::set_stamped(
            ctx,
            &durable_wait_index_state_key(address),
            writer,
            key.clone(),
        );
    }
}

/// Revoke the index: fence it, revoke its awakeables, and cancel its waits.
/// With `only_if_quiescent`, an unresolved wait or a live awakeable leaves
/// the index untouched and answers `false`.
async fn revoke_index(
    ctx: &ObjectContext<'_>,
    object: object_state::AdmittedObject,
    namespace: &crate::RestateNamespace,
    only_if_quiescent: bool,
) -> HandlerResult<bool> {
    let mut metadata = load_durable_wait_index_metadata(ctx, object.writer).await?;
    let waits = load_indexed_waits(ctx).await?;
    let keys = ctx.get_keys().await?;
    if keys
        .iter()
        .any(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX))
        || (only_if_quiescent
            && (!waits.is_empty()
                || !metadata.awakeables.is_empty()
                || !scope_effects_and_groups_are_quiescent(ctx, namespace).await?))
    {
        return Ok(false);
    }
    let awakeables = std::mem::take(&mut metadata.awakeables);
    metadata.revoked = true;
    // A revoked index keeps its `_compat` record: it fences a stale handler
    // from recreating the state the revocation cleared.
    object.clear_all(ctx);
    object_state::set_stamped(
        ctx,
        DURABLE_WAIT_INDEX_METADATA_KEY,
        object.writer,
        metadata,
    );
    for entry in awakeables {
        revoke_durable_wait_awakeable(ctx, &entry);
    }
    resolve_indexed_waits(ctx, object.writer, namespace, waits, false).await?;
    Ok(true)
}

/// Whether nothing recorded by `begin_effect` or `record_group` is still
/// live: no executing effect, and every recorded group's index reports no
/// unsettled child. A group found settled is forgotten here, so a caller
/// that never closed it does not fence its scope forever.
async fn scope_effects_and_groups_are_quiescent(
    ctx: &ObjectContext<'_>,
    namespace: &crate::RestateNamespace,
) -> Result<bool, TerminalError> {
    let keys = ctx.get_keys().await?;
    if keys
        .iter()
        .any(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_EFFECT_PREFIX))
    {
        return Ok(false);
    }
    let mut live = false;
    for (state_key, group_key) in keys.iter().filter_map(|state_key| {
        state_key
            .strip_prefix(DURABLE_WAIT_INDEX_GROUP_PREFIX)
            .map(|group_key| (state_key, group_key))
    }) {
        let unsettled = namespace
            .effect_group_state(ctx, group_key.to_string())
            .unsettled_children()
            .call()
            .await?
            .into_body();
        if unsettled > 0 {
            live = true;
        } else {
            ctx.clear(state_key);
        }
    }
    Ok(!live)
}

fn durable_wait_index_effect_key(replay_key: &str) -> String {
    format!("{DURABLE_WAIT_INDEX_EFFECT_PREFIX}{replay_key}")
}

fn durable_wait_index_group_key(group_key: &str) -> String {
    format!("{DURABLE_WAIT_INDEX_GROUP_PREFIX}{group_key}")
}

fn durable_wait_index_group_child_key(replay_key: &str) -> String {
    format!("{DURABLE_WAIT_INDEX_GROUP_CHILD_PREFIX}{replay_key}")
}

fn durable_wait_index_closure_participant_key(participant_id: &str) -> String {
    let digest = Sha256::digest(participant_id.as_bytes());
    format!("{DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX}{digest:x}")
}

pub(crate) fn split_cancellable_waits(
    waits: Vec<AwaitEventKey>,
) -> (Vec<AwaitEventKey>, Vec<AwaitEventKey>) {
    waits
        .into_iter()
        .partition(|key| !key.wait.is_turn_control())
}
impl LashDurableWaitRegistry for LashDurableWaitRegistryImpl {
    async fn retire_root(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitRootRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        root_retirement::retire_root(ctx, object, &self.namespace, request)
            .await
            .map(|()| Reply::at(wire, ()))
    }

    async fn is_revoked(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, ()) = call.open()?;
        object_state::admit_exclusive_read(&ctx, &DURABLE_WAIT_REGISTRY_FAMILY).await?;
        Ok(Reply::at(
            wire,
            read_durable_wait_index_metadata(&ctx)
                .await?
                .is_some_and(|metadata| metadata.revoked),
        ))
    }

    async fn peek_turn_gate(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<RestateDurableWaitIndexRequest>,
    ) -> HandlerResult<Reply<RestateTurnGatePeek>> {
        let (wire, request) = call.open()?;
        object_state::admit_shared(&ctx, &DURABLE_WAIT_REGISTRY_FAMILY).await?;
        if !matches!(
            request.key.wait,
            AwaitEventWaitIdentity::TurnCancelGate | AwaitEventWaitIdentity::TurnCancelEscalation
        ) {
            return Err(TerminalError::new(format!(
                "peek_turn_gate reads a turn cancellation gate, not a {:?} wait",
                request.key.wait
            ))
            .into());
        }
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let metadata = object_state::get_stamped_shared::<RestateDurableWaitIndexMetadata>(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?;
        if metadata.is_some_and(|metadata| metadata.revoked) {
            return Ok(Reply::at(wire, RestateTurnGatePeek::Revoked));
        }
        let resolution_key = durable_wait_index_resolution_key(&address);
        Ok(Reply::at(
            wire,
            RestateTurnGatePeek::Open(
                object_state::get_stamped_shared(
                    &ctx,
                    &resolution_key,
                    &DURABLE_WAIT_REGISTRY_FORMATS,
                )
                .await?,
            ),
        ))
    }

    async fn outstanding(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<Vec<AwaitEventKey>>> {
        let (wire, ()) = call.open()?;
        object_state::admit_exclusive_read(&ctx, &DURABLE_WAIT_REGISTRY_FAMILY).await?;
        Ok(Reply::at(wire, read_outstanding_waits(&ctx).await?))
    }

    async fn register(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitIndexRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitRegistration>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        let registration = if metadata.revoked {
            RestateDurableWaitRegistration::Revoked
        } else if let Some(resolution) = object_state::get_stamped::<Resolution>(
            &ctx,
            &durable_wait_index_resolution_key(&address),
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        {
            RestateDurableWaitRegistration::Resolved(resolution)
        } else {
            object_state::set_stamped(
                &ctx,
                &durable_wait_index_state_key(&address),
                object.writer,
                request.key.clone(),
            );
            RestateDurableWaitRegistration::Registered
        };
        #[cfg(test)]
        wait_registration_witness::observe_wait_registration(&request.key, &registration);
        Ok(Reply::at(wire, registration))
    }

    async fn settle(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitSettleRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let _metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if matches!(request.key.wait, AwaitEventWaitIdentity::TurnTerminal) {
            // A late attach may register after CloseRootScope retired the root.
            // Its workflow promise already owns the terminal; settling the
            // attach must not restore a session-lifetime index row.
            ctx.clear(&durable_wait_index_state_key(&address));
            ctx.clear(&durable_wait_index_resolution_key(&address));
            return Ok(Reply::at(wire, ()));
        }
        retain_turn_wait_preimage(&ctx, object.writer, &request.key, &address);
        object_state::set_stamped(
            &ctx,
            &durable_wait_index_resolution_key(&address),
            object.writer,
            request.resolution,
        );
        if !matches!(request.key.scope, ExecutionScope::Turn { .. }) {
            ctx.clear(&durable_wait_index_state_key(&address));
        }
        Ok(Reply::at(wire, ()))
    }

    async fn retain_resolution(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let _metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        let workflow_key = address.workflow_key.clone();
        let replay_key = request.key.key_id.clone();
        self.namespace
            .durable_wait_workflow(&ctx, workflow_key)
            .resolve(request.clone())
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
            .call()
            .await?;
        // Retirement is a fence, not another first-write notification. Keep
        // it in the index even when the workflow promise already held READY,
        // RANK, CANCEL, or ADMIT so a later registration cannot park or revive
        // the pre-retirement terminal.
        retain_turn_wait_preimage(&ctx, object.writer, &request.key, &address);
        object_state::set_stamped(
            &ctx,
            &durable_wait_index_resolution_key(&address),
            object.writer,
            request.resolution,
        );
        if !matches!(request.key.scope, ExecutionScope::Turn { .. }) {
            ctx.clear(&durable_wait_index_state_key(&address));
        }
        Ok(Reply::at(wire, ()))
    }

    async fn register_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitRegistration>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            return Ok(Reply::at(wire, RestateDurableWaitRegistration::Revoked));
        }
        if let Some(resolution) = object_state::get_stamped::<Resolution>(
            &ctx,
            &durable_wait_index_resolution_key(&address),
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        {
            resolve_durable_wait_awakeable(&ctx, &request, &resolution);
            return Ok(Reply::at(wire, RestateDurableWaitRegistration::Registered));
        }
        let peek = self
            .namespace
            .durable_wait_workflow(&ctx, address.workflow_key)
            .peek()
            .header(
                LASH_REPLAY_KEY_HEADER.to_string(),
                request.key.key_id.clone(),
            );
        let resolution = peek.call().await?.into_body();
        if let Some(resolution) = resolution {
            resolve_durable_wait_awakeable(&ctx, &request, &resolution);
        } else if !metadata
            .awakeables
            .iter()
            .any(|entry| entry.key == request.key && entry.awakeable_id == request.awakeable_id)
        {
            metadata.awakeables.push(request);
            object_state::set_stamped(
                &ctx,
                DURABLE_WAIT_INDEX_METADATA_KEY,
                object.writer,
                metadata,
            );
        }
        Ok(Reply::at(wire, RestateDurableWaitRegistration::Registered))
    }

    async fn unregister_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let _address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        metadata
            .awakeables
            .retain(|entry| entry.key != request.key || entry.awakeable_id != request.awakeable_id);
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
        Ok(Reply::at(wire, ()))
    }

    async fn resolve(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitResolveResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let address = derive_durable_wait_index_address(ctx.key(), &request.key)?;
        let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            return Ok(Reply::at(
                wire,
                RestateDurableWaitResolveResponse::Outcome(ResolveOutcome::UnknownOrRevoked),
            ));
        }
        // §4, W17: the owning group child's cancel decision closed this key.
        // Checked before any retained terminal, because the close's own
        // release of the child's wait may have retained one since.
        if metadata.is_cancel_decided(&request.key.scope, &request.key.wait)? {
            return Ok(Reply::at(
                wire,
                RestateDurableWaitResolveResponse::Refused(
                    RestateDurableWaitResolveRefusal::CancelDecided,
                ),
            ));
        }
        let resolution_key = durable_wait_index_resolution_key(&address);
        if let Some(terminal) = object_state::get_stamped::<Resolution>(
            &ctx,
            &resolution_key,
            &DURABLE_WAIT_REGISTRY_FORMATS,
        )
        .await?
        {
            return Ok(Reply::at(
                wire,
                RestateDurableWaitResolveResponse::Outcome(ResolveOutcome::AlreadyResolved {
                    terminal,
                }),
            ));
        }
        let resolution = request.resolution.clone();
        let replay_key = request.key.key_id.clone();
        let resolve = self
            .namespace
            .durable_wait_workflow(&ctx, address.workflow_key.clone())
            .resolve(request.clone())
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
        let outcome = resolve.call().await?.into_body();
        // Wake with the terminal the gate actually holds: a request that lost
        // to an earlier writer must not report its own mode to the waiter.
        let settled = match &outcome {
            ResolveOutcome::AlreadyResolved { terminal } => terminal.clone(),
            ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked => resolution.clone(),
        };
        // A turn's terminal is published one-way after its commit, so it can
        // land after CloseRootScope retired the root. Its workflow promise
        // owns the terminal, as in `settle`; mirroring it would restore a
        // session-lifetime row (FIG-3978).
        if !matches!(request.key.wait, AwaitEventWaitIdentity::TurnTerminal) {
            mirror_resolve_outcome(
                &ctx,
                object.writer,
                &request.key,
                &address,
                resolution,
                &outcome,
            );
        }
        if outcome == ResolveOutcome::UnknownOrRevoked {
            return Ok(Reply::at(
                wire,
                RestateDurableWaitResolveResponse::Outcome(outcome),
            ));
        }
        let mut retained = Vec::with_capacity(metadata.awakeables.len());
        for entry in std::mem::take(&mut metadata.awakeables) {
            if entry.key == request.key {
                resolve_durable_wait_awakeable(&ctx, &entry, &settled);
            } else {
                retained.push(entry);
            }
        }
        metadata.awakeables = retained;
        object_state::set_stamped(
            &ctx,
            DURABLE_WAIT_INDEX_METADATA_KEY,
            object.writer,
            metadata,
        );
        Ok(Reply::at(
            wire,
            RestateDurableWaitResolveResponse::Outcome(outcome),
        ))
    }

    async fn fence_cancel_decided(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitCancelDecidedRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let expected = durable_wait_index_key_for_scope(&request.scope);
        if expected != ctx.key() {
            return Err(TerminalError::new(format!(
                "cancel-decided completion fence for scope {expected} addressed index {}",
                ctx.key()
            ))
            .into());
        }
        let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        let id = cancel_decided_id(&request.scope, &request.wait)?;
        let id = if let Some(turn_id) = request.scope.turn_id() {
            let root = lash_core::store::PhysicalTurn::split_turn_id(turn_id).0;
            format!("{}{id}", closed_root_cancel_prefix(&root))
        } else {
            id
        };
        if !metadata.revoked && metadata.cancel_decided.insert(id) {
            object_state::set_stamped(
                &ctx,
                DURABLE_WAIT_INDEX_METADATA_KEY,
                object.writer,
                metadata,
            );
        }
        Ok(Reply::at(wire, ()))
    }

    async fn cancel_all(&self, ctx: ObjectContext<'_>, call: Call<()>) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        let _metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        let (waits, _controls) = split_cancellable_waits(load_indexed_waits(&ctx).await?);
        for key in &waits {
            if !matches!(key.scope, ExecutionScope::Turn { .. }) {
                ctx.clear(&durable_wait_index_state_key(
                    &RestateDurableWaitAddress::for_key(key),
                ));
            }
        }
        resolve_indexed_waits(&ctx, object.writer, &self.namespace, waits, true).await?;
        Ok(Reply::at(wire, ()))
    }

    async fn revoke_all(&self, ctx: ObjectContext<'_>, call: Call<()>) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        revoke_index(&ctx, object, &self.namespace, false).await?;
        Ok(Reply::at(wire, ()))
    }

    async fn revoke_all_if_quiescent(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        revoke_index(&ctx, object, &self.namespace, true)
            .await
            .map(|revoked| Reply::at(wire, revoked))
    }

    async fn retire_scope(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        revoke_index(&ctx, object, &self.namespace, false)
            .await
            .map(|revoked| Reply::at(wire, revoked))
    }

    async fn reinstate(&self, ctx: ObjectContext<'_>, call: Call<()>) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            metadata.revoked = false;
            object_state::set_stamped(
                &ctx,
                DURABLE_WAIT_INDEX_METADATA_KEY,
                object.writer,
                metadata,
            );
        }
        Ok(Reply::at(wire, ()))
    }

    async fn begin_effect(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitEffectRequest>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            return Ok(Reply::at(wire, false));
        }
        object_state::set_stamped(
            &ctx,
            &durable_wait_index_effect_key(&request.replay_key),
            object.writer,
            true,
        );
        Ok(Reply::at(wire, true))
    }

    async fn end_effect(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitEffectRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let _metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        ctx.clear(&durable_wait_index_effect_key(&request.replay_key));
        Ok(Reply::at(wire, ()))
    }

    async fn record_group(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitGroupRequest>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            return Ok(Reply::at(wire, false));
        }
        object_state::set_stamped(
            &ctx,
            &durable_wait_index_group_key(&request.group_key),
            object.writer,
            true,
        );
        Ok(Reply::at(wire, true))
    }

    async fn record_group_child(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitGroupChildRequest>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            return Ok(Reply::at(wire, false));
        }
        object_state::set_stamped(
            &ctx,
            &durable_wait_index_group_child_key(&request.replay_key),
            object.writer,
            request.group_key,
        );
        Ok(Reply::at(wire, true))
    }

    async fn group_child_membership(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitGroupChildMembershipRequest>,
    ) -> HandlerResult<Reply<Option<String>>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let _metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        Ok(Reply::at(
            wire,
            object_state::get_stamped::<String>(
                &ctx,
                &durable_wait_index_group_child_key(&request.replay_key),
                &DURABLE_WAIT_REGISTRY_FORMATS,
            )
            .await?,
        ))
    }

    async fn register_closure_participant(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateTurnCancelClosureParticipantRequest>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        if metadata.revoked {
            return Ok(Reply::at(wire, false));
        }
        object_state::set_stamped(
            &ctx,
            &durable_wait_index_closure_participant_key(&request.participant_id),
            object.writer,
            request.participant_id,
        );
        Ok(Reply::at(wire, true))
    }

    async fn release_closure_participant(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateTurnCancelClosureParticipantRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let _metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        ctx.clear(&durable_wait_index_closure_participant_key(
            &request.participant_id,
        ));
        Ok(Reply::at(wire, ()))
    }
}
