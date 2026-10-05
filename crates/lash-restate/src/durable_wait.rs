#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! Await-event identity and the two durable-wait Restate services.
//!
//! One responsibility: every Lash await-event key is turned into an exact
//! Restate address here, and the two services that own that address live here
//! too — `LashDurableWaitWorkflow` owns the promise,
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
use std::sync::Arc;

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

mod notifications;
mod observer;
use notifications::{
    mirror_resolve_outcome, resolve_durable_wait_awakeable, revoke_durable_wait_awakeable,
    wake_ended_waits,
};
pub(crate) mod process_terminal;
pub use process_terminal::{
    ProcessTerminalDelivery, ProcessTerminalSubscription, RestateProcessTerminalRequest,
};
mod run_retirement;
mod scope_retirement;
pub(crate) mod source_seal;
mod subscriptions;

use self::scope_retirement::revoke_index;

pub(crate) use self::observer::{WaitObserver, observe_durable_wait};
pub use self::source_seal::{
    RestateSourceArmReply, RestateSourceArmRequest, RestateSourceSealReply,
    RestateSourceSealRequest, RestateSourceSealWrite, RestateSourceSubscribeReply,
    RestateSourceSubscribeRequest,
};

use crate::compat::{Call, Reply};
use crate::ingress::RestateAuthorityId;
use crate::object_state::{
    self, FleetView, ObjectFamily, ObjectUpgradeResponse, StoredValueFormats, StoredValueWriter,
};

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
/// The stored format every value the durable-wait index keeps under its
/// `wait-index/v2/` keys stamps into its object-state envelope (FIG-3814):
/// metadata, indexed waits, execution markers and source seals alike. It is also
/// the family format of every `LashDurableWaitIndex` object's `_compat`
/// record (ADR 0115 §3.2). Bump it when a stored shape under those keys
/// changes, and register the previous format's lift in
/// `lash_core::store::RECORD_UPCASTERS`.
///
/// version_guard(
///     roots(RestateDurableWaitIndexMetadata, IndexedWait),
///     roots(path = "crates/lash-restate/src/durable_wait/messages.rs", RestateDurableWaitAwaitRequest),
///     roots(path = "crates/lash-core-store/src/tool_run/source_seal.rs", SourceSeal),
///     roots(
///         path = "crates/lash-restate/src/durable_wait/source_seal.rs",
///         IndexedSource, RestateSourceArmRequest, RestateSourceArmReply,
///         RestateSourceSubscribeRequest, RestateSourceSubscribeReply,
///         RestateSourceSealRequest, RestateSourceSealReply, RestateSourceSealWrite,
///     ),
///     roots(path = "crates/lash-restate/src/ingress.rs", RestateInvocationId),
///     items(
///         DURABLE_WAIT_REGISTRY_FORMATS, DURABLE_WAIT_INDEX_METADATA_KEY,
///         DURABLE_WAIT_INDEX_WAIT_PREFIX, DURABLE_WAIT_INDEX_EFFECT_PREFIX,
///         DURABLE_WAIT_INDEX_PROCESS_JOURNAL_PREFIX, DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX,
///     ),
///     items(
///         path = "crates/lash-restate/src/durable_wait/source_seal.rs",
///         DURABLE_WAIT_INDEX_SOURCE_PREFIX, SOURCE_SEAL_PROMISE_KEY,
///     ),
///     shapes(path = "crates/lash-restate/src/object_state.rs", cover(StampedValue)),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "engine:restate.durable_wait_registry_format"
pub const DURABLE_WAIT_REGISTRY_FORMAT_VERSION: u16 = 1;
/// Phase A's synthetic N+1 (ADR 0115 §6) moves the family to format 2 with
/// format 1's shape; its `upgrade` handler rewrites each object after
/// finalize.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "engine:restate.durable_wait_registry_format"
pub const DURABLE_WAIT_REGISTRY_FORMAT_VERSION: u16 = 2;
/// The wait registry's stored-format table: the family's registered surface.
pub(crate) const DURABLE_WAIT_REGISTRY_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "durable-wait registry",
    surface: lash_core::surface_format!(DURABLE_WAIT_REGISTRY_FORMAT_VERSION),
};

