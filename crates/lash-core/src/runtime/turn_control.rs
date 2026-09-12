use crate::SessionId;
use crate::TurnId;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{ErrorEnvelope, TurnOutcome};

use super::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    Resolution, ResolveOutcome, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, ScopedEffectController,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnCancelPeekIdentity {
    StartGate,
    PostAbortGate,
    // Shipped protocols issue at most one LLM call per protocol iteration, so
    // the iteration is a unique replay identity for this between-call peek.
    AfterLlm {
        protocol_iteration: usize,
    },
    /// The step boundary that closed `protocol_iteration`: its response was
    /// streamed, every tool call of that iteration completed, and its
    /// checkpoint committed. Observed exactly once per closed iteration on
    /// every binding, so it is a stable replay identity.
    AfterStep {
        protocol_iteration: usize,
    },
}

impl TurnCancelPeekIdentity {
    fn causal_identity(self) -> String {
        match self {
            Self::StartGate => "turn_cancel.start_gate".to_string(),
            Self::PostAbortGate => "turn_cancel.post_abort_gate".to_string(),
            Self::AfterLlm { protocol_iteration } => {
                format!("turn_cancel.after_llm.{protocol_iteration}")
            }
            Self::AfterStep { protocol_iteration } => {
                format!("turn_cancel.after_step.{protocol_iteration}")
            }
        }
    }

    /// Identity of the escalation peek that follows this gate peek when the
    /// gate holds an after-step request. Issued only in that case, and the
    /// gate answer it depends on is itself journaled, so replay is stable.
    fn escalation_causal_identity(self) -> String {
        format!(
            "turn_cancel.escalation.{}",
            &self.causal_identity()["turn_cancel.".len()..]
        )
    }

    /// Boundaries that honour an after-step request outright. The post-abort
    /// gate is reached only after the cooperative token already fired, and a
    /// mid-run peek defers an after-step request to its step boundary.
    fn honours_after_step(self) -> Option<Option<usize>> {
        match self {
            Self::StartGate | Self::PostAbortGate => Some(None),
            Self::AfterStep { protocol_iteration } => Some(Some(protocol_iteration)),
            Self::AfterLlm { .. } => None,
        }
    }
}

const PHYSICAL_TURN_CANCEL_PEEK_FAMILY_VERSION: u8 = 1;

fn turn_cancel_peek_replay_key(
    execution_scope: &ExecutionScope,
    address: &TurnAddress,
    causal_identity: &str,
) -> String {
    if matches!(
        execution_scope,
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } if session_id == address.session_id && turn_id == address.turn_id
    ) {
        return causal_identity.to_string();
    }
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.turn-cancel-peek",
        PHYSICAL_TURN_CANCEL_PEEK_FAMILY_VERSION,
    );
    identity.string(&address.session_id);
    identity.string(&address.turn_id);
    identity.string(causal_identity);
    crate::stable_identity::rendered_hash(
        "turn-cancel-peek",
        PHYSICAL_TURN_CANCEL_PEEK_FAMILY_VERSION,
        &identity.finish(),
    )
}

/// Stable routing identity for one foreground turn.
///
/// These identifiers select work; they are not authorization credentials.
/// Hosts exposing turn control to untrusted callers must authenticate and
/// authorize the request before calling Lash.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnAddress {
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

impl TurnAddress {
    pub fn new(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    pub fn execution_scope(&self) -> ExecutionScope {
        ExecutionScope::turn(&self.session_id, &self.turn_id)
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        Ok(self.execution_scope().validate()?)
    }
}

/// Shared origin hint for a process-local cancellation token.
///
/// The outer option records whether a local entry point supplied a hint; the
/// inner option is the opaque host origin, which may intentionally be absent.
/// It is not a durable cancellation request and must not be used as
/// authorization.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct TurnCancelOriginHint {
    state: Arc<Mutex<TurnCancelOriginState>>,
}

#[derive(Default)]
struct TurnCancelOriginState {
    configured_origin: Option<Option<String>>,
    observed_origin: Option<Option<String>>,
    after_step_requested: bool,
}

impl TurnCancelOriginHint {
    /// Record the origin of an observed cancellation request.
    pub fn set(&self, origin: Option<String>) {
        let mut state = self.state.lock_recover();
        if state.observed_origin.is_none() {
            state.observed_origin = Some(origin);
        }
    }

    /// Record a process-local after-step stop. The token stays untouched; the
    /// owning turn honours the flag at its next step boundary by resolving its
    /// own cancellation gate with internal after-step evidence.
    pub fn request_after_step(&self, origin: Option<String>) {
        let mut state = self.state.lock_recover();
        if state.observed_origin.is_none() {
            state.observed_origin = Some(origin);
        }
        state.after_step_requested = true;
    }

    pub(crate) fn after_step_requested(&self) -> bool {
        self.state.lock_recover().after_step_requested
    }

    pub(crate) fn get(&self) -> Option<String> {
        let state = self.state.lock_recover();
        state
            .observed_origin
            .clone()
            .or_else(|| state.configured_origin.clone())
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn was_set(&self) -> bool {
        self.state.lock_recover().observed_origin.is_some()
    }

    /// Record a process-local token and the origin to use if that token fires
    /// independently of a routed cancellation request.
    #[doc(hidden)]
    pub(crate) fn configure_local_token(&self, origin: Option<String>) {
        let mut state = self.state.lock_recover();
        state.configured_origin = Some(origin);
    }
}

pub use lash_sansio::{TurnCancelDisposition, TurnCancelMode, TurnCancellationEvidence};

/// One undelivered active-turn input affected by cancellation repair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnCancelAffectedInput {
    pub input_id: String,
    pub payload: crate::TurnInput,
    pub disposition: TurnCancelDisposition,
}

impl PartialEq for TurnCancelAffectedInput {
    fn eq(&self, other: &Self) -> bool {
        self.input_id == other.input_id
            && self.disposition == other.disposition
            && serde_json::to_value(&self.payload).ok() == serde_json::to_value(&other.payload).ok()
    }
}

impl Eq for TurnCancelAffectedInput {}

/// Durable outcome accumulated on a turn-cancel request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelInputOutcome {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_inputs: Vec<TurnCancelAffectedInput>,
}

/// A durable cancellation request header together with its monotonic intent
/// revision. The revision changes only when the request header changes, so it
/// fences delayed projections without coupling them to outcome retention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnCancelIntentSnapshot {
    Absent,
    Present {
        request: TurnCancelRequest,
        revision: u64,
    },
}

/// The exact terminal one authorized cancellation-closure operation proposes
/// for the base gate. This is durable operation intent, never a second winner:
/// the keyed promise still decides which terminal was accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "cancellation", rename_all = "snake_case")]
pub enum TurnCancelClosureProposal {
    CancelRequested(TurnCancellationEvidence),
    CompletionSealed,
}

