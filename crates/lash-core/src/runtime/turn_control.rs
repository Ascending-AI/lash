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
pub(crate) enum TurnCancelPeekIdentity {
    StartGate,
    PostAbortGate,
}

impl TurnCancelPeekIdentity {
    fn as_str(self) -> &'static str {
        match self {
            Self::StartGate => "turn_cancel.start_gate",
            Self::PostAbortGate => "turn_cancel.post_abort_gate",
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
    pub session_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation_id: Option<crate::IncarnationId>,
}

impl TurnAddress {
    pub fn new(session_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            incarnation_id: None,
        }
    }

    pub fn new_incarnation(
        session_id: impl Into<String>,
        incarnation_id: crate::IncarnationId,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            incarnation_id: Some(incarnation_id),
        }
    }

    pub fn new_for_lifetime(
        session_id: impl Into<String>,
        lifetime: &crate::SessionLifetime,
        turn_id: impl Into<String>,
    ) -> Self {
        match lifetime.as_durable() {
            Some(incarnation_id) => {
                Self::new_incarnation(session_id, incarnation_id.clone(), turn_id)
            }
            None => Self::new(session_id, turn_id),
        }
    }

    pub fn execution_scope(&self) -> ExecutionScope {
        match self.incarnation_id.as_ref() {
            Some(incarnation_id) => ExecutionScope::turn_incarnation(
                &self.session_id,
                incarnation_id.clone(),
                &self.turn_id,
            ),
            None => ExecutionScope::turn(&self.session_id, &self.turn_id),
        }
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
    origin: Arc<Mutex<Option<Option<String>>>>,
}

impl TurnCancelOriginHint {
    pub fn set(&self, origin: Option<String>) {
        let mut hint = self.origin.lock().expect("turn cancel origin hint lock");
        if hint.is_none() {
            *hint = Some(origin);
        }
    }

