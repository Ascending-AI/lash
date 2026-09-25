pub use lash_core_store::turn_control_binding::*;
pub use lash_core_store::turn_control_vocabulary::*;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{ErrorEnvelope, TurnOutcome};

use super::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    Resolution, ResolveOutcome, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, ScopedEffectController,
};

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// A code cell's cancel checkpoint (FIG-3672 P9): the cell whose effect
    /// has replay key `cell` reached its `checkpoint`-th instruction bucket.
    /// Instruction counts are deterministic, so a replay of the cell issues
    /// the same checkpoints at the same points.
    CellCheckpoint {
        cell: String,
        checkpoint: u64,
    },
    /// After a code cell that stopped on the host: whether the stop was the
    /// turn's cancellation. The cell's effect replay key `cell` is unique in
    /// the turn.
    AfterCell {
        cell: String,
    },
}

impl TurnCancelPeekIdentity {
    /// The effect id the gate's journaled observation carries.
    pub fn causal_identity(&self) -> String {
        match self {
            Self::StartGate => "turn_cancel.start_gate".to_string(),
            Self::PostAbortGate => "turn_cancel.post_abort_gate".to_string(),
            Self::AfterLlm { protocol_iteration } => {
                format!("turn_cancel.after_llm.{protocol_iteration}")
            }
            Self::AfterStep { protocol_iteration } => {
                format!("turn_cancel.after_step.{protocol_iteration}")
            }
            Self::CellCheckpoint { cell, checkpoint } => {
                format!("turn_cancel.cell_checkpoint.{checkpoint}.{cell}")
            }
            Self::AfterCell { cell } => format!("turn_cancel.after_cell.{cell}"),
        }
    }

    /// Identity of the escalation peek that follows this gate peek when the
    /// gate holds an after-step request. Issued only in that case, and the
    /// gate answer it depends on is itself journaled, so replay is stable.
    fn escalation_causal_identity(&self) -> String {
        format!(
            "turn_cancel.escalation.{}",
            &self.causal_identity()["turn_cancel.".len()..]
        )
    }