/// Durable authorization to close one turn's cancellation gate pair.
///
/// Store implementations persist this value in a non-overwritable per-turn
/// slot under the current session-execution fence. A successor may finish the
/// exact promise operation after takeover, but only the current fence may apply
/// input effects, commit, publish, or consume the slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelClosureAuthorization {
    session_id: SessionId,
    turn_id: TurnId,
    binding_id: String,
    admitted_scope: ExecutionScope,
    cancel_key: AwaitEventKey,
    escalation_key: AwaitEventKey,
    terminal_key: AwaitEventKey,
    proposed_base: TurnCancelClosureProposal,
    observed_intent: TurnCancelIntentSnapshot,
    authorizing_fencing_token: u64,
}

/// Authenticated terminal produced by the exact durable promise owner for one
/// persisted cancellation-closure authorization.
///
/// Callers cannot construct this value. Stores accept it instead of a
/// caller-supplied repair decision, so consuming an authorization necessarily
/// follows successful settlement by the binding recorded in that
/// authorization. `base_cancellation` is the immutable first policy acceptor;
/// `effective_cancellation` may carry a later same-policy timing escalation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCancelClosureSettlement {
    authorization: TurnCancelClosureAuthorization,
    base_cancellation: Option<TurnCancellationEvidence>,
    effective_cancellation: Option<TurnCancellationEvidence>,
}

impl TurnCancelClosureSettlement {
    pub fn authorization(&self) -> &TurnCancelClosureAuthorization {
        &self.authorization
    }

    pub fn base_cancellation(&self) -> Option<&TurnCancellationEvidence> {
        self.base_cancellation.as_ref()
    }

    pub fn effective_cancellation(&self) -> Option<&TurnCancellationEvidence> {
        self.effective_cancellation.as_ref()
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn settled_for_test(
        authorization: TurnCancelClosureAuthorization,
        base_cancellation: Option<TurnCancellationEvidence>,
        effective_cancellation: Option<TurnCancellationEvidence>,
    ) -> Self {
        Self {
            authorization,
            base_cancellation,
            effective_cancellation,
        }
    }
}

impl TurnCancelClosureAuthorization {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: TurnAddress,
        binding_id: impl Into<String>,
        admitted_scope: ExecutionScope,
        cancel_key: AwaitEventKey,
        escalation_key: AwaitEventKey,
        terminal_key: AwaitEventKey,
        proposed_base: TurnCancelClosureProposal,
        observed_intent: TurnCancelIntentSnapshot,
        fence: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<Self, RuntimeError> {
        address.validate()?;
        admitted_scope.validate()?;
        let binding_id = binding_id.into();
        if binding_id.trim().is_empty() {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation closure requires a non-empty binding id",
            ));
        }
        if fence.session_id != address.session_id {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::SessionExecutionLeaseLost,
                "turn cancellation closure fence belongs to another session",
            ));
        }
        let expected_scope = address.execution_scope();
        if admitted_scope.session_id().is_some() && admitted_scope != expected_scope {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "session-scoped turn cancellation closure admission must name its exact turn",
            ));
        }
        if cancel_key.scope != expected_scope
            || escalation_key.scope != expected_scope
            || terminal_key.scope != expected_scope
            || cancel_key.wait != AwaitEventWaitIdentity::TurnCancelGate
            || escalation_key.wait != AwaitEventWaitIdentity::TurnCancelEscalation
            || terminal_key.wait != AwaitEventWaitIdentity::TurnTerminal
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation closure keys do not match the authorized turn",
            ));
        }
        Ok(Self {
            session_id: address.session_id,
            turn_id: address.turn_id,
            binding_id,
            admitted_scope,
            cancel_key,
            escalation_key,
            terminal_key,
            proposed_base,
            observed_intent,
            authorizing_fencing_token: fence.fencing_token,
        })
    }

    pub fn address(&self) -> TurnAddress {
        TurnAddress::new(&self.session_id, &self.turn_id)
    }

    /// Revalidate a decoded authorization before a backend or resolver trusts it.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.address().validate()?;
        self.admitted_scope.validate()?;
        let expected_scope = self.address().execution_scope();
        if self.binding_id.trim().is_empty()
            || !crate::runtime::effect::executor::binding_id_admits_scope(
                &self.binding_id,
                &self.admitted_scope,
            )
            || (self.admitted_scope.session_id().is_some() && self.admitted_scope != expected_scope)
            || self.cancel_key.scope != expected_scope
            || self.escalation_key.scope != expected_scope
            || self.terminal_key.scope != expected_scope
            || self.cancel_key.wait != AwaitEventWaitIdentity::TurnCancelGate
            || self.escalation_key.wait != AwaitEventWaitIdentity::TurnCancelEscalation
            || self.terminal_key.wait != AwaitEventWaitIdentity::TurnTerminal
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation closure authorization is structurally invalid",
            ));
        }
        if let TurnCancelClosureProposal::CancelRequested(evidence) = &self.proposed_base
            && evidence.request_id.trim().is_empty()
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation closure evidence requires a non-empty request id",
            ));
        }
        Ok(())
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }
    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }
    pub fn admitted_scope(&self) -> &ExecutionScope {
        &self.admitted_scope
    }
    pub fn cancel_key(&self) -> &AwaitEventKey {
        &self.cancel_key
    }
    pub fn escalation_key(&self) -> &AwaitEventKey {
        &self.escalation_key
    }
    pub fn terminal_key(&self) -> &AwaitEventKey {
        &self.terminal_key
    }
    pub fn proposed_base(&self) -> &TurnCancelClosureProposal {
        &self.proposed_base
    }
    pub fn observed_intent(&self) -> &TurnCancelIntentSnapshot {
        &self.observed_intent
    }
    pub fn authorizing_fencing_token(&self) -> u64 {
        self.authorizing_fencing_token
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnCancelClosureAuthorizationOutcome {
    Authorized,
    AdoptedExact,
}

impl TurnCancelIntentSnapshot {
    pub fn request(&self) -> Option<&TurnCancelRequest> {
        match self {
            Self::Absent => None,
            Self::Present { request, .. } => Some(request),
        }
    }
}

impl TurnCancelInputOutcome {
    /// Reports whether cancellation repair affected no active-turn input, so hosts can skip
    /// restore, re-enqueue, or audit work without inspecting the payload list.
    pub fn is_empty(&self) -> bool {
        self.affected_inputs.is_empty()
    }

    /// Number of affected pending inputs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.affected_inputs.len()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelRequest {
    pub address: TurnAddress,
    pub request_id: String,
    /// Opaque host-domain data. Lash never interprets this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Policy for active-turn input the cancelled turn did not deliver.
    #[serde(default, skip_serializing_if = "turn_cancel_disposition_is_defer")]
    pub undelivered: TurnCancelDisposition,
    /// When the request is honoured. `Immediate` fires the cooperative token
    /// and backtracks to the last checkpoint; `AfterStep` waits for the step
    /// boundary that closes the current protocol iteration. Records written
    /// before this field existed decode as `Immediate`.
    #[serde(default, skip_serializing_if = "TurnCancelMode::is_immediate")]
    pub mode: TurnCancelMode,
}

fn turn_cancel_disposition_is_defer(disposition: &TurnCancelDisposition) -> bool {
    matches!(disposition, TurnCancelDisposition::Defer)
}

impl TurnCancelRequest {
    pub fn new(
        address: TurnAddress,
        request_id: impl Into<String>,
        origin: Option<String>,
    ) -> Self {
        Self {
            address,
            request_id: request_id.into(),
            origin,
            reason: None,
            undelivered: TurnCancelDisposition::Defer,
            mode: TurnCancelMode::Immediate,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn undelivered(mut self, disposition: TurnCancelDisposition) -> Self {
        self.undelivered = disposition;
        self
    }

    pub fn mode(mut self, mode: TurnCancelMode) -> Self {
        self.mode = mode;
        self
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        self.address.validate()?;
        if self.request_id.trim().is_empty() {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation requires a non-empty request id",
            ));
        }
        Ok(())
    }

    pub(crate) fn evidence(&self) -> TurnCancellationEvidence {
        TurnCancellationEvidence {
            request_id: self.request_id.clone(),
            origin: self.origin.clone(),
            reason: self.reason.clone(),
            undelivered: self.undelivered,
            mode: self.mode,
            honoured_after_step: None,
        }
    }
}

/// Durable request and the input outcome accumulated by repair paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelRequestRecord {
    pub request: TurnCancelRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnCancelInputOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "cancellation", rename_all = "snake_case")]