/// The object family whose `_compat` record every handler admits first
/// (ADR 0115 §3.2).
pub(crate) const DURABLE_WAIT_REGISTRY_FAMILY: ObjectFamily = ObjectFamily {
    component: lash_core_store::compat::ComponentId::RESTATE_DURABLE_WAIT_REGISTRY,
    formats: &DURABLE_WAIT_REGISTRY_FORMATS,
};
/// version_surface = "coexist"
/// version_guard(items(DURABLE_WAIT_INDEX_METADATA_KEY, load_durable_wait_index_metadata, peek_turn_gate, read_durable_wait_index_metadata, register_awakeable, reinstate, resolve, unregister_awakeable), items(path = "crates/lash-restate/src/durable_wait/scope_retirement.rs", revoke_index))
pub(crate) const DURABLE_WAIT_INDEX_METADATA_KEY: &str = "wait-index/v2/metadata";
/// version_surface = "coexist"
/// version_guard(items(DURABLE_WAIT_INDEX_WAIT_PREFIX, durable_wait_address_from_state_key, durable_wait_index_state_key, load_indexed_waits))
const DURABLE_WAIT_INDEX_WAIT_PREFIX: &str = "wait-index/v2/wait/";
/// One wait's retained authority and mirrored terminal, for every scope kind.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(crate) struct IndexedWait {
    pub(crate) key: AwaitEventKey,
    pub(crate) terminal: Option<Resolution>,
}
/// An effect executing under the scope inside a handler, keyed by replay
/// key: recorded at start, cleared at completion (FIG-2499 quiescence).
/// version_surface = "coexist"
/// version_guard(items(DURABLE_WAIT_INDEX_EFFECT_PREFIX, durable_wait_index_effect_key), items(path = "crates/lash-restate/src/durable_wait/scope_retirement.rs", scope_effects_are_quiescent))
const DURABLE_WAIT_INDEX_EFFECT_PREFIX: &str = "wait-index/v2/effect/";
/// A process segment can issue effects until its journal closes. Its single
/// pin replaces the two index calls around each effect (FIG-4849).
/// version_surface = "coexist"
/// version_guard(items(DURABLE_WAIT_INDEX_PROCESS_JOURNAL_PREFIX), items(path = "crates/lash-restate/src/durable_wait/scope_retirement.rs", process_journal_key, process_journals_are_quiescent))
const DURABLE_WAIT_INDEX_PROCESS_JOURNAL_PREFIX: &str = "wait-index/v2/process-journal/";
/// A participant holding scope closure until its recorded work ends.
/// version_surface = "coexist"
/// version_guard(items(DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX, durable_wait_index_closure_participant_key), items(path = "crates/lash-restate/src/durable_wait/scope_retirement.rs", revoke_index))
const DURABLE_WAIT_INDEX_CLOSURE_PARTICIPANT_PREFIX: &str = "wait-index/v2/closure-participant/";

#[cfg(test)]
mod wait_registration_witness;

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    process_sources: Vec<ProcessTerminalSubscription>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    process_receivers: Vec<ProcessTerminalSubscription>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    process_unsubscribed: Vec<AwaitEventKey>,
}