    pub(crate) fn get(&self) -> Option<String> {
        self.origin
            .lock()
            .expect("turn cancel origin hint lock")
            .clone()
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancellationEvidence {
    pub request_id: String,
    /// Opaque host-domain data. Lash records and returns it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        self.address.validate()?;
        if self.request_id.trim().is_empty() {
            return Err(RuntimeError::new(
                "invalid_turn_cancel_request",
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
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "cancellation", rename_all = "snake_case")]
pub enum TurnCancelOutcome {
    Requested(TurnCancellationEvidence),
    AlreadyRequested(TurnCancellationEvidence),
    CompletionWonRace,
    UnknownOrRevoked,
}

/// Result of addressing one turn-cancellation gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelReceipt {
    pub outcome: TurnCancelOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TurnTerminal {
    Committed {
        outcome: TurnOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancellation: Option<TurnCancellationEvidence>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_revision: Option<u64>,
    },
    Failed {
        error: ErrorEnvelope,
    },
}

/// Backend-specific terminal attachment for a foreground turn.
#[async_trait::async_trait]
pub trait TurnAttach: Send + Sync {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError>;
}

/// Cooperative, exact-turn control compiled onto Lash's keyed-promise seam.
///
/// `Requested` means the cancellation request won this driver's keyed-promise
/// gate. On an inline effect host that promise is process-local, so the driver
/// only reaches owners in the same OS process. A durable effect-host deployment
/// is required for another process or replayed owner to observe the request.
/// The returned [`TurnCancelReceipt`] reports only the cancellation outcome.
/// Hosts must use their configured effect-host topology when deciding whether
/// an inline receipt is cross-process proof.
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
    attach: Option<Arc<dyn TurnAttach>>,
}

impl TurnWorkDriver {
    pub fn new(effect_host: Arc<dyn EffectHost>) -> Self {
        Self {
            effect_host,
            attach: None,
        }
    }

    pub fn with_attach(mut self, attach: Arc<dyn TurnAttach>) -> Self {
        self.attach = Some(attach);
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
        let key = match cancel_gate_key(self.effect_host.as_ref(), &request.address).await {
            Ok(key) => key,
            Err(err) if err.code.as_str() == "await_event_unknown_or_revoked" => {
                return Ok(TurnCancelReceipt {
                    outcome: TurnCancelOutcome::UnknownOrRevoked,
                });
            }
            Err(err) => return Err(err),
        };
        let evidence = request.evidence();
        let resolution = gate_resolution(TurnGateTerminal::CancelRequested(evidence.clone()))?;
        let outcome = match self
            .effect_host
            .resolve_await_event(&key, resolution)
            .await?
        {
            ResolveOutcome::Accepted => Ok(TurnCancelOutcome::Requested(evidence)),
            ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
                TurnGateTerminal::CancelRequested(existing) => {
                    Ok(TurnCancelOutcome::AlreadyRequested(existing))
                }
                TurnGateTerminal::CompletionSealed => Ok(TurnCancelOutcome::CompletionWonRace),
            },
            ResolveOutcome::UnknownOrRevoked => Ok(TurnCancelOutcome::UnknownOrRevoked),
        }?;
        Ok(TurnCancelReceipt { outcome })
    }

    pub async fn await_terminal(
        &self,
        address: &TurnAddress,
    ) -> Result<TurnTerminal, RuntimeError> {
        address.validate()?;
        if let Some(attach) = self.attach.as_ref() {
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
                    "turn_terminal_await_timeout",
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
        .map_err(|err| RuntimeError::new("turn_cancel_gate_encode", err.to_string()))
}

fn decode_gate(resolution: Resolution) -> Result<TurnGateTerminal, RuntimeError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value)
            .map_err(|err| RuntimeError::new("turn_cancel_gate_decode", err.to_string())),
        other => Err(RuntimeError::new(
            "turn_cancel_gate_invalid_terminal",
            format!("turn cancellation gate resolved with {other:?}"),
        )),
    }
}

fn terminal_resolution(value: &TurnTerminal) -> Result<Resolution, RuntimeError> {
    serde_json::to_value(value)
        .map(Resolution::Ok)
        .map_err(|err| RuntimeError::new("turn_terminal_encode", err.to_string()))
}

fn decode_terminal(
    address: &TurnAddress,
    resolution: Resolution,
) -> Result<TurnTerminal, RuntimeError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value).map_err(|err| {
            RuntimeError::new(
                "turn_terminal_decode",
                format!(
                    "invalid terminal result for turn `{}` in session `{}`: {err}",
                    address.turn_id, address.session_id
                ),
            )
        }),
        other => Err(RuntimeError::new(
            "turn_terminal_invalid_resolution",
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

/// Per-execution bridge between the durable gate and the turn's internal
/// cancellation token.
pub(crate) struct ActiveTurnControl {
    address: TurnAddress,
    cancel_key: AwaitEventKey,
    terminal_key: AwaitEventKey,
    evidence: Mutex<Option<TurnCancellationEvidence>>,
    local_cancel_origin: TurnCancelOriginHint,
}

impl ActiveTurnControl {
    pub(crate) async fn new(
        resolver: &dyn AwaitEventResolver,
        address: TurnAddress,
    ) -> Result<Self, RuntimeError> {
        address.validate()?;
        Ok(Self {
            cancel_key: cancel_gate_key(resolver, &address).await?,
            terminal_key: terminal_key(resolver, &address).await?,
            address,
            evidence: Mutex::new(None),
            local_cancel_origin: TurnCancelOriginHint::default(),
        })
    }

    pub(crate) fn with_local_cancel_origin(mut self, origin: TurnCancelOriginHint) -> Self {
        self.local_cancel_origin = origin;
        self
    }

    pub(crate) async fn await_cancel(
        &self,
        resolver: &dyn AwaitEventResolver,
        stop_wait: CancellationToken,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let resolution = resolver
            .await_await_event(&self.cancel_key, stop_wait, None)
            .await?;
        match decode_gate(resolution)? {
            TurnGateTerminal::CancelRequested(evidence) => {
                self.remember(evidence.clone());
                Ok(Some(evidence))
            }
            TurnGateTerminal::CompletionSealed => Ok(None),
        }
    }

    pub(crate) async fn observe_pending_cancel(
        &self,
        controller: &dyn RuntimeEffectController,
        identity: TurnCancelPeekIdentity,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let causal_identity = identity.as_str();
        let invocation = RuntimeInvocation::effect(
            RuntimeScope {
                session_id: self.address.session_id.clone(),
                turn_id: Some(self.address.turn_id.clone()),
                turn_index: None,
                protocol_iteration: None,
            },
            causal_identity,
            RuntimeEffectKind::PeekAwaitEvent,
            causal_identity,
        );
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::PeekAwaitEvent {
                        key: self.cancel_key.clone(),
                    },
                ),
                RuntimeEffectLocalExecutor::unavailable(),
            )
            .await
            .map_err(|err| RuntimeError::new(err.code, err.message))?;
        let RuntimeEffectOutcome::PeekAwaitEvent { resolution } = outcome else {
            return Err(RuntimeError::new(
                "turn_control_peek_outcome",
                format!("{causal_identity} returned a non-peek runtime effect outcome"),
            ));
        };
        let Some(resolution) = resolution else {
            return Ok(None);
        };
        match decode_gate(resolution)? {
            TurnGateTerminal::CancelRequested(evidence) => {
                self.remember(evidence.clone());
                Ok(Some(evidence))
            }
            TurnGateTerminal::CompletionSealed => Ok(None),
        }
    }

    pub(crate) async fn settle_before_commit(
        &self,
        resolver: &dyn AwaitEventResolver,
        locally_cancelled: bool,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        if let Some(evidence) = self.evidence() {
            return Ok(Some(evidence));
        }
        let proposed = if locally_cancelled {
            TurnGateTerminal::CancelRequested(self.internal_evidence())
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
                    "turn_control_unknown_or_revoked",
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

    pub(crate) async fn publish_terminal(
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
                "turn_terminal_unknown_or_revoked",
                format!(
                    "terminal promise for turn `{}` in session `{}` was revoked",
                    self.address.turn_id, self.address.session_id
                ),
            )),
        }
    }

    pub(crate) fn evidence(&self) -> Option<TurnCancellationEvidence> {
        self.evidence
            .lock()
            .expect("turn cancellation evidence lock")
            .clone()
    }

    fn remember(&self, evidence: TurnCancellationEvidence) {
        *self
            .evidence
            .lock()
            .expect("turn cancellation evidence lock") = Some(evidence);
    }

    fn internal_evidence(&self) -> TurnCancellationEvidence {
        TurnCancellationEvidence {
            request_id: format!("internal:{}", self.address.turn_id),
            origin: self.local_cancel_origin.get(),
            reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InlineEffectHost, TurnFinish, TurnStop};

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

    #[tokio::test]
    async fn cancel_before_start_duplicate_and_terminal_attach() {
        let host = Arc::new(InlineEffectHost::default());
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
            .settle_before_commit(host.as_ref(), false)
            .await
            .expect("settle")
            .expect("cancellation won");
        assert_eq!(observed, evidence);
        let terminal = TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled),
            cancellation: Some(observed),
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
                outcome: TurnOutcome::Stopped(TurnStop::Cancelled),
                cancellation: Some(_),
                session_revision: Some(7),
            }
        ));
    }

    #[tokio::test]
    async fn concurrent_completion_seal_vs_cancel_is_first_writer_wins() {
        let host = Arc::new(InlineEffectHost::default());
        let driver = TurnWorkDriver::new(host.clone());
        let address = address("race");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

        let (seal, cancel) = tokio::join!(
            active.settle_before_commit(host.as_ref(), false),
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
        let host = Arc::new(InlineEffectHost::default());
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
            .settle_before_commit(host.as_ref(), false)
            .await
            .expect("settle recovered turn")
            .expect("pending cancellation survives owner loss");
        assert_eq!(settled, expected);
    }

    #[tokio::test]
    async fn turn_control_is_exact_scope_and_excluded_from_wait_cancel_sweep() {
        let host = Arc::new(InlineEffectHost::default());
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
        let host = Arc::new(InlineEffectHost::default());
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
        let host = Arc::new(InlineEffectHost::default());
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
            .settle_before_commit(host.as_ref(), false)
            .await
            .expect("seal after timed-out attach");
        active
            .publish_terminal(
                host.as_ref(),
                &TurnTerminal::Committed {
                    outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                        text: "done".to_string(),
                    }),
                    cancellation: None,
                    session_revision: None,
                },
            )
            .await
            .expect("publish after timed-out attach");
        assert!(matches!(
            driver.await_terminal(&address).await.expect("late attach"),
            TurnTerminal::Committed {
                outcome: TurnOutcome::Finished(_),
                cancellation: None,
                ..
            }
        ));
    }

    #[test]
    fn local_cancel_origin_hint_preserves_first_origin() {
        let hint = TurnCancelOriginHint::default();
        hint.set(Some("shutdown".to_string()));
        hint.set(Some("user".to_string()));

        assert_eq!(hint.get().as_deref(), Some("shutdown"));
    }

    #[test]
    fn local_cancel_origin_hint_preserves_explicit_absence() {
        let hint = TurnCancelOriginHint::default();
        hint.set(None);
        hint.set(Some("user".to_string()));

        assert_eq!(hint.get(), None);
    }

    #[test]
    fn terminal_success_has_no_cancellation_evidence() {
        let terminal = TurnTerminal::Committed {
            outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: "done".to_string(),
            }),
            cancellation: None,
            session_revision: None,
        };
        let encoded = terminal_resolution(&terminal).expect("encode terminal");
        assert!(matches!(encoded, Resolution::Ok(_)));
    }
}