pub enum TurnCancelOutcome {
    Requested(TurnCancellationEvidence),
    AlreadyRequested(TurnCancellationEvidence),
    /// The address already held a weaker durable request and this stronger
    /// one upgraded it. The evidence is the escalating request's.
    Escalated(TurnCancellationEvidence),
    /// The turn already accepted a different undelivered-input policy.
    ///
    /// Cancellation policy belongs to the immutable base-gate winner. A
    /// timing escalation may change when that cancellation is honoured, but
    /// it never changes who accepted the policy or what that policy is.
    PolicyConflict {
        requested: TurnCancelDisposition,
        accepted: TurnCancellationEvidence,
    },
    CompletionWonRace,
    UnknownOrRevoked,
}

/// Result of addressing one turn-cancellation gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelReceipt {
    pub outcome: TurnCancelOutcome,
    /// Durable request and repair outcome when the driver has a session store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<TurnCancelRequestRecord>,
}

/// The published terminal of one foreground turn.
///
/// This value is the payload of the turn's terminal keyed promise, so its JSON
/// encoding is a durable carrier: a terminal published by one binary can be
/// read by another after a rolling upgrade. That carrier is forward-only and
/// unversioned by design — Lash never reads a superseded shape. A reshape of
/// `TurnTerminal` therefore fails an in-flight
/// [`TurnAttach::await_terminal`] typed, with
/// [`crate::RuntimeErrorCode::TurnTerminalDecode`], and the host re-awaits or
/// re-reads the committed turn rather than getting a silently misread
/// terminal. Moving cancellation evidence into `TurnStop::Cancelled` was one
/// such reshape.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TurnTerminal {
    Committed {
        /// Cancellation evidence, when any, rides `outcome` — see
        /// [`TurnOutcome::cancellation`].
        outcome: TurnOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_revision: Option<u64>,
    },
    Failed {
        error: ErrorEnvelope,
    },
}

/// Backend-specific terminal attachment for a foreground turn.
///
/// [`crate::RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed`] means only a
/// bounded transport attachment elapsed. The durable terminal wait remains
/// live, so the host must re-attach with the same [`TurnAddress`].
#[async_trait::async_trait]
pub trait TurnAttach: Send + Sync {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError>;
}

pub(crate) async fn await_terminal_from_resolver(
    resolver: &dyn AwaitEventResolver,
    address: &TurnAddress,
) -> Result<TurnTerminal, RuntimeError> {
    address.validate()?;
    let key = terminal_key(resolver, address).await?;
    let resolution = resolver
        .await_await_event(&key, CancellationToken::new(), None)
        .await?;
    decode_terminal(address, resolution)
}

/// Cooperative, exact-turn control compiled onto Lash's keyed-promise seam.
///
/// `Requested` means the cancellation request won this driver's keyed-promise
/// gate. On an native effect host that promise is process-local, so the driver
/// only reaches owners in the same OS process. A durable effect-host deployment
/// is required for another process or replayed owner to observe the request.
/// The returned [`TurnCancelReceipt`] reports only the cancellation outcome.
/// Hosts must use their configured effect-host topology when deciding whether
/// an native receipt is cross-process proof.
///
/// Lash asks the running or replayed owner to unwind and commit a cancelled
/// result; it cannot guarantee that detached tasks, subprocesses, or
/// non-cooperative providers have stopped. Engine invocation cancellation
/// remains a host-owned break-glass action and is never proof of a Lash
/// `Cancelled` result.
///
/// Session and turn ids are routing identity, not authorization. Hosts must
/// enforce authorization before exposing this driver across a trust boundary.
#[derive(Clone)]
pub struct TurnWorkDriver {
    effect_host: Arc<dyn EffectHost>,
    store: TurnWorkStore,
    #[cfg(any(test, feature = "testing"))]
    test_attach: Option<Arc<dyn TurnAttach>>,
}

#[derive(Clone)]
enum TurnWorkStore {
    Session {
        session_id: String,
        store: Arc<dyn crate::RuntimePersistence>,
    },
    Catalog(Arc<dyn crate::SessionStoreFactory>),
}

impl TurnWorkDriver {
    /// Bind control to one already-opened session store.
    ///
    /// The address is checked against `session_id` before the store or effect
    /// host is touched. Facades with an opened session should use this form so
    /// a root catalog override cannot redirect cancellation storage.
    pub fn for_session(
        effect_host: Arc<dyn EffectHost>,
        session_id: impl Into<String>,
        store: Arc<dyn crate::RuntimePersistence>,
    ) -> Self {
        Self {
            effect_host,
            store: TurnWorkStore::Session {
                session_id: session_id.into(),
                store,
            },
            #[cfg(any(test, feature = "testing"))]
            test_attach: None,
        }
    }

