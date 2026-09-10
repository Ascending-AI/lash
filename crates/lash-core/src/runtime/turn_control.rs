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
    Resolution, ResolveOutcome, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeError, RuntimeInvocation, RuntimeScope,
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
        self.execution_scope().validate()
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

    fn evidence(&self) -> TurnCancellationEvidence {
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
        }
    }

    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub async fn request_cancel(
        &self,
        request: TurnCancelRequest,
    ) -> Result<TurnCancelReceipt, RuntimeError> {
        request.validate()?;
        let key = match cancel_gate_key(self.effect_host.as_ref(), &request.address).await {
            Ok(key) => key,
            Err(err) if err.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Ok(TurnCancelReceipt {
                    outcome: TurnCancelOutcome::UnknownOrRevoked,
                    record: None,
                });
            }
            Err(err) => return Err(err),
        };
        let store = self.store_for(&request.address).await?;
        let record = store
            .record_turn_cancel_request(request.clone())
            .await
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
            })?;
        let evidence = record.request.evidence();
        let resolution = gate_resolution(TurnGateTerminal::CancelRequested(evidence.clone()))?;
        let outcome = match self
            .effect_host
            .resolve_await_event(&key, resolution)
            .await?
        {
            ResolveOutcome::Accepted => Ok(TurnCancelOutcome::Requested(evidence)),
            ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
                TurnGateTerminal::CancelRequested(existing)
                    if evidence.mode.is_stronger_than(existing.mode) =>
                {
                    self.escalate(&request.address, evidence, existing).await
                }
                TurnGateTerminal::CancelRequested(existing) => {
                    Ok(TurnCancelOutcome::AlreadyRequested(existing))
                }
                TurnGateTerminal::CompletionSealed => Ok(TurnCancelOutcome::CompletionWonRace),
            },
            ResolveOutcome::UnknownOrRevoked => Ok(TurnCancelOutcome::UnknownOrRevoked),
        }?;
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
        match &self.store {
            TurnWorkStore::Session { session_id, store } => {
                if session_id != &address.session_id {
                    return Err(RuntimeError::new(
                        crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                        format!(
                            "turn work driver is bound to session `{session_id}` and cannot address `{}`",
                            address.session_id
                        ),
                    ));
                }
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

    /// Upgrade an address whose first-writer gate holds a weaker request.
    ///
    /// The gate itself is immutable once written, so the stronger request
    /// rides a second reserved promise that the owner watches only after it
    /// observed a weaker gate. It is first-writer-wins too: a second stronger
    /// request reports the escalation that already won.
    async fn escalate(
        &self,
        address: &TurnAddress,
        evidence: TurnCancellationEvidence,
        existing: TurnCancellationEvidence,
    ) -> Result<TurnCancelOutcome, RuntimeError> {
        let key = escalation_key(self.effect_host.as_ref(), address).await?;
        let resolution = gate_resolution(TurnGateTerminal::CancelRequested(evidence.clone()))?;
        Ok(
            match self
                .effect_host
                .resolve_await_event(&key, resolution)
                .await?
            {
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
        if let Some(attach) = self.effect_host.turn_attach() {
            return attach.await_terminal(address).await;
        }
        let key = terminal_key(self.effect_host.as_ref(), address).await?;
        let resolution = self
            .effect_host
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
        controller: &dyn RuntimeEffectController,
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
        controller: &dyn RuntimeEffectController,
        causal_identity: String,
        key: &AwaitEventKey,
    ) -> Result<Option<TurnGateTerminal>, RuntimeError> {
        let invocation = RuntimeInvocation::effect(
            RuntimeScope {
                session_id: self.address.session_id.clone(),
                turn_id: Some(self.address.turn_id.clone()),
                turn_index: None,
                protocol_iteration: None,
            },
            causal_identity.clone(),
            RuntimeEffectKind::PeekAwaitEvent,
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

    /// Settle the durable cancellation gate before the turn commits.
    ///
    /// `assembled` is the evidence the executed turn already carries, when it
    /// stopped cancelled. Sealing that value rather than minting a fresh one
    /// is what keeps a single cancellation to a single request id: the
    /// evidence a host saw on the streamed `TurnOutcome` is the evidence the
    /// committed report carries.
    pub async fn settle_before_commit(
        &self,
        resolver: &dyn AwaitEventResolver,
        locally_cancelled: bool,
        assembled: Option<TurnCancellationEvidence>,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        if let Some(evidence) = self.evidence() {
            return Ok(Some(evidence));
        }
        // A process-local after-step stop that found no further step boundary
        // lands at the final commit: the last step ran to completion and the
        // turn stops here without dropping anything it produced.
        let after_step_locally = self.local_cancel_origin.after_step_requested();
        let proposed = if locally_cancelled || after_step_locally {
            TurnGateTerminal::CancelRequested(match assembled {
                // A lash-originated stop already names itself. The
                // process-local origin hint, when a host entry point supplied
                // one, is the only fact the executing machine cannot know.
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
        };
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
                self.remember(evidence.clone());
                Ok(Some(evidence))
            }
            TurnGateTerminal::CompletionSealed => Ok(None),
        }
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
mod tests {
    use super::*;
    use crate::{NativeEffectHost, TurnFinish, TurnStop};

    fn address(label: &str) -> TurnAddress {
        TurnAddress::new(
            format!("turn-control-{label}-{}", uuid::Uuid::new_v4()),
            "turn-a",
        )
    }

    fn request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
        TurnCancelRequest::new(address, request_id, Some("user".to_string()))
            .with_reason("stop button")
    }

    #[test]
    fn legacy_cancel_request_without_disposition_defaults_to_defer() {
        let decoded: TurnCancelRequest = serde_json::from_value(serde_json::json!({
            "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
            "request_id": "legacy-request"
        }))
        .expect("decode a pre-disposition cancel request");
        assert_eq!(decoded.undelivered, TurnCancelDisposition::Defer);
        assert!(
            serde_json::to_value(decoded)
                .expect("encode defaulted request")
                .get("undelivered")
                .is_none(),
            "the legacy Defer default stays sparse on the durable row"
        );
    }

    #[tokio::test]
    async fn cancel_before_start_duplicate_and_terminal_attach() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("before-start");

        let first = driver
            .request_cancel(request(address.clone(), "request-1"))
            .await
            .expect("request cancellation");
        let evidence = match first.outcome {
            TurnCancelOutcome::Requested(evidence) => evidence,
            other => panic!("expected requested, got {other:?}"),
        };
        assert_eq!(evidence.request_id, "request-1");

        let duplicate = driver
            .request_cancel(request(address.clone(), "request-2"))
            .await
            .expect("duplicate cancellation");
        assert!(matches!(
            duplicate.outcome,
            TurnCancelOutcome::AlreadyRequested(TurnCancellationEvidence { ref request_id, .. })
                if request_id == "request-1"
        ));

        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let observed = active
            .settle_before_commit(host.as_ref(), false, None)
            .await
            .expect("settle")
            .expect("cancellation won");
        assert_eq!(observed, evidence);
        let terminal = TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence: observed }),
            session_revision: Some(7),
        };
        active
            .publish_terminal(host.as_ref(), &terminal)
            .await
            .expect("publish terminal");
        let attached = driver
            .await_terminal(&address)
            .await
            .expect("attach terminal");
        assert!(matches!(
            attached,
            TurnTerminal::Committed {
                outcome: TurnOutcome::Stopped(TurnStop::Cancelled { .. }),
                session_revision: Some(7),
            }
        ));
    }

    #[tokio::test]
    async fn settle_seals_the_assembled_evidence_instead_of_minting_a_second_id() {
        // A provider abort classified as cancelled arrives with evidence the
        // sans-IO machine already put on the streamed outcome. Sealing that
        // value is what keeps one cancellation to one request id: minting
        // `internal:{turn_id}` here would hand the host a second identity for
        // the same fact.
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("assembled");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let assembled = TurnCancellationEvidence::internal("provider-cancelled:3");

        let settled = active
            .settle_before_commit(host.as_ref(), true, Some(assembled.clone()))
            .await
            .expect("settle")
            .expect("a locally cancelled turn settles cancelled");
        assert_eq!(settled, assembled);
        assert_ne!(settled, active.internal_evidence());

        // The durable gate carries the same identity, so a later requester and
        // a replayed owner both read the value the turn streamed.
        let late = driver
            .request_cancel(request(address, "late-request"))
            .await
            .expect("late cancellation");
        assert!(matches!(
            late.outcome,
            TurnCancelOutcome::AlreadyRequested(ref evidence) if *evidence == assembled
        ));
    }

    #[tokio::test]
    async fn concurrent_completion_seal_vs_cancel_is_first_writer_wins() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("race");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

        let (seal, cancel) = tokio::join!(
            active.settle_before_commit(host.as_ref(), false, None),
            driver.request_cancel(request(address, "race-request")),
        );
        match (seal.expect("seal"), cancel.expect("cancel").outcome) {
            (None, TurnCancelOutcome::CompletionWonRace) => {}
            (Some(evidence), TurnCancelOutcome::Requested(requested)) => {
                assert_eq!(evidence, requested);
            }
            other => panic!("inconsistent gate race result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovered_owner_observes_pending_cancel_after_control_recreation() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("replay");
        let requested = driver
            .request_cancel(request(address.clone(), "request-before-replay"))
            .await
            .expect("request cancellation");
        let expected = match requested.outcome {
            TurnCancelOutcome::Requested(evidence) => evidence,
            other => panic!("expected requested, got {other:?}"),
        };

        let scoped = host
            .scoped(address.execution_scope())
            .expect("scope recovered turn controller");
        let recovered = ActiveTurnControl::new(host.as_ref(), address)
            .await
            .expect("recreate active control under the recovered owner");
        let observed = recovered
            .observe_pending_cancel(scoped.controller(), TurnCancelPeekIdentity::StartGate)
            .await
            .expect("read recovered turn start gate")
            .expect("pending cancellation is visible before recovered effects");
        assert_eq!(observed, expected);
        let settled = recovered
            .settle_before_commit(host.as_ref(), false, None)
            .await
            .expect("settle recovered turn")
            .expect("pending cancellation survives owner loss");
        assert_eq!(settled, expected);
    }

    #[tokio::test]
    async fn turn_control_is_exact_scope_and_excluded_from_wait_cancel_sweep() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address_a = address("scope");
        let address_b = TurnAddress::new(&address_a.session_id, "turn-b");
        let address_future = TurnAddress::new(&address_a.session_id, "turn-future");

        driver
            .request_cancel(request(address_a.clone(), "request-a"))
            .await
            .expect("cancel a");

        let tool_key = host
            .await_event_key(
                &ExecutionScope::turn(&address_a.session_id, "tool-turn"),
                AwaitEventWaitIdentity::tool_completion("tool-call"),
            )
            .await
            .expect("tool key");
        let tool_host = host.clone();
        let tool_wait = crate::task::spawn(async move {
            tool_host
                .await_await_event(&tool_key, CancellationToken::new(), None)
                .await
        });
        tokio::task::yield_now().await;
        host.cancel_await_events_for_session(&address_a.session_id)
            .await
            .expect("cancel durable waits");
        assert!(matches!(
            tool_wait
                .await
                .expect("tool wait task")
                .expect("tool resolution"),
            Resolution::Cancelled
        ));

        assert!(matches!(
            driver
                .request_cancel(request(address_a.clone(), "request-a-duplicate"))
                .await
                .expect("duplicate a")
                .outcome,
            TurnCancelOutcome::AlreadyRequested(_)
        ));
        assert!(matches!(
            driver
                .request_cancel(request(address_b, "request-b"))
                .await
                .expect("cancel b")
                .outcome,
            TurnCancelOutcome::Requested(_)
        ));
        assert!(matches!(
            driver
                .request_cancel(request(address_future, "request-future"))
                .await
                .expect("cancel future")
                .outcome,
            TurnCancelOutcome::Requested(_)
        ));
    }

    #[tokio::test]
    async fn session_deletion_revokes_control_promises() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("revoke");
        host.revoke_await_events_for_session(&address.session_id)
            .await
            .expect("revoke session");
        assert!(matches!(
            driver
                .request_cancel(request(address, "request-after-delete"))
                .await
                .expect("revoked outcome")
                .outcome,
            TurnCancelOutcome::UnknownOrRevoked
        ));
    }

    #[tokio::test]
    async fn terminal_attachment_timeout_does_not_poison_later_publication() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("terminal-timeout");
        let error = driver
            .await_terminal_with_timeout(&address, Duration::from_millis(1))
            .await
            .expect_err("unpublished terminal must time out");
        assert_eq!(error.code.as_str(), "turn_terminal_await_timeout");

        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control after timed-out attach");
        active
            .settle_before_commit(host.as_ref(), false, None)
            .await
            .expect("seal after timed-out attach");
        active
            .publish_terminal(
                host.as_ref(),
                &TurnTerminal::Committed {
                    outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                        text: "done".to_string(),
                    }),
                    session_revision: None,
                },
            )
            .await
            .expect("publish after timed-out attach");
        assert!(matches!(
            driver.await_terminal(&address).await.expect("late attach"),
            TurnTerminal::Committed {
                outcome: TurnOutcome::Finished(_),
                ..
            }
        ));
    }

    #[test]
    fn local_cancel_origin_hint_preserves_first_origin() {
        let hint = TurnCancelOriginHint::default();
        assert!(!hint.was_set());
        hint.set(Some("shutdown".to_string()));
        hint.set(Some("user".to_string()));

        assert!(hint.was_set());
        assert_eq!(hint.get().as_deref(), Some("shutdown"));
    }

    #[test]
    fn local_cancel_origin_hint_preserves_explicit_absence() {
        let hint = TurnCancelOriginHint::default();
        assert!(!hint.was_set());
        hint.set(None);
        hint.set(Some("user".to_string()));

        assert!(hint.was_set());
        assert_eq!(hint.get(), None);
    }

    #[test]
    fn installed_originless_token_does_not_block_a_later_registry_origin() {
        let hint = TurnCancelOriginHint::default();
        hint.configure_local_token(None);

        assert!(!hint.was_set());

        hint.set(Some("user".to_string()));
        assert_eq!(hint.get().as_deref(), Some("user"));
    }

    #[test]
    fn observed_registry_origin_wins_over_configured_token_origin() {
        let hint = TurnCancelOriginHint::default();
        hint.configure_local_token(Some("shutdown".to_string()));
        assert_eq!(hint.get().as_deref(), Some("shutdown"));

        hint.set(Some("user".to_string()));
        assert_eq!(hint.get().as_deref(), Some("user"));
    }

    #[test]
    fn terminal_success_has_no_cancellation_evidence() {
        let terminal = TurnTerminal::Committed {
            outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: "done".to_string(),
            }),
            session_revision: None,
        };
        let encoded = terminal_resolution(&terminal).expect("encode terminal");
        assert!(matches!(encoded, Resolution::Ok(_)));
    }

    #[test]
    fn legacy_cancel_request_without_mode_decodes_as_immediate() {
        let decoded: TurnCancelRequest = serde_json::from_value(serde_json::json!({
            "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
            "request_id": "legacy-request",
            "origin": "user",
            "reason": "stop button",
            "undelivered": "drop"
        }))
        .expect("decode a pre-mode cancel request");
        assert_eq!(decoded.mode, TurnCancelMode::Immediate);
        assert_eq!(decoded.undelivered, TurnCancelDisposition::Drop);
        let encoded = serde_json::to_value(&decoded).expect("encode defaulted request");
        assert!(
            encoded.get("mode").is_none(),
            "the Immediate default stays sparse on the durable row: {encoded}"
        );
    }

    #[test]
    fn legacy_cancellation_evidence_without_mode_decodes_as_immediate() {
        let decoded: TurnCancellationEvidence = serde_json::from_value(serde_json::json!({
            "request_id": "legacy-request",
            "origin": "user"
        }))
        .expect("decode pre-mode evidence");
        assert_eq!(decoded.mode, TurnCancelMode::Immediate);
        assert_eq!(decoded.honoured_after_step, None);
        let encoded = serde_json::to_value(&decoded).expect("encode evidence");
        assert!(encoded.get("mode").is_none());
        assert!(encoded.get("honoured_after_step").is_none());
    }

    #[test]
    fn after_step_request_and_evidence_round_trip_the_mode() {
        let request = request(address("mode"), "request-1").mode(TurnCancelMode::AfterStep);
        let encoded = serde_json::to_value(&request).expect("encode request");
        assert_eq!(encoded["mode"], serde_json::json!("after_step"));
        let decoded: TurnCancelRequest =
            serde_json::from_value(encoded).expect("decode after-step request");
        assert_eq!(decoded, request);
        let evidence = TurnCancellationEvidence {
            honoured_after_step: Some(3),
            ..decoded.evidence()
        };
        assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
        let encoded = serde_json::to_value(&evidence).expect("encode evidence");
        assert_eq!(encoded["mode"], serde_json::json!("after_step"));
        assert_eq!(encoded["honoured_after_step"], serde_json::json!(3));
        let decoded: TurnCancellationEvidence =
            serde_json::from_value(encoded).expect("decode evidence");
        assert_eq!(decoded, evidence);
    }

    #[test]
    fn cancel_mode_ordering_only_lets_immediate_escalate_after_step() {
        assert!(TurnCancelMode::Immediate.is_stronger_than(TurnCancelMode::AfterStep));
        assert!(!TurnCancelMode::AfterStep.is_stronger_than(TurnCancelMode::Immediate));
        assert!(!TurnCancelMode::Immediate.is_stronger_than(TurnCancelMode::Immediate));
        assert!(!TurnCancelMode::AfterStep.is_stronger_than(TurnCancelMode::AfterStep));
        assert!(TurnCancelMode::default().is_immediate());
    }

    #[test]
    fn peek_identities_are_replay_deterministic_and_name_their_escalation() {
        let after_step = TurnCancelPeekIdentity::AfterStep {
            protocol_iteration: 4,
        };
        assert_eq!(after_step.causal_identity(), "turn_cancel.after_step.4");
        assert_eq!(
            after_step.escalation_causal_identity(),
            "turn_cancel.escalation.after_step.4"
        );
        assert_eq!(after_step.honours_after_step(), Some(Some(4)));
        assert_eq!(
            TurnCancelPeekIdentity::StartGate.honours_after_step(),
            Some(None)
        );
        assert_eq!(
            TurnCancelPeekIdentity::PostAbortGate.honours_after_step(),
            Some(None)
        );
        assert_eq!(
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 0
            }
            .honours_after_step(),
            None
        );
        assert_eq!(
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 0
            }
            .escalation_causal_identity(),
            "turn_cancel.escalation.after_llm.0"
        );
    }

    #[tokio::test]
    async fn after_step_request_is_deferred_until_immediate_escalates_it() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("escalate");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

        let stop = driver
            .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
            .await
            .expect("after-step request");
        let stop_evidence = match stop.outcome {
            TurnCancelOutcome::Requested(evidence) => evidence,
            other => panic!("expected requested, got {other:?}"),
        };
        assert_eq!(stop_evidence.mode, TurnCancelMode::AfterStep);

        // Mid-model-call observation (AfterLlm never honours after-step): the
        // request is remembered as deferred, never as effective evidence.
        let observed = active
            .observe_pending_cancel(
                host.as_ref(),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 0,
                },
            )
            .await
            .expect("peek after llm");
        assert_eq!(observed, None);
        assert_eq!(active.evidence(), None);
        assert_eq!(active.deferred_evidence(), Some(stop_evidence.clone()));

        let again = driver
            .request_cancel(request(address.clone(), "stop-2").mode(TurnCancelMode::AfterStep))
            .await
            .expect("second after-step request");
        assert!(matches!(
            again.outcome,
            TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "stop-1"
        ));

        let abort = driver
            .request_cancel(request(address.clone(), "abort-1"))
            .await
            .expect("escalation");
        let abort_evidence = match abort.outcome {
            TurnCancelOutcome::Escalated(evidence) => evidence,
            other => panic!("expected escalated, got {other:?}"),
        };
        assert_eq!(abort_evidence.request_id, "abort-1");
        assert_eq!(abort_evidence.mode, TurnCancelMode::Immediate);

        let repeat = driver
            .request_cancel(request(address.clone(), "abort-2"))
            .await
            .expect("repeated escalation");
        assert!(matches!(
            repeat.outcome,
            TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "abort-1"
        ));

        let observed = active
            .observe_pending_cancel(
                host.as_ref(),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 1,
                },
            )
            .await
            .expect("peek after escalation");
        assert_eq!(observed, Some(abort_evidence.clone()));
        assert_eq!(active.evidence(), Some(abort_evidence.clone()));

        let settled = active
            .settle_before_commit(host.as_ref(), true, None)
            .await
            .expect("settle");
        assert_eq!(settled, Some(abort_evidence));
    }

    #[tokio::test]
    async fn after_step_request_is_honoured_at_the_step_boundary_with_its_iteration() {
        let host = Arc::new(NativeEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("boundary");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        driver
            .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
            .await
            .expect("after-step request");
        assert_eq!(
            active
                .observe_pending_cancel(
                    host.as_ref(),
                    TurnCancelPeekIdentity::AfterLlm {
                        protocol_iteration: 2,
                    },
                )
                .await
                .expect("peek after llm"),
            None
        );
        let honoured = active
            .observe_pending_cancel(
                host.as_ref(),
                TurnCancelPeekIdentity::AfterStep {
                    protocol_iteration: 2,
                },
            )
            .await
            .expect("peek at boundary")
            .expect("after-step lands at the boundary");
        assert_eq!(honoured.request_id, "stop-1");
        assert_eq!(honoured.mode, TurnCancelMode::AfterStep);
        assert_eq!(honoured.honoured_after_step, Some(2));
        assert_eq!(active.evidence(), Some(honoured.clone()));
        let settled = active
            .settle_before_commit(host.as_ref(), false, None)
            .await
            .expect("settle");
        assert_eq!(settled, Some(honoured));
        // Once the gate holds after-step evidence, a later Immediate request
        // still escalates the record; the owner is what decides whether it
        // is already past its boundary.
        let late = driver
            .request_cancel(request(address.clone(), "abort-late"))
            .await
            .expect("late escalation");
        assert!(matches!(late.outcome, TurnCancelOutcome::Escalated(_)));
    }

    #[tokio::test]
    async fn local_after_step_stop_resolves_the_own_gate_and_lands_at_commit() {
        let host = Arc::new(NativeEffectHost::default());
        let address = address("local-after-step");
        let hint = TurnCancelOriginHint::default();
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control")
            .with_local_cancel_origin(hint.clone());
        hint.request_after_step(Some("shutdown".to_string()));
        assert!(hint.after_step_requested());
        active
            .resolve_local_after_step(host.as_ref())
            .await
            .expect("resolve own gate");
        let honoured = active
            .observe_pending_cancel(
                host.as_ref(),
                TurnCancelPeekIdentity::AfterStep {
                    protocol_iteration: 0,
                },
            )
            .await
            .expect("peek at boundary")
            .expect("local after-step lands at the boundary");
        assert_eq!(honoured.mode, TurnCancelMode::AfterStep);
        assert_eq!(honoured.origin.as_deref(), Some("shutdown"));
        assert_eq!(honoured.honoured_after_step, Some(0));
        assert_eq!(honoured.request_id, format!("internal:{}", address.turn_id));

        // Without a boundary the flag still settles the final commit as an
        // after-step stop.
        let commit_only = ActiveTurnControl::new(host.as_ref(), self::address("local-commit"))
            .await
            .expect("active control")
            .with_local_cancel_origin({
                let hint = TurnCancelOriginHint::default();
                hint.request_after_step(None);
                hint
            });
        let settled = commit_only
            .settle_before_commit(host.as_ref(), false, None)
            .await
            .expect("settle")
            .expect("after-step flag settles as cancelled");
        assert_eq!(settled.mode, TurnCancelMode::AfterStep);
        assert_eq!(settled.honoured_after_step, None);
    }
}
