//! Durable turn-cancellation vocabulary.
//!
//! The cancellation records a store persists and the closure authorization a
//! commit settles against. The turn-control host, its cancellation tokens and
//! the effect-executor gate stay in `lash-core`.

use crate::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, RuntimeError, SessionId,
    TurnCancelDisposition, TurnCancelMode, TurnCancellationEvidence, TurnId,
};
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::Mutex;

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

    pub fn validate(&self) -> Result<(), RuntimeError> {
        Ok(self.execution_scope().validate()?)
    }
}
/// One undelivered active-turn input affected by cancellation repair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnCancelAffectedInput {
    pub input_id: crate::InputId,
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
/// slot bound to its admitted execution scope. A successor may finish the exact
/// promise operation after takeover. Final publication uses head CAS and durable
/// cancellation facts, independent of advisory lease liveness or generation;
/// activation's orphan repair retains its current execution fence.
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
            || !crate::turn_control_binding::binding_id_admits_scope(
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

    /// Whether this request is a timing escalation of the `accepted` one.
    ///
    /// Only a repeat that already agrees with the accepted undelivered-input
    /// disposition can escalate. A repeat that disagrees is a policy conflict
    /// the authoritative gate refuses, so it must not advance the durable
    /// intent revision either: a refused request has no durable effect at all,
    /// and a store that treated it as an escalation would invalidate the
    /// live owner's closure CAS on behalf of a request that never won.
    #[must_use]
    pub fn escalates(&self, accepted: &Self) -> bool {
        self.undelivered == accepted.undelivered && self.mode.is_stronger_than(accepted.mode)
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.address.validate()?;
        if self.request_id.trim().is_empty() {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "turn cancellation requires a non-empty request id",
            ));
        }
        Ok(())
    }

    pub fn evidence(&self) -> TurnCancellationEvidence {
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

fn turn_cancel_disposition_is_defer(disposition: &TurnCancelDisposition) -> bool {
    matches!(disposition, TurnCancelDisposition::Defer)
}

/// Shared origin hint for a process-local cancellation token.
///
/// The outer option records whether a local entry point supplied a hint; the
/// inner option is the opaque host origin, which may intentionally be absent.
/// It is not a durable cancellation request and must not be used as
/// authorization.
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

    pub fn after_step_requested(&self) -> bool {
        self.state.lock_recover().after_step_requested
    }

    pub fn get(&self) -> Option<String> {
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
    pub fn configure_local_token(&self, origin: Option<String>) {
        let mut state = self.state.lock_recover();
        state.configured_origin = Some(origin);
    }
}