    /// Bind control to a deployment catalog for arbitrary-session addressing.
    ///
    /// Each request resolves its store from this same catalog. This is the
    /// remote/admin form; an already-opened session uses [`Self::for_session`].
    pub fn for_catalog(
        effect_host: Arc<dyn EffectHost>,
        store_factory: Arc<dyn crate::SessionStoreFactory>,
    ) -> Self {
        Self {
            effect_host,
            store: TurnWorkStore::Catalog(store_factory),
            #[cfg(any(test, feature = "testing"))]
            test_attach: None,
        }
    }

    /// Override terminal attachment in test builds.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn with_test_attach(mut self, attach: Arc<dyn TurnAttach>) -> Self {
        self.test_attach = Some(attach);
        self
    }

    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub async fn request_cancel(
        &self,
        request: TurnCancelRequest,
    ) -> Result<TurnCancelReceipt, RuntimeError> {
        request.validate()?;
        self.validate_address(&request.address)?;
        let store = self.store_for(&request.address).await?;
        // A durable receipt is authoritative even when the promise namespace
        // has since been revoked or its binding is unavailable.
        if store
            .turn_is_committed(&request.address)
            .await
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
            })?
        {
            return Ok(TurnCancelReceipt {
                outcome: TurnCancelOutcome::CompletionWonRace,
                record: None,
            });
        }
        let store_authority = (self.effect_host.turn_control_authority_owner()
            == crate::TurnControlAuthorityOwner::SessionStore)
            .then(|| store.turn_cancellation_authority())
            .flatten();
        let store_resolver = store_authority
            .as_ref()
            .map(|authority| authority.resolver());
        let resolver: &dyn AwaitEventResolver = store_resolver
            .as_ref()
            .map_or(self.effect_host.as_ref(), |resolver| resolver.as_ref());
        let key = match cancel_gate_key(resolver, &request.address).await {
            Ok(key) => key,
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(TurnCancelReceipt {
                    outcome: TurnCancelOutcome::UnknownOrRevoked,
                    record: None,
                });
            }
            Err(err) => return Err(err),
        };
        let terminal_key = match terminal_key(resolver, &request.address).await {
            Ok(key) => key,
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(TurnCancelReceipt {
                    outcome: TurnCancelOutcome::UnknownOrRevoked,
                    record: None,
                });
            }
            Err(err) => return Err(err),
        };
        match resolver.peek_await_event(&terminal_key).await {
            Ok(Some(terminal)) => {
                decode_terminal(&request.address, terminal)?;
                return Ok(TurnCancelReceipt {
                    outcome: TurnCancelOutcome::CompletionWonRace,
                    record: None,
                });
            }
            Ok(None) => {}
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(TurnCancelReceipt {
                    outcome: TurnCancelOutcome::UnknownOrRevoked,
                    record: None,
                });
            }
            Err(err) => return Err(err),
        }
        let _recorded_intent = store
            .record_turn_cancel_request(request.clone())
            .await
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
            })?;
        // The projection predicate must be observed after this caller's
        // provisional intent write and before it consults either gate. A
        // concurrent write in between is harmless: the snapshot then names
        // the newer row that the later gate observation is allowed to
        // reconcile, while any write after this read advances the revision
        // and makes the store CAS refuse.
        let mut observed = store
            .turn_cancel_request_intent(&request.address)
            .await
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
            })?;
        // The record write and the final commit serialize on the store's
        // session transaction authority. Recheck after it so a commit that won
        // before this intent was eligible produces a typed no-op, including
        // the gap before terminal promise publication.
        if store
            .turn_is_committed(&request.address)
            .await
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
            })?
        {
            return Ok(TurnCancelReceipt {
                outcome: TurnCancelOutcome::CompletionWonRace,
                record: None,
            });
        }
        // The store row is durable intent, not arbitration authority. The
        // incoming request is the candidate this caller presents to the gate;
        // whichever candidate actually resolves the gate is the winner.
        let evidence = request.evidence();
        let resolution = gate_resolution(TurnGateTerminal::CancelRequested(evidence.clone()))?;
        let resolved: Result<(TurnCancelOutcome, Option<TurnCancellationEvidence>), RuntimeError> =
            match resolver.resolve_await_event(&key, resolution).await? {
                ResolveOutcome::Accepted => Ok((
                    TurnCancelOutcome::Requested(evidence.clone()),
                    Some(evidence),
                )),
                ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
                    TurnGateTerminal::CancelRequested(existing)
                        if evidence.undelivered != existing.undelivered =>
                    {
                        Ok((
                            TurnCancelOutcome::PolicyConflict {
                                requested: evidence.undelivered,
                                accepted: existing.clone(),
                            },
                            Some(existing),
                        ))
                    }
                    TurnGateTerminal::CancelRequested(existing)
                        if evidence.mode.is_stronger_than(existing.mode) =>
                    {
                        let outcome = self
                            .escalate(resolver, &request.address, evidence, existing.clone())
                            .await?;
                        Ok((outcome, Some(existing)))
                    }
                    TurnGateTerminal::CancelRequested(existing) => Ok((
                        TurnCancelOutcome::AlreadyRequested(
                            effective_cancel_evidence(resolver, &request.address, existing.clone())
                                .await?,
                        ),
                        Some(existing),
                    )),
                    TurnGateTerminal::CompletionSealed => {
                        Ok((TurnCancelOutcome::CompletionWonRace, None))
                    }
                },
                ResolveOutcome::UnknownOrRevoked => Ok((TurnCancelOutcome::UnknownOrRevoked, None)),
            };
        let (outcome, mut base_winner) = resolved?;
        while let Some(evidence) = base_winner.as_ref() {
            if store
                .reconcile_turn_cancel_winner(&request.address, &observed, evidence)
                .await
                .map_err(|err| {
                    RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
                })?
            {
                break;
            }
            observed = store
                .turn_cancel_request_intent(&request.address)
                .await
                .map_err(|err| {
                    RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
                })?;
            base_winner =
                match ActiveTurnControl::peek_base_cancel_evidence(resolver, &request.address)
                    .await?
                {
                    Some(evidence) => Some(evidence),
                    None => None,
                };
        }
        let record = store
            .turn_cancel_request(&request.address)
            .await
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
            })?;
        Ok(TurnCancelReceipt { outcome, record })
    }

    async fn store_for(
        &self,
        address: &TurnAddress,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, RuntimeError> {
        self.validate_address(address)?;
        match &self.store {
            TurnWorkStore::Session { session_id, store } => {
                debug_assert_eq!(session_id, &address.session_id);
                Ok(Arc::clone(store))
            }
            TurnWorkStore::Catalog(factory) => factory
                .open_existing_store_by_id(&address.session_id)
                .await
                .map_err(|err| RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err))?
                .ok_or_else(|| {
                    RuntimeError::new(
                        crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                        format!("session `{}` does not exist", address.session_id),
                    )
                }),
        }
    }

    fn validate_address(&self, address: &TurnAddress) -> Result<(), RuntimeError> {
        if let TurnWorkStore::Session { session_id, .. } = &self.store
            && session_id != &address.session_id
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "turn work driver is bound to session `{session_id}` and cannot address `{}`",
                    address.session_id
                ),
            ));
        }
        Ok(())
    }

    /// Upgrade an address whose first-writer gate holds a weaker request.
    ///
    /// The gate itself is immutable once written, so the stronger request
    /// rides a second reserved promise that the owner watches only after it
    /// observed a weaker gate. It is first-writer-wins too: a second stronger
    /// request reports the escalation that already won.
    async fn escalate(
        &self,
        resolver: &dyn AwaitEventResolver,
        address: &TurnAddress,
        evidence: TurnCancellationEvidence,
        existing: TurnCancellationEvidence,
    ) -> Result<TurnCancelOutcome, RuntimeError> {
        let key = escalation_key(resolver, address).await?;
        let resolution = gate_resolution(TurnGateTerminal::CancelRequested(evidence.clone()))?;
        Ok(
            match resolver.resolve_await_event(&key, resolution).await? {
                ResolveOutcome::Accepted => TurnCancelOutcome::Escalated(evidence),
                ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
                    TurnGateTerminal::CancelRequested(escalated) => {
                        TurnCancelOutcome::AlreadyRequested(escalated)
                    }
                    TurnGateTerminal::CompletionSealed => {
                        TurnCancelOutcome::AlreadyRequested(existing)
                    }
                },
                ResolveOutcome::UnknownOrRevoked => TurnCancelOutcome::UnknownOrRevoked,
            },
        )
    }

    pub async fn await_terminal(
        &self,
        address: &TurnAddress,
    ) -> Result<TurnTerminal, RuntimeError> {
        address.validate()?;
        self.validate_address(address)?;
        #[cfg(any(test, feature = "testing"))]
        if let Some(attach) = self.test_attach.as_ref() {
            return attach.await_terminal(address).await;
        }
        if let Some(attach) = self.effect_host.turn_attach() {
            return attach.await_terminal(address).await;
        }
        let store = self.store_for(address).await?;
        let store_authority = (self.effect_host.turn_control_authority_owner()
            == crate::TurnControlAuthorityOwner::SessionStore)
            .then(|| store.turn_cancellation_authority())
            .flatten();
        let store_resolver = store_authority
            .as_ref()
            .map(|authority| authority.resolver());
        let resolver: &dyn AwaitEventResolver = store_resolver
            .as_ref()
            .map_or(self.effect_host.as_ref(), |resolver| resolver.as_ref());
        let key = terminal_key(resolver, address).await?;
        let resolution = resolver
            .await_await_event(&key, CancellationToken::new(), None)
            .await?;
        decode_terminal(address, resolution)
    }

    /// Await a terminal publication for at most `timeout`.
    ///
    /// Timing out only stops this caller's attachment. It never resolves or
    /// poisons the turn's first-writer-wins keyed promises.
    pub async fn await_terminal_with_timeout(
        &self,
        address: &TurnAddress,
        timeout: Duration,
    ) -> Result<TurnTerminal, RuntimeError> {
        tokio::time::timeout(timeout, self.await_terminal(address))
            .await
            .map_err(|_| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::TurnTerminalAwaitTimeout,
                    format!(
                        "timed out awaiting terminal for turn `{}` in session `{}` after {} ms",
                        address.turn_id,
                        address.session_id,
                        timeout.as_millis()
                    ),
                )
            })?
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "cancellation", rename_all = "snake_case")]
enum TurnGateTerminal {
    CancelRequested(TurnCancellationEvidence),
    CompletionSealed,
}