pub(crate) fn restate_durable_wait_request(key: &AwaitEventKey) -> RestateDurableWaitAwaitRequest {
    RestateDurableWaitAwaitRequest { key: key.clone() }
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
    Registered(Box<RestateDurableWaitAwakeableRequest>),
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
    hand_over: Option<lash_core::engine::BuildGeneration>,
) -> Result<RestateTurnCancelGate, TerminalError>
where
    C: ContextClient<'ctx>,
{
    let entry = RestateDurableWaitAwakeableRequest {
        key,
        awakeable_id,
        hand_over,
    };
    let replay_key = entry.key.key_id.clone();
    let register = namespace
        .durable_wait_registry(context, session_id)
        .register_awakeable(entry.clone())
        .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
    let registration = register.call().await?.into_body();
    Ok(match registration {
        RestateDurableWaitRegistration::Revoked => RestateTurnCancelGate::Revoked,
        RestateDurableWaitRegistration::Registered
        | RestateDurableWaitRegistration::Resolved(_) => {
            RestateTurnCancelGate::Registered(Box::new(entry))
        }
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
        call: Call<RestateDurableWaitAwaitRequest>,
    ) -> HandlerResult<Reply<Resolution>>;

    #[shared]
    async fn peek(call: Call<()>) -> HandlerResult<Reply<Option<Resolution>>>;

    #[shared]
    async fn resolve(
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<ResolveOutcome>>;

    /// Hold a Deferred source's seal (K4, FIG-4883): the first seal stays,
    /// and a later write reads it. Only the source's
    /// `LashDurableWaitIndex/seal_source` writes here, after authenticating
    /// the write against the descriptor its Run pinned.
    #[shared]
    async fn seal_source(
        call: Call<RestateSourceSealWrite>,
    ) -> HandlerResult<Reply<lash_core::tool_run::SealOutcome>>;
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
        call: Call<RestateDurableWaitAwaitRequest>,
    ) -> HandlerResult<Reply<Resolution>> {
        let (wire, request) = call.open()?;
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

        let resolution: Resolution =
            if let Some(payload) = ctx.peek_promise::<String>(DURABLE_WAIT_PROMISE_KEY).await? {
                serde_json::from_str(&payload).map_err(TerminalError::from_error)?
            } else {
                let payload = restate_sdk::select! {
                    payload = ctx.promise::<String>(DURABLE_WAIT_PROMISE_KEY) => payload?,
                    on_cancel => {
                        return Ok(Reply::at(wire, Resolution::Cancelled));
                    }
                };
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

    async fn seal_source(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<RestateSourceSealWrite>,
    ) -> HandlerResult<Reply<lash_core::tool_run::SealOutcome>> {
        source_seal::hold_seal(ctx, call).await
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
    async fn attach_process_terminal(
        call: Call<ProcessTerminalSubscription>,
    ) -> HandlerResult<Reply<bool>>;
    async fn subscribe_process_terminal(
        call: Call<ProcessTerminalSubscription>,
    ) -> HandlerResult<Reply<Option<lash_core::ProcessAwaitOutput>>>;
    async fn unsubscribe_process_terminal(
        call: Call<ProcessTerminalSubscription>,
    ) -> HandlerResult<Reply<()>>;
    async fn deliver_process_terminal(
        call: Call<ProcessTerminalDelivery>,
    ) -> HandlerResult<Reply<()>>;
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
    async fn retire_run(call: Call<RestateDurableWaitRunRequest>) -> HandlerResult<Reply<()>>;
    async fn register_awakeable(
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitRegistration>>;
    async fn unregister_awakeable(
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<()>>;
    /// Wake every parked turn wait of this scope that registered a hand-over
    /// for the request's generation (FIG-4739), answering how many it woke.
    /// Each woken wait is left open and its turn ends at a segment boundary;
    /// its entry is dropped, as a fired gate's is. Idempotent: a second call
    /// finds no entry left to wake.
    async fn hand_over_turns(
        call: Call<RestateDurableWaitHandOverRequest>,
    ) -> HandlerResult<Reply<u64>>;
    async fn resolve(
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitResolveResponse>>;
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
    async fn register_process_journal(
        call: Call<RestateDurableWaitProcessJournalRequest>,
    ) -> HandlerResult<Reply<bool>>;
    async fn release_process_journal(
        call: Call<RestateDurableWaitProcessJournalRequest>,
    ) -> HandlerResult<Reply<()>>;
    async fn end_effect(call: Call<RestateDurableWaitEffectRequest>) -> HandlerResult<Reply<()>>;
    async fn register_closure_participant(
        call: Call<RestateTurnCancelClosureParticipantRequest>,
    ) -> HandlerResult<Reply<bool>>;
    async fn release_closure_participant(
        call: Call<RestateTurnCancelClosureParticipantRequest>,
    ) -> HandlerResult<Reply<()>>;
    /// Pin a Deferred source's descriptor before its key leaves the call
    /// (K4, FIG-4883). A re-arm with the same descriptor reads any seal the
    /// source holds; another descriptor is refused.
    async fn arm_source(
        call: Call<RestateSourceArmRequest>,
    ) -> HandlerResult<Reply<RestateSourceArmReply>>;
    /// Subscribe a segment of the source's owning Run: a sealed source
    /// answers its seal at once, an open one resolves the awakeable when it
    /// seals.
    async fn subscribe_source(
        call: Call<RestateSourceSubscribeRequest>,
    ) -> HandlerResult<Reply<RestateSourceSubscribeReply>>;
    async fn unsubscribe_source(
        call: Call<RestateSourceSubscribeRequest>,
    ) -> HandlerResult<Reply<()>>;
    /// Authenticate one seal write against the pinned descriptor and apply
    /// first-writer-wins: the reply is the source's one seal.
    async fn seal_source(
        call: Call<RestateSourceSealRequest>,
    ) -> HandlerResult<Reply<RestateSourceSealReply>>;
    /// Rewrite the index at the newest family format once finalize has
    /// moved the fleet to it, and raise its `_compat` (ADR 0115 §3.2,
    /// FIG-4041): the object sweep's step.
    async fn upgrade(call: Call<()>) -> HandlerResult<Reply<ObjectUpgradeResponse>>;
}

/// [`LashDurableWaitRegistry`] in one deployment's namespace (FIG-3898).
#[derive(Clone)]
pub(crate) struct LashDurableWaitRegistryImpl {
    namespace: crate::RestateNamespace,
    /// Where the handlers read the fleet epoch their writes are stamped at.
    fleet: FleetView,
    admin: Option<crate::RestateAdminClient>,
    attachments: Arc<dyn lash_core::AttachmentReferrers>,
    materials: Option<Arc<dyn lash_core::store::ToolMaterialStore>>,
}

impl Default for LashDurableWaitRegistryImpl {
    fn default() -> Self {
        Self {
            namespace: Default::default(),
            fleet: Default::default(),
            admin: None,
            attachments: Arc::new(lash_core::attachments::NoopAttachmentReferrers),
            materials: None,
        }
    }
}

impl LashDurableWaitRegistryImpl {
    pub(crate) fn new(
        namespace: crate::RestateNamespace,
        fleet: FleetView,
        admin: crate::RestateAdminClient,
    ) -> Self {
        Self {
            namespace,
            fleet,
            admin: Some(admin),
            ..Self::default()
        }
    }

    pub(crate) fn with_attachments(
        mut self,
        attachments: Arc<dyn lash_core::AttachmentReferrers>,
    ) -> Self {
        self.attachments = attachments;
        self
    }

    pub(crate) fn with_materials(
        mut self,
        materials: Arc<dyn lash_core::store::ToolMaterialStore>,
    ) -> Self {
        self.materials = Some(materials);
        self
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
    format!("{DURABLE_WAIT_INDEX_WAIT_PREFIX}{}", address.workflow_key)
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
    let workflow_key = state_key.strip_prefix(DURABLE_WAIT_INDEX_WAIT_PREFIX)?;
    if workflow_key.is_empty() || workflow_key.contains('/') {
        return None;
    }
    let address = RestateDurableWaitAddress::for_key(key);
    (address.workflow_key == workflow_key).then_some(address)
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

async fn load_indexed_waits(ctx: &ObjectContext<'_>) -> Result<Vec<IndexedWait>, TerminalError> {
    let keys = ctx.get_keys().await?;
    load_indexed_waits_in(ctx, &keys).await
}

/// [`load_indexed_waits`] over a key listing the handler already read.
async fn load_indexed_waits_in(
    ctx: &ObjectContext<'_>,
    keys: &[String],
) -> Result<Vec<IndexedWait>, TerminalError> {
    let mut waits = Vec::new();
    for state_key in keys
        .iter()
        .filter(|state_key| state_key.starts_with(DURABLE_WAIT_INDEX_WAIT_PREFIX))
    {
        let wait: IndexedWait =
            object_state::get_stamped(ctx, state_key, &DURABLE_WAIT_REGISTRY_FORMATS)
                .await?
                .ok_or_else(|| {
                    TerminalError::new(format!(
                        "durable-wait index entry {state_key} has no key preimage"
                    ))
                })?;
        let address =
            durable_wait_address_from_state_key(&wait.key, state_key).ok_or_else(|| {
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
        waits.push(wait);
    }
    Ok(waits)
}

async fn read_outstanding_waits(
    ctx: &ObjectContext<'_>,
) -> Result<Vec<AwaitEventKey>, TerminalError> {
    let metadata = read_durable_wait_index_metadata(ctx)
        .await?
        .unwrap_or_default();
    if metadata.revoked {
        return Ok(Vec::new());
    }

    let keys = ctx.get_keys().await?;
    let sources = source_seal::load_sources(ctx, &keys, |_| true).await?;
    let mut outstanding = Vec::new();
    for wait in load_indexed_waits_in(ctx, &keys).await? {
        if wait.terminal.is_none()
            && sources
                .iter()
                .all(|(_, source)| source.descriptor.source != wait.key || source.seal.is_none())
        {
            outstanding.push(wait.key);
        }
    }
    for (_, source) in sources {
        if source.seal.is_none()
            && source.descriptor.authority
                == lash_core::tool_run::SourceAuthority::ExternalCompletion
        {
            outstanding.push(source.descriptor.source);
        }
    }
    outstanding.sort_unstable_by(|left, right| left.key_id.cmp(&right.key_id));
    outstanding.dedup();
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

fn store_indexed_wait(
    ctx: &ObjectContext<'_>,
    writer: StoredValueWriter,
    key: &AwaitEventKey,
    address: &RestateDurableWaitAddress,
    terminal: Option<Resolution>,
) {
    object_state::set_stamped(
        ctx,
        &durable_wait_index_state_key(address),
        writer,
        IndexedWait {
            key: key.clone(),
            terminal,
        },
    );
}

fn durable_wait_index_effect_key(replay_key: &str) -> String {
    format!("{DURABLE_WAIT_INDEX_EFFECT_PREFIX}{replay_key}")
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
    async fn attach_process_terminal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<ProcessTerminalSubscription>,
    ) -> HandlerResult<Reply<bool>> {
        process_terminal::attach(self, ctx, call).await
    }
    async fn subscribe_process_terminal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<ProcessTerminalSubscription>,
    ) -> HandlerResult<Reply<Option<lash_core::ProcessAwaitOutput>>> {
        process_terminal::subscribe(self, ctx, call).await
    }
    async fn unsubscribe_process_terminal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<ProcessTerminalSubscription>,
    ) -> HandlerResult<Reply<()>> {
        process_terminal::unsubscribe(self, ctx, call).await
    }
    async fn deliver_process_terminal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<ProcessTerminalDelivery>,
    ) -> HandlerResult<Reply<()>> {
        process_terminal::deliver(self, ctx, call).await
    }
    async fn upgrade(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<ObjectUpgradeResponse>> {
        let (wire, ()) = call.open()?;
        let response = object_state::upgrade_object(
            &ctx,
            &DURABLE_WAIT_REGISTRY_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        Ok(Reply::at(wire, response))
    }

    async fn retire_run(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitRunRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        run_retirement::retire_run(ctx, object, self, request)
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
        let state_key = durable_wait_index_state_key(&address);
        Ok(Reply::at(
            wire,
            RestateTurnGatePeek::Open(
                object_state::get_stamped_shared::<IndexedWait>(
                    &ctx,
                    &state_key,
                    &DURABLE_WAIT_REGISTRY_FORMATS,
                )
                .await?
                .and_then(|wait| wait.terminal),
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
        subscriptions::register(self, ctx, call).await
    }

    async fn settle(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitSettleRequest>,
    ) -> HandlerResult<Reply<()>> {
        subscriptions::settle(self, ctx, call).await
    }

    async fn register_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitRegistration>> {
        subscriptions::register_awakeable(self, ctx, call).await
    }

    async fn unregister_awakeable(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitAwakeableRequest>,
    ) -> HandlerResult<Reply<()>> {
        subscriptions::unregister_awakeable(self, ctx, call).await
    }

    async fn hand_over_turns(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitHandOverRequest>,
    ) -> HandlerResult<Reply<u64>> {
        subscriptions::hand_over_turns(self, ctx, call).await
    }

    async fn resolve(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitResolveRequest>,
    ) -> HandlerResult<Reply<RestateDurableWaitResolveResponse>> {
        subscriptions::resolve(self, ctx, call).await
    }

    async fn cancel_all(&self, ctx: ObjectContext<'_>, call: Call<()>) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        let mut metadata = load_durable_wait_index_metadata(&ctx, object.writer).await?;
        let (waits, _controls) = split_cancellable_waits(
            load_indexed_waits(&ctx)
                .await?
                .into_iter()
                .map(|wait| wait.key)
                .collect(),
        );
        if wake_ended_waits(
            &self.namespace,
            &ctx,
            &mut metadata,
            &Resolution::Cancelled,
            |key| waits.contains(key),
        ) {
            object_state::set_stamped(
                &ctx,
                DURABLE_WAIT_INDEX_METADATA_KEY,
                object.writer,
                metadata,
            );
        }
        resolve_indexed_waits(&ctx, object.writer, &self.namespace, waits, true).await?;
        Ok(Reply::at(wire, ()))
    }

    async fn revoke_all(&self, ctx: ObjectContext<'_>, call: Call<()>) -> HandlerResult<Reply<()>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        revoke_index(&ctx, object, self, false).await?;
        Ok(Reply::at(wire, ()))
    }

    async fn revoke_all_if_quiescent(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<bool>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        revoke_index(&ctx, object, self, true)
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
        revoke_index(&ctx, object, self, false)
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

    async fn register_process_journal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitProcessJournalRequest>,
    ) -> HandlerResult<Reply<bool>> {
        scope_retirement::register_process_journal(self, ctx, call).await
    }

    async fn release_process_journal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateDurableWaitProcessJournalRequest>,
    ) -> HandlerResult<Reply<()>> {
        scope_retirement::release_process_journal(self, ctx, call).await
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

    async fn arm_source(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateSourceArmRequest>,
    ) -> HandlerResult<Reply<RestateSourceArmReply>> {
        source_seal::arm_source(self, ctx, call).await
    }

    async fn subscribe_source(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateSourceSubscribeRequest>,
    ) -> HandlerResult<Reply<RestateSourceSubscribeReply>> {
        source_seal::subscribe_source(self, ctx, call).await
    }

    async fn unsubscribe_source(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateSourceSubscribeRequest>,
    ) -> HandlerResult<Reply<()>> {
        source_seal::unsubscribe_source(self, ctx, call).await
    }

    async fn seal_source(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateSourceSealRequest>,
    ) -> HandlerResult<Reply<RestateSourceSealReply>> {
        source_seal::seal_source(self, ctx, call).await
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