    /// Boundaries that honour an after-step request outright. The post-abort
    /// gate is reached only after an effect already stopped the turn, and a
    /// mid-run peek defers an after-step request to its step boundary.
    fn honours_after_step(&self) -> Option<Option<usize>> {
        match self {
            Self::StartGate | Self::PostAbortGate => Some(None),
            Self::AfterStep { protocol_iteration } => Some(Some(*protocol_iteration)),
            Self::AfterLlm { .. } | Self::CellCheckpoint { .. } | Self::AfterCell { .. } => None,
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

pub use lash_sansio::{TurnCancelDisposition, TurnCancelMode, TurnCancellationEvidence};

mod local_stop;
pub use local_stop::{LocalTurnStop, StopDeliveryGuard};

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
/// gate. The promise is journaled by the effect host, so another process or a
/// replayed owner observes the request. The returned [`TurnCancelReceipt`]
/// reports only the cancellation outcome.
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
        let resolver: &dyn AwaitEventResolver = self.effect_host.as_ref();
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
        // The owner seals the base gate, then commits, then publishes the
        // terminal. The two checks above cover the last two steps, so without
        // this one a request landing in the first window would write a
        // provisional row for a turn whose cancellation authority has already
        // closed against it. A request that has ended is a typed no-op with no
        // durable effect at all, so the seal is observed before the write.
        match resolver.peek_await_event(&key).await {
            Ok(Some(terminal)) => {
                if matches!(decode_gate(terminal)?, TurnGateTerminal::CompletionSealed) {
                    return Ok(TurnCancelReceipt {
                        outcome: TurnCancelOutcome::CompletionWonRace,
                        record: None,
                    });
                }
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
                    // The disposition comparison is deliberately the first arm:
                    // a conflicting repeat is refused before it can reach the
                    // escalation promise, so a stronger timing mode never
                    // carries a different policy onto the address. Timing
                    // escalation is only offered to a repeat that already
                    // agrees with the accepted disposition.
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
                ActiveTurnControl::peek_base_cancel_evidence(resolver, &request.address).await?;
        }
        // No cancellation is in force for either no-op outcome, so the receipt
        // carries no record — the same shape the pre-gate no-op returns. A row
        // this caller provisionally wrote while racing the seal is not
        // cancellation evidence and must not be reported as if it were.
        if matches!(
            outcome,
            TurnCancelOutcome::CompletionWonRace | TurnCancelOutcome::UnknownOrRevoked
        ) {
            return Ok(TurnCancelReceipt {
                outcome,
                record: None,
            });
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
                .map_err(|err| {
                    RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, err.to_string())
                })?
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
    ///
    /// Only a request whose undelivered-input disposition already matches the
    /// base winner reaches here, the escalation payload records no
    /// disposition at all, and the reported evidence is rebuilt with the
    /// accepted base disposition, so escalation can change the honoured
    /// timing and nothing else.
    async fn escalate(
        &self,
        resolver: &dyn AwaitEventResolver,
        address: &TurnAddress,
        evidence: TurnCancellationEvidence,
        existing: TurnCancellationEvidence,
    ) -> Result<TurnCancelOutcome, RuntimeError> {
        let key = escalation_key(resolver, address).await?;
        let resolution = gate_resolution(TurnEscalationTerminal::Escalated(
            TurnEscalationEvidence::from(&evidence),
        ))?;
        Ok(
            match resolver.resolve_await_event(&key, resolution).await? {
                ResolveOutcome::Accepted => TurnCancelOutcome::Escalated(evidence),
                ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
                    TurnEscalationTerminal::Escalated(escalated) => {
                        TurnCancelOutcome::AlreadyRequested(escalated_cancel_evidence(
                            &existing, escalated,
                        ))
                    }
                    TurnEscalationTerminal::CompletionSealed => {
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
        // Refuses an address whose session does not exist before any wait.
        self.store_for(address).await?;
        let resolver: &dyn AwaitEventResolver = self.effect_host.as_ref();
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
pub(crate) enum TurnGateTerminal {
    CancelRequested(TurnCancellationEvidence),
    CompletionSealed,
}

/// The part of a cancellation request the escalation promise records: which
/// stronger request won escalation admission, and nothing else.
///
/// The accepted undelivered-input disposition is deliberately absent. It has
/// exactly one durable home — the base gate's [`TurnGateTerminal`] evidence —
/// so an escalation row can never carry a second copy that disagrees. Readers
/// rebuild the effective evidence via [`escalated_cancel_evidence`].
///
/// The variant keeps the `cancel_requested` tag so the two promise spellings
/// inter-decode in both directions: a row written before this payload existed
/// simply ignores the extra fields, and a row written now decodes under the
/// old shape with `undelivered` taking its serde default — which every old
/// reader then discarded under the accepted base policy anyway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TurnEscalationEvidence {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Mode of the request that won escalation — always `Immediate`.
    #[serde(default, skip_serializing_if = "TurnCancelMode::is_immediate")]
    pub mode: TurnCancelMode,
}

impl From<&TurnCancellationEvidence> for TurnEscalationEvidence {
    fn from(evidence: &TurnCancellationEvidence) -> Self {
        Self {
            request_id: evidence.request_id.clone(),
            origin: evidence.origin.clone(),
            reason: evidence.reason.clone(),
            mode: evidence.mode,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "cancellation", rename_all = "snake_case")]
enum TurnEscalationTerminal {
    #[serde(rename = "cancel_requested")]
    Escalated(TurnEscalationEvidence),
    CompletionSealed,
}

pub(crate) fn gate_resolution(value: impl Serialize) -> Result<Resolution, RuntimeError> {
    serde_json::to_value(value)
        .map(Resolution::Ok)
        .map_err(|err| {
            RuntimeError::new(
                crate::RuntimeErrorCode::TurnCancelGateEncode,
                err.to_string(),
            )
        })
}

fn decode_gate<T: serde::de::DeserializeOwned>(resolution: Resolution) -> Result<T, RuntimeError> {
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

pub(crate) async fn cancel_gate_key(
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

pub(crate) async fn escalation_key(
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

/// Rebuild the effective cancellation evidence from an escalation-gate winner
/// and the accepted undelivered-input policy.
///
/// Timing escalation moves *when* a cancellation is honoured. It never moves
/// *what* the accepted request decided about undelivered active-turn input:
/// that disposition belongs to the base-gate winner and is immutable once
/// accepted. [`TurnWorkDriver::request_cancel`] refuses a conflicting
/// disposition before it ever reaches the escalation promise, and the
/// escalation payload no longer even carries the field, so every reader
/// reconstructs the effective evidence with the base disposition. The
/// invariant then holds structurally: no escalation row — replayed from a
/// durable journal, written by a peer process, or minted by a future writer —
/// can silently substitute the accepted policy, which is the substitution
/// FIG-2874 removes.
fn escalated_cancel_evidence(
    base: &TurnCancellationEvidence,
    escalation: TurnEscalationEvidence,
) -> TurnCancellationEvidence {
    TurnCancellationEvidence {
        request_id: escalation.request_id,
        origin: escalation.origin,
        reason: escalation.reason,
        undelivered: base.undelivered,
        mode: escalation.mode,
        honoured_after_step: None,
    }
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
        Some(TurnEscalationTerminal::Escalated(escalated)) => {
            Ok(escalated_cancel_evidence(&base, escalated))
        }
        Some(TurnEscalationTerminal::CompletionSealed) | None => Ok(base),
    }
}

/// The keys of one turn's cancellation gate pair: the base gate and its
/// escalation promise.
///
/// An engine that races a wait it records against a turn's cancellation, and
/// a recorded step body that watches it, both wait on this pair; drive code
/// never does (FIG-3672 P9).
#[derive(Clone, Debug)]
pub struct TurnCancelGatePair {
    cancel: AwaitEventKey,
    escalation: AwaitEventKey,
}

impl TurnCancelGatePair {
    /// The pair for `scope`'s turn, keyed by `resolver`.
    pub async fn for_scope(
        resolver: &dyn AwaitEventResolver,
        scope: &ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            cancel: resolver
                .await_event_key(scope, AwaitEventWaitIdentity::TurnCancelGate)
                .await?,
            escalation: resolver
                .await_event_key(scope, AwaitEventWaitIdentity::TurnCancelEscalation)
                .await?,
        })
    }

    /// The pair from keys an engine already derived.
    pub fn new(cancel: AwaitEventKey, escalation: AwaitEventKey) -> Self {
        Self { cancel, escalation }
    }

    /// Resolves once the pair asks the turn to stop now: an `Immediate`
    /// request, or an escalation of an `AfterStep` one. An `AfterStep`
    /// request alone never resolves it; the turn honours that one at its
    /// journaled step-boundary peek. `Ok(None)` means the gate closed without
    /// a stop (a completion seal), or `await_key` gave up. `await_key` is the
    /// caller's own wait on one key.
    pub async fn await_stop<F, Fut>(
        &self,
        await_key: F,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError>
    where
        F: Fn(AwaitEventKey) -> Fut,
        Fut: std::future::Future<Output = Result<Resolution, RuntimeError>>,
    {
        let resolution = await_key(self.cancel.clone()).await?;
        if matches!(resolution, Resolution::Cancelled) {
            return Ok(None);
        }
        match decode_gate(resolution)? {
            TurnGateTerminal::CancelRequested(evidence) if evidence.mode.is_immediate() => {
                Ok(Some(evidence))
            }
            TurnGateTerminal::CancelRequested(base) => {
                let resolution = await_key(self.escalation.clone()).await?;
                if matches!(resolution, Resolution::Cancelled) {
                    return Ok(None);
                }
                match decode_gate(resolution)? {
                    TurnEscalationTerminal::Escalated(escalated) => {
                        Ok(Some(escalated_cancel_evidence(&base, escalated)))
                    }
                    TurnEscalationTerminal::CompletionSealed => Ok(None),
                }
            }
            TurnGateTerminal::CompletionSealed => Ok(None),
        }
    }
}

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
            gate_resolution(TurnEscalationTerminal::CompletionSealed)?,
        )
        .await?;
    match outcome {
        ResolveOutcome::Accepted => Ok(Some(base)),
        ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
            TurnEscalationTerminal::Escalated(escalated) => {
                Ok(Some(escalated_cancel_evidence(&base, escalated)))
            }
            TurnEscalationTerminal::CompletionSealed => Ok(Some(base)),
        },
        ResolveOutcome::UnknownOrRevoked => Ok(None),
    }
}

/// One physical turn's handle on its durable cancellation gate pair.
///
/// It holds the gate keys and nothing mutable. What the turn honours is a
/// recorded fact the drive keeps itself (ADR 0105 §3): it is advanced only by
/// the journaled peeks below and by recorded outcomes, and the drive hands it
/// back here when it settles the gate. No live watch, flag or token on the
/// drive path decides anything.
///
/// Two kinds of caller use this handle:
///
/// - **drive code** issues the journaled peeks ([`Self::observe_pending_cancel`])
///   and settles the gate before the final commit ([`Self::settle_before_commit`]);
/// - **execution-side code** — a recorded step body, or a host-local stop
///   forwarder — watches or resolves the gate over a deployment resolver
///   ([`Self::watch_immediate`], [`Self::request_local_stop`]). What it observes
///   reaches the drive only through the step's recorded outcome or a later
///   journaled peek.
pub struct ActiveTurnControl {
    address: TurnAddress,
    cancel_key: AwaitEventKey,
    terminal_key: AwaitEventKey,
    escalation_key: AwaitEventKey,
}

impl ActiveTurnControl {
    /// The terminal the turn proposes for its base gate: the cancellation it
    /// honours (the drive's recorded fact), else the one its assembled outcome
    /// carries, else a completion seal.
    fn proposed_terminal(
        honoured: Option<&TurnCancellationEvidence>,
        assembled: Option<TurnCancellationEvidence>,
    ) -> TurnGateTerminal {
        match honoured.cloned().or(assembled) {
            Some(evidence) => TurnGateTerminal::CancelRequested(evidence),
            None => TurnGateTerminal::CompletionSealed,
        }
    }

    /// Materialize the exact closure operation before any promise is resolved.
    pub fn closure_authorization(
        &self,
        binding_id: impl Into<String>,
        admitted_scope: ExecutionScope,
        fence: &crate::SessionExecutionLeaseAuthority,
        observed_intent: TurnCancelIntentSnapshot,
        honoured: Option<&TurnCancellationEvidence>,
        assembled: Option<TurnCancellationEvidence>,
    ) -> Result<TurnCancelClosureAuthorization, RuntimeError> {
        let proposed_base = match Self::proposed_terminal(honoured, assembled) {
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
        honoured: Option<&TurnCancellationEvidence>,
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
        let effective_cancellation = self.settle_proposed(resolver, proposed, honoured).await?;
        let base_cancellation = self.read_settled_base_cancel_evidence(resolver).await?;
        Ok(TurnCancelClosureSettlement::new(
            authorization.clone(),
            base_cancellation,
            effective_cancellation,
        ))
    }

    async fn settle_proposed(
        &self,
        resolver: &dyn AwaitEventResolver,
        proposed: TurnGateTerminal,
        honoured: Option<&TurnCancellationEvidence>,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
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
                // The boundary that honoured the request is the drive's
                // recorded fact, not the gate's: carry it onto the settled
                // winner when the winner is the request the turn honoured.
                if let Some(honoured) = honoured {
                    let mut cached = honoured.clone();
                    let honoured_after_step = cached.honoured_after_step.take();
                    if cached == effective {
                        effective.honoured_after_step = honoured_after_step;
                    }
                }
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
        })
    }

    /// The turn this handle controls.
    pub fn address(&self) -> &TurnAddress {
        &self.address
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
    pub async fn peek_orphan_repair_decision(
        resolver: &dyn AwaitEventResolver,
        address: &TurnAddress,
    ) -> Result<Option<crate::TurnCancelRepairDecision>, RuntimeError> {
        Self::peek_effective_cancel_decision(resolver, address).await
    }

    /// Execution-side only: wait until the gate pair asks this turn to stop
    /// now — an `Immediate` request, or an escalation of an `AfterStep` one.
    ///
    /// A recorded step body (a model call) races its work against this, over
    /// the deployment resolver, and records what it saw in its own outcome:
    /// this is how a race against a step that cannot be selected away from
    /// mid-flight keeps its loser (ADR 0105 §3). See
    /// [`TurnCancelGatePair::await_stop`]. Drive code never awaits this.
    pub async fn watch_immediate(
        &self,
        resolver: &dyn AwaitEventResolver,
        stop_wait: CancellationToken,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        TurnCancelGatePair {
            cancel: self.cancel_key.clone(),
            escalation: self.escalation_key.clone(),
        }
        .await_stop(|key| {
            let stop_wait = stop_wait.clone();
            async move { resolver.await_await_event(&key, stop_wait, None).await }
        })
        .await
    }

    /// Execution-side only: turn a host-local stop into a durable request on
    /// this turn's gate pair, with lash's internal evidence.
    ///
    /// The base gate is first-writer-wins, so a stop that finds a request
    /// already there changes nothing but, for an `Immediate` stop over an
    /// `AfterStep` winner, the escalation promise. The drive observes the
    /// result only through its journaled peeks and the recorded outcomes of
    /// its steps, exactly like a routed [`TurnWorkDriver::request_cancel`].
    pub async fn request_local_stop(
        &self,
        resolver: &dyn AwaitEventResolver,
        mode: TurnCancelMode,
        origin: Option<String>,
    ) -> Result<(), RuntimeError> {
        let evidence = TurnCancellationEvidence {
            mode,
            ..self.internal_evidence(origin)
        };
        let resolution = gate_resolution(TurnGateTerminal::CancelRequested(evidence.clone()))?;
        match resolver
            .resolve_await_event(&self.cancel_key, resolution)
            .await?
        {
            ResolveOutcome::Accepted | ResolveOutcome::UnknownOrRevoked => Ok(()),
            ResolveOutcome::AlreadyResolved { terminal } => match decode_gate(terminal)? {
                TurnGateTerminal::CancelRequested(existing)
                    if evidence.mode.is_stronger_than(existing.mode) =>
                {
                    let escalation = gate_resolution(TurnEscalationTerminal::Escalated(
                        TurnEscalationEvidence::from(&evidence),
                    ))?;
                    resolver
                        .resolve_await_event(&self.escalation_key, escalation)
                        .await?;
                    Ok(())
                }
                TurnGateTerminal::CancelRequested(_) | TurnGateTerminal::CompletionSealed => Ok(()),
            },
        }
    }

    /// Journaled observation of the cancellation gate at one replay identity.
    ///
    /// An after-step request is honoured at the start gate, the post-abort gate, and its own
    /// step boundary; at a mid-run peek it is deferred and only an escalation (peeked under a
    /// derived identity, so replay stays deterministic) makes the turn stop there. The answer
    /// is the caller's to record as the cancellation the turn honours.
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
            return Ok(Some(evidence));
        }
        let escalation = self
            .peek(
                controller,
                identity.escalation_causal_identity(),
                &self.escalation_key,
            )
            .await?;
        if let Some(TurnEscalationTerminal::Escalated(escalated)) = escalation {
            return Ok(Some(escalated_cancel_evidence(&evidence, escalated)));
        }
        let Some(honoured_after_step) = identity.honours_after_step() else {
            return Ok(None);
        };
        Ok(Some(TurnCancellationEvidence {
            honoured_after_step,
            ..evidence
        }))
    }

    async fn peek<T: serde::de::DeserializeOwned>(
        &self,
        controller: &ScopedEffectController<'_>,
        causal_identity: String,
        key: &AwaitEventKey,
    ) -> Result<Option<T>, RuntimeError> {
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
    /// `honoured` is the cancellation the drive recorded the turn honouring;
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
        honoured: Option<&TurnCancellationEvidence>,
        assembled: Option<TurnCancellationEvidence>,
    ) -> Result<Option<TurnCancellationEvidence>, RuntimeError> {
        let proposed = Self::proposed_terminal(honoured, assembled);
        self.settle_proposed(resolver, proposed, honoured).await
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

    /// Lash's own evidence for a stop no routed request carried: a host-local
    /// stop, or a cancelled outcome whose request the turn never observed.
    pub fn internal_evidence(&self, origin: Option<String>) -> TurnCancellationEvidence {
        TurnCancellationEvidence {
            origin,
            ..TurnCancellationEvidence::internal(&self.address.turn_id)
        }
    }
}

/// The pure turn-control laws; the laws that drive a host and a session store
/// run over a SQLite memory backend in `tests/store_backed` (ADR 0102).
#[cfg(test)]
#[path = "turn_control/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "turn_control/determinism_tests.rs"]
mod determinism_tests;