fn gate_resolution(value: TurnGateTerminal) -> Result<Resolution, RuntimeError> {
    serde_json::to_value(value)
        .map(Resolution::Ok)
        .map_err(|err| {
            RuntimeError::new(
                crate::RuntimeErrorCode::TurnCancelGateEncode,
                err.to_string(),
            )
        })
}

fn decode_gate(resolution: Resolution) -> Result<TurnGateTerminal, RuntimeError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value).map_err(|err| {
            RuntimeError::new(
                crate::RuntimeErrorCode::TurnCancelGateDecode,
                err.to_string(),
            )
        }),
        other => Err(RuntimeError::new(
            crate::RuntimeErrorCode::TurnCancelGateInvalidTerminal,
            format!("turn cancellation gate resolved with {other:?}"),
        )),
    }
}

fn terminal_resolution(value: &TurnTerminal) -> Result<Resolution, RuntimeError> {
    serde_json::to_value(value)
        .map(Resolution::Ok)
        .map_err(|err| {
            RuntimeError::new(crate::RuntimeErrorCode::TurnTerminalEncode, err.to_string())
        })
}

fn decode_terminal(
    address: &TurnAddress,
    resolution: Resolution,
) -> Result<TurnTerminal, RuntimeError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value).map_err(|err| {
            RuntimeError::new(
                crate::RuntimeErrorCode::TurnTerminalDecode,
                format!(
                    "invalid terminal result for turn `{}` in session `{}`: {err}",
                    address.turn_id, address.session_id
                ),
            )
        }),
        other => Err(RuntimeError::new(
            crate::RuntimeErrorCode::TurnTerminalInvalidResolution,
            format!(
                "terminal result for turn `{}` in session `{}` resolved with {other:?}",
                address.turn_id, address.session_id
            ),
        )),
    }
}

async fn cancel_gate_key(
    resolver: &dyn AwaitEventResolver,
    address: &TurnAddress,
) -> Result<AwaitEventKey, RuntimeError> {
    resolver
        .await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await
}

async fn terminal_key(
    resolver: &dyn AwaitEventResolver,
    address: &TurnAddress,
) -> Result<AwaitEventKey, RuntimeError> {
    resolver
        .await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )
        .await
}

async fn escalation_key(
    resolver: &dyn AwaitEventResolver,
    address: &TurnAddress,
) -> Result<AwaitEventKey, RuntimeError> {
    resolver
        .await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnCancelEscalation,
        )
        .await
}

/// Return the cancellation evidence that the immutable gate pair has accepted.
///
/// An after-step request owns the base gate. A later immediate request can own
/// the escalation gate, so readers must inspect both before projecting the
/// winner. Durable request rows are deliberately not consulted here: they are
/// only a projection of this authority.
async fn effective_cancel_evidence(
    resolver: &dyn AwaitEventResolver,
    address: &TurnAddress,
    base: TurnCancellationEvidence,
) -> Result<TurnCancellationEvidence, RuntimeError> {
    if base.mode.is_immediate() {
        return Ok(base);
    }
    let key = match escalation_key(resolver, address).await {
        Ok(key) => key,
        Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
            return Ok(base);
        }
        Err(err) => return Err(err),
    };
    let terminal = match resolver.peek_await_event(&key).await {
        Ok(terminal) => terminal,
        Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
            return Ok(base);
        }
        Err(err) => return Err(err),
    };
    match terminal.map(decode_gate).transpose()? {
        Some(TurnGateTerminal::CancelRequested(escalated)) => Ok(escalated),
        Some(TurnGateTerminal::CompletionSealed) | None => Ok(base),
    }
}

/// Close escalation admission for a turn-ending decision and return the
/// winner that was accepted before that closure.
///
/// The base gate remains the cancellation/completion authority. Its
/// `AfterStep` winner deliberately leaves a second first-writer promise open
/// while the turn is live so an `Immediate` request can escalate it. An
/// irreversible final commit must close that promise: otherwise a same-header
/// escalation can be accepted after the caller's row snapshot without
/// advancing the row revision, and a weaker disposition can publish after the
/// stronger request was acknowledged. Orphan repair cannot use this helper
/// until promise closure can be fenced by its session-execution lease.
async fn close_cancel_escalation(
    resolver: &dyn AwaitEventResolver,
    escalation_key: &AwaitEventKey,
    base: TurnCancellationEvidence,
) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
    if base.mode.is_immediate() {
        return Ok(Some(base));
    }
    let outcome = resolver
        .resolve_await_event(
            escalation_key,
            gate_resolution(TurnGateTerminal::CompletionSealed)?,
        )
        .await?;
    match outcome {
        ResolveOutcome::Accepted => Ok(Some(base)),
        ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
            TurnGateTerminal::CancelRequested(escalated) => Ok(Some(escalated)),
            TurnGateTerminal::CompletionSealed => Ok(Some(base)),
        },
        ResolveOutcome::UnknownOrRevoked => Ok(None),
    }
}

/// Per-execution bridge between the durable gate and the turn's internal
/// cancellation token.
///
/// Two evidence slots: `evidence` is the cancellation the turn honours (an
/// immediate request, an escalation, or an after-step request that reached
/// its boundary) and is what drives the token, the machine, and the commit.
/// `deferred` is an observed after-step request that is waiting for its step
/// boundary; it never reaches the machine, so protocol drivers keep running
/// the iteration to completion.
pub struct ActiveTurnControl {
    address: TurnAddress,
    cancel_key: AwaitEventKey,
    terminal_key: AwaitEventKey,
    escalation_key: AwaitEventKey,
    evidence: Mutex<Option<TurnCancellationEvidence>>,
    deferred: Mutex<Option<TurnCancellationEvidence>>,
    local_cancel_origin: TurnCancelOriginHint,
}

impl ActiveTurnControl {
    fn proposed_terminal(
        &self,
        locally_cancelled: bool,
        assembled: Option<TurnCancellationEvidence>,
    ) -> TurnGateTerminal {
        let remembered = self.evidence();
        let after_step_locally = self.local_cancel_origin.after_step_requested();
        if let Some(evidence) = remembered {
            TurnGateTerminal::CancelRequested(evidence)
        } else if locally_cancelled || after_step_locally {
            TurnGateTerminal::CancelRequested(match assembled {
                Some(mut evidence) => {
                    if evidence.origin.is_none() {
                        evidence.origin = self.local_cancel_origin.get();
                    }
                    evidence
                }
                None if locally_cancelled => self.internal_evidence(),
                None => TurnCancellationEvidence {
                    mode: TurnCancelMode::AfterStep,
                    ..self.internal_evidence()
                },
            })
        } else {
            TurnGateTerminal::CompletionSealed
        }
    }

    /// Materialize the exact closure operation before any promise is resolved.
    #[allow(clippy::too_many_arguments)]
    pub fn closure_authorization(
        &self,
        binding_id: impl Into<String>,
        admitted_scope: ExecutionScope,
        fence: &crate::SessionExecutionLeaseAuthority,
        observed_intent: TurnCancelIntentSnapshot,
        locally_cancelled: bool,
        assembled: Option<TurnCancellationEvidence>,
    ) -> Result<TurnCancelClosureAuthorization, RuntimeError> {
        let proposed_base = match self.proposed_terminal(locally_cancelled, assembled) {
            TurnGateTerminal::CancelRequested(evidence) => {
                TurnCancelClosureProposal::CancelRequested(evidence)
            }
            TurnGateTerminal::CompletionSealed => TurnCancelClosureProposal::CompletionSealed,
        };
        TurnCancelClosureAuthorization::new(
            self.address.clone(),
            binding_id,
            admitted_scope,
            self.cancel_key.clone(),
            self.escalation_key.clone(),
            self.terminal_key.clone(),
            proposed_base,
            observed_intent,
            fence,
        )
    }

    /// Finish an exact operation already persisted by the store. The promise
    /// outcomes remain authoritative, including a legitimate different winner.
    pub async fn settle_authorized(
        &self,
        resolver: &dyn AwaitEventResolver,
        authorization: &TurnCancelClosureAuthorization,
    ) -> Result<TurnCancelClosureSettlement, RuntimeError> {
        authorization.validate()?;
        if authorization.address() != self.address
            || authorization.cancel_key() != &self.cancel_key
            || authorization.escalation_key() != &self.escalation_key
            || authorization.terminal_key() != &self.terminal_key
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "pending turn cancellation closure does not match the active turn authority",
            ));
        }
        let proposed = match authorization.proposed_base().clone() {
            TurnCancelClosureProposal::CancelRequested(evidence) => {
                TurnGateTerminal::CancelRequested(evidence)
            }
            TurnCancelClosureProposal::CompletionSealed => TurnGateTerminal::CompletionSealed,
        };
        let effective_cancellation = self.settle_proposed(resolver, proposed).await?;
        let base_cancellation = self.read_settled_base_cancel_evidence(resolver).await?;
        Ok(TurnCancelClosureSettlement {
            authorization: authorization.clone(),
            base_cancellation,
            effective_cancellation,
        })
    }

    async fn settle_proposed(
        &self,
        resolver: &dyn AwaitEventResolver,
        proposed: TurnGateTerminal,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let remembered = self.evidence();
        let outcome = resolver
            .resolve_await_event(&self.cancel_key, gate_resolution(proposed.clone())?)
            .await?;
        let terminal = match outcome {
            ResolveOutcome::Accepted => proposed,
            ResolveOutcome::AlreadyResolved { terminal } => decode_gate(terminal)?,
            ResolveOutcome::UnknownOrRevoked => {
                return Err(RuntimeError::new(
                    crate::RuntimeErrorCode::TurnControlUnknownOrRevoked,
                    format!(
                        "turn `{}` in session `{}` was revoked before final commit",
                        self.address.turn_id, self.address.session_id
                    ),
                ));
            }
        };
        match terminal {
            TurnGateTerminal::CancelRequested(evidence) => {
                let Some(mut effective) =
                    close_cancel_escalation(resolver, &self.escalation_key, evidence).await?
                else {
                    return Err(RuntimeError::new(
                        crate::RuntimeErrorCode::TurnControlUnknownOrRevoked,
                        format!(
                            "turn `{}` in session `{}` was revoked while closing cancellation escalation before final commit",
                            self.address.turn_id, self.address.session_id
                        ),
                    ));
                };
                if let Some(mut cached) = remembered {
                    let honoured_after_step = cached.honoured_after_step.take();
                    if cached == effective {
                        effective.honoured_after_step = honoured_after_step;
                    }
                }
                self.remember(effective.clone());
                Ok(Some(effective))
            }
            TurnGateTerminal::CompletionSealed => Ok(None),
        }
    }

    /// Observe only the immutable base cancellation winner.
    ///
    /// Durable row projection uses this value rather than the effective
    /// (possibly escalated) cancellation so a delayed base projection can
    /// never replace the original policy acceptor with an escalation request.
    async fn peek_base_cancel_evidence(
        resolver: &dyn AwaitEventResolver,
        address: &TurnAddress,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let key = match cancel_gate_key(resolver, address).await {
            Ok(key) => key,
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        match resolver.peek_await_event(&key).await {
            Ok(Some(terminal)) => Ok(match decode_gate(terminal)? {
                TurnGateTerminal::CancelRequested(evidence) => Some(evidence),
                TurnGateTerminal::CompletionSealed => None,
            }),
            Ok(None) => Ok(None),
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Read the exact authorized base gate after settlement. Unlike the
    /// permissive recovery probe, absence or revocation is a refusal: a
    /// closure authorization cannot be consumed without an observable terminal
    /// from its bound promise owner.
    async fn read_settled_base_cancel_evidence(
        &self,
        resolver: &dyn AwaitEventResolver,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        match resolver.peek_await_event(&self.cancel_key).await {
            Ok(Some(terminal)) => Ok(match decode_gate(terminal)? {
                TurnGateTerminal::CancelRequested(evidence) => Some(evidence),
                TurnGateTerminal::CompletionSealed => None,
            }),
            Ok(None) => Err(RuntimeError::new(
                crate::RuntimeErrorCode::TurnControlUnknownOrRevoked,
                format!(
                    "turn `{}` in session `{}` has no settled base cancellation terminal",
                    self.address.turn_id, self.address.session_id
                ),
            )),
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                Err(RuntimeError::new(
                    crate::RuntimeErrorCode::TurnControlUnknownOrRevoked,
                    format!(
                        "turn `{}` in session `{}` lost its base cancellation terminal before closure settlement",
                        self.address.turn_id, self.address.session_id
                    ),
                ))
            }
            Err(err) => Err(err),
        }
    }

    pub async fn new(
        resolver: &dyn AwaitEventResolver,
        address: TurnAddress,
    ) -> Result<Self, RuntimeError> {
        address.validate()?;
        Ok(Self {
            cancel_key: cancel_gate_key(resolver, &address).await?,
            terminal_key: terminal_key(resolver, &address).await?,
            escalation_key: escalation_key(resolver, &address).await?,
            address,
            evidence: Mutex::new(None),
            deferred: Mutex::new(None),
            local_cancel_origin: TurnCancelOriginHint::default(),
        })
    }

    pub fn with_local_cancel_origin(mut self, origin: TurnCancelOriginHint) -> Self {
        self.local_cancel_origin = origin;
        self
    }

    /// Observe the current effective gate winner without closing escalation.
    /// Ordinary request projection uses this after a row-CAS refusal while the
    /// turn is still live and future `AfterStep` to `Immediate` escalation
    /// remains valid.
    async fn peek_effective_cancel_decision(
        resolver: &dyn AwaitEventResolver,
        address: &TurnAddress,
    ) -> Result<Option<crate::TurnCancelRepairDecision>, RuntimeError> {
        let key = match cancel_gate_key(resolver, address).await {
            Ok(key) => key,
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let resolution = match resolver.peek_await_event(&key).await {
            Ok(resolution) => resolution,
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let Some(terminal) = resolution.map(decode_gate).transpose()? else {
            return Ok(Some(crate::TurnCancelRepairDecision::NoCancellationIntent));
        };
        Ok(Some(match terminal {
            TurnGateTerminal::CancelRequested(evidence) => {
                crate::TurnCancelRepairDecision::CancellationWon(
                    effective_cancel_evidence(resolver, address, evidence).await?,
                )
            }
            TurnGateTerminal::CompletionSealed => {
                crate::TurnCancelRepairDecision::CancellationDidNotWin
            }
        }))
    }

    /// Observe the current gate pair for orphan-input repair without changing
    /// either promise. The caller's store transaction supplies the lease and
    /// intent fences; an expired repair owner must not close shared escalation
    /// authority before that transaction can reject it.
    pub(crate) async fn peek_orphan_repair_decision(
        resolver: &dyn AwaitEventResolver,
        address: &TurnAddress,
    ) -> Result<Option<crate::TurnCancelRepairDecision>, RuntimeError> {
        Self::peek_effective_cancel_decision(resolver, address).await
    }

    /// Live watch for a cancellation the turn must honour now.
    ///
    /// Returns immediate evidence as soon as the gate resolves with it. An
    /// after-step gate is deferred instead and the watch moves on to the
    /// escalation promise: only an escalation makes this return, so the
    /// caller fires the cooperative token exactly for immediate stops.
    pub async fn await_cancel(
        &self,
        resolver: &dyn AwaitEventResolver,
        stop_wait: CancellationToken,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let resolution = resolver
            .await_await_event(&self.cancel_key, stop_wait.clone(), None)
            .await?;
        match decode_gate(resolution)? {
            TurnGateTerminal::CancelRequested(evidence) if evidence.mode.is_immediate() => {
                self.remember(evidence.clone());
                Ok(Some(evidence))
            }
            TurnGateTerminal::CancelRequested(evidence) => {
                self.remember_deferred(evidence);
                let resolution = resolver
                    .await_await_event(&self.escalation_key, stop_wait, None)
                    .await?;
                match decode_gate(resolution)? {
                    TurnGateTerminal::CancelRequested(evidence) => {
                        self.remember(evidence.clone());
                        Ok(Some(evidence))
                    }
                    TurnGateTerminal::CompletionSealed => Ok(None),
                }
            }
            TurnGateTerminal::CompletionSealed => Ok(None),
        }
    }

    /// Journaled observation of the cancellation gate at one replay identity.
    ///
    /// Returns the evidence the turn honours at this identity. An after-step
    /// request is honoured at the start gate, the post-abort gate, and its own
    /// step boundary; at a mid-run peek it is deferred and only an escalation
    /// (peeked under a derived identity, so replay stays deterministic) makes
    /// the turn stop there.
    pub async fn observe_pending_cancel(
        &self,
        controller: &ScopedEffectController<'_>,
        identity: TurnCancelPeekIdentity,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let Some(gate) = self
            .peek(controller, identity.causal_identity(), &self.cancel_key)
            .await?
        else {
            return Ok(None);
        };
        let TurnGateTerminal::CancelRequested(evidence) = gate else {
            return Ok(None);
        };
        if evidence.mode.is_immediate() {
            self.remember(evidence.clone());
            return Ok(Some(evidence));
        }
        self.remember_deferred(evidence.clone());
        let escalation = self
            .peek(
                controller,
                identity.escalation_causal_identity(),
                &self.escalation_key,
            )
            .await?;
        if let Some(TurnGateTerminal::CancelRequested(escalated)) = escalation {
            self.remember(escalated.clone());
            return Ok(Some(escalated));
        }
        let Some(honoured_after_step) = identity.honours_after_step() else {
            return Ok(None);
        };
        let honoured = TurnCancellationEvidence {
            honoured_after_step,
            ..evidence
        };
        self.remember(honoured.clone());
        Ok(Some(honoured))
    }

    /// Resolve this turn's own gate with internal after-step evidence when a
    /// process-local stop asked for it. Runs before the step-boundary peek so
    /// the journaled peek, not process-local state, is what replay sees.
    pub async fn resolve_local_after_step(
        &self,
        resolver: &dyn AwaitEventResolver,
    ) -> Result<(), RuntimeError> {
        if !self.local_cancel_origin.after_step_requested() || self.evidence().is_some() {
            return Ok(());
        }
        let evidence = TurnCancellationEvidence {
            mode: TurnCancelMode::AfterStep,
            ..self.internal_evidence()
        };
        resolver
            .resolve_await_event(
                &self.cancel_key,
                gate_resolution(TurnGateTerminal::CancelRequested(evidence))?,
            )
            .await?;
        Ok(())
    }

    async fn peek(
        &self,
        controller: &ScopedEffectController<'_>,
        causal_identity: String,
        key: &AwaitEventKey,
    ) -> Result<Option<TurnGateTerminal>, RuntimeError> {
        // TurnAddress continues to route the cancellation promise in `key`;
        // the journaled observation belongs to the controller's admitted scope.
        // Keep the shipped foreground key only when the admitted Turn exactly
        // names this physical turn. Process, queue-drain, runtime-operation,
        // and follow-on Turn scopes can span physical turns, so their keys
        // fold in the captured address as well as the gate identity.
        let replay_key = turn_cancel_peek_replay_key(
            controller.execution_scope(),
            &self.address,
            &causal_identity,
        );
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(controller.execution_scope().clone(), replay_key)?,
            RuntimeAttribution {
                session_id: Some(self.address.session_id.clone()),
                turn_id: Some(self.address.turn_id.clone()),
                turn_index: None,
                protocol_iteration: None,
            },
            causal_identity.clone(),
        );
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::PeekAwaitEvent { key: key.clone() },
                ),
                RuntimeEffectLocalExecutor::unavailable(),
            )
            .await
            .map_err(|err| RuntimeError::new(err.code, err.message))?;
        let RuntimeEffectOutcome::PeekAwaitEvent { resolution } = outcome else {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::TurnControlPeekOutcome,
                format!("{causal_identity} returned a non-peek runtime effect outcome"),
            ));
        };
        resolution.map(decode_gate).transpose()
    }

    /// Settle the durable cancellation gate and close escalation before the
    /// turn commits.
    ///
    /// `assembled` is the evidence the executed turn already carries, when it
    /// stopped cancelled. Sealing that value rather than minting a fresh one
    /// is what keeps a single cancellation to a single request id: the
    /// evidence a host saw on the streamed `TurnOutcome` is the evidence the
    /// committed report carries. Closing the escalation promise is the
    /// authority boundary after which no later request can change the accepted
    /// winner or its undelivered-input disposition.
    pub async fn settle_before_commit(
        &self,
        resolver: &dyn AwaitEventResolver,
        locally_cancelled: bool,
        assembled: Option<TurnCancellationEvidence>,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let proposed = self.proposed_terminal(locally_cancelled, assembled);
        self.settle_proposed(resolver, proposed).await
    }

    pub async fn publish_terminal(
        &self,
        resolver: &dyn AwaitEventResolver,
        terminal: &TurnTerminal,
    ) -> Result<(), RuntimeError> {
        match resolver
            .resolve_await_event(&self.terminal_key, terminal_resolution(terminal)?)
            .await?
        {
            ResolveOutcome::Accepted | ResolveOutcome::AlreadyResolved { .. } => Ok(()),
            ResolveOutcome::UnknownOrRevoked => Err(RuntimeError::new(
                crate::RuntimeErrorCode::TurnTerminalUnknownOrRevoked,
                format!(
                    "terminal promise for turn `{}` in session `{}` was revoked",
                    self.address.turn_id, self.address.session_id
                ),
            )),
        }
    }

    /// The cancellation this turn honours, if one has been observed.
    pub fn evidence(&self) -> Option<TurnCancellationEvidence> {
        self.evidence.lock_recover().clone()
    }

    /// An observed after-step request still waiting for its step boundary.
    #[allow(dead_code)]
    pub fn deferred_evidence(&self) -> Option<TurnCancellationEvidence> {
        self.deferred.lock_recover().clone()
    }

    /// Evidence for an outcome that is already known to be cancelled, before
    /// the durable gate has settled: the observed request when one arrived,
    /// otherwise lash's own internal evidence. The settled evidence from
    /// [`Self::settle_before_commit`] is what the committed turn carries.
    pub fn evidence_or_internal(&self) -> TurnCancellationEvidence {
        self.evidence().unwrap_or_else(|| self.internal_evidence())
    }

    fn remember(&self, evidence: TurnCancellationEvidence) {
        *self.evidence.lock_recover() = Some(evidence);
    }

    fn remember_deferred(&self, evidence: TurnCancellationEvidence) {
        let mut deferred = self.deferred.lock_recover();
        if deferred.is_none() {
            *deferred = Some(evidence);
        }
    }

    fn internal_evidence(&self) -> TurnCancellationEvidence {
        TurnCancellationEvidence {
            origin: self.local_cancel_origin.get(),
            ..TurnCancellationEvidence::internal(&self.address.turn_id)
        }
    }
}

#[cfg(test)]
#[path = "turn_control/tests.rs"]
mod tests;
