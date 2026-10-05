//! Control intents (FIG-3600 S7, ADR 0104 O4, astra B6): an operator's
//! decision about a logical run or a session, persisted as a versioned
//! record in the transaction that applies its store half.
//!
//! The engine half is a `ControlIntent` obligation (ADR 0109): the
//! transaction that records the intent arms it on the intent's row, a relay
//! delivers it immediately and retries it with backoff, and every write that
//! settles the engine half — its acknowledgement or its failure — compares
//! the obligation's claim token, so a claim another relay retook never
//! settles the intent. A `CloseSession` intent outlives its session: it is
//! the positive deletion tombstone the factory answers a deleted session's
//! runs from.
//!
//! The ledger is [`ControlIntentStore`], carried by the session store
//! factory rather than a session's own store: a `CloseSession` intent's engine
//! half is acknowledged, or retried by its relay, after its session is gone.

use serde::{Deserialize, Serialize};

use super::{ClaimToken, DeliveryError, EnginePark, ObligationId, ObligationState, ParkId};
use crate::{SessionId, TurnId};

/// The registered durable format of a [`ControlIntent`] record.
/// version_surface = "coexist"
/// version_guard( items(from_stored))
pub const CONTROL_INTENT_FORMAT: u32 = 1;

/// A control intent's id: the store's intent clock sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControlIntentId(u64);

impl ControlIntentId {
    /// The id the store's intent clock allocated as `sequence`.
    #[must_use]
    pub const fn from_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// The clock sequence this id names.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ControlIntentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// What an intent decides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlIntentKind {
    /// Resume the parked run's execution under the same fence.
    Redrive { run: TurnId, park: ParkId },
    /// End the parked run `Cancelled`.
    Cancel { run: TurnId, park: ParkId },
    /// End the parked run and execute its held inputs under `new_run`.
    Fork {
        run: TurnId,
        park: ParkId,
        new_run: Option<TurnId>,
    },
    /// Close the session: every listed run ends `Cancelled`.
    CloseSession { runs: Vec<TurnId> },
}

impl ControlIntentKind {
    /// The stored code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Redrive { .. } => "redrive",
            Self::Cancel { .. } => "cancel",
            Self::Fork { .. } => "fork",
            Self::CloseSession { .. } => "close_session",
        }
    }
}

/// What was decided about an intent's engine half. A failed attempt and
/// the attempts running out are not states: they live on the intent's
/// obligation alone, and whether the engine half is still owed is
/// [`ControlIntent::engine_half_owed`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ControlIntentState {
    /// The store half committed; nothing has decided the engine half.
    Pending,
    Acknowledged {
        at_ms: u64,
    },
    /// A redrive overtaken by a cancel, fork or close before it applied.
    Superseded {
        by: ControlIntentId,
    },
    /// The engine refused the engine half for good, for `cause`. Re-arming
    /// the intent's stalled obligation returns it to `Pending`.
    Refused {
        cause: DeliveryError,
    },
}

impl ControlIntentState {
    /// The stored code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged { .. } => "acknowledged",
            Self::Superseded { .. } => "superseded",
            Self::Refused { .. } => "refused",
        }
    }
}

/// The `ControlIntent` obligation armed on an intent's row, as the row read
/// it: its id, and where it stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentObligation {
    pub id: ObligationId,
    pub state: ObligationState,
}

/// One control intent, as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlIntent {
    pub id: ControlIntentId,
    pub session_id: SessionId,
    /// [`CONTROL_INTENT_FORMAT`] of the writer.
    pub format: u32,
    pub kind: ControlIntentKind,
    pub state: ControlIntentState,
    pub created_at_ms: u64,
    /// The engine's handle on the run's stopped execution, copied from its
    /// park by a verb whose store half deletes the park (a cancel or a
    /// fork), so the engine half can still find the execution to release.
    pub engine: Option<EnginePark>,
    /// The `ControlIntent` obligation the recording transaction armed on
    /// the intent's row (ADR 0109): its engine half, delivered under its
    /// id. Its attempts, due time, last error and stall live on the
    /// obligation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation: Option<IntentObligation>,
}

impl ControlIntent {
    /// The id its obligation is delivered under, once armed.
    #[must_use]
    pub fn obligation_id(&self) -> Option<&ObligationId> {
        self.obligation.as_ref().map(|obligation| &obligation.id)
    }

    /// Whether this intent's engine half is still owed: nothing has decided
    /// it, and its obligation is due or claimed, so a relay will deliver it.
    /// While a cancel or fork is owed its session admits nothing. An intent
    /// whose obligation stalled owes nothing until an operator re-arms it,
    /// whatever stalled it, so a stall never holds a session.
    ///
    /// This is the one definition in Rust; the stores' `engine_half_owed`
    /// generated column states the same rule over the same two columns.
    #[must_use]
    pub fn engine_half_owed(&self) -> bool {
        matches!(self.state, ControlIntentState::Pending)
            && self.obligation.as_ref().is_some_and(|obligation| {
                matches!(
                    obligation.state,
                    ObligationState::Due | ObligationState::Claimed
                )
            })
    }

    /// The deletion this intent records, when it is a session's
    /// `CloseSession`: the terminal evidence every run of the deleted
    /// session answers.
    #[must_use]
    pub fn session_deleted_terminal(&self, run: &TurnId) -> Option<super::RunTerminal> {
        matches!(self.kind, ControlIntentKind::CloseSession { .. }).then(|| super::RunTerminal {
            session_id: self.session_id.clone(),
            run: run.clone(),
            cause: super::RunTerminalCause::SessionDeleted { intent: self.id },
            head_revision: None,
            at_ms: self.created_at_ms,
        })
    }

    /// Decode the columns a backend stored, refusing a format this build
    /// does not read.
    #[allow(clippy::too_many_arguments)]
    pub fn from_stored(
        id: u64,
        session_id: SessionId,
        format: u32,
        kind_json: &str,
        state_json: &str,
        created_at_ms: u64,
        engine: Option<String>,
        obligation_id: Option<String>,
        obligation_state: Option<String>,
    ) -> Result<Self, super::StoreError> {
        if format != CONTROL_INTENT_FORMAT {
            return Err(super::StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "ControlIntent",
                actual: format,
                expected: CONTROL_INTENT_FORMAT,
            });
        }
        let corrupt = |message: String| super::StoreError::StoredDataCorrupt {
            record_kind: "ControlIntent",
            message,
        };
        Ok(Self {
            id: ControlIntentId::from_sequence(id),
            session_id,
            format,
            kind: serde_json::from_str(kind_json)
                .map_err(|error| corrupt(format!("control intent kind: {error}")))?,
            state: serde_json::from_str(state_json)
                .map_err(|error| corrupt(format!("control intent state: {error}")))?,
            created_at_ms,
            engine: engine.map(EnginePark::new),
            obligation: match (obligation_id, obligation_state) {
                (Some(id), Some(state)) => Some(IntentObligation {
                    id: ObligationId::new(id),
                    state: ObligationState::from_label(&state)?,
                }),
                (None, None) => None,
                _ => {
                    return Err(corrupt(
                        "control intent obligation id and state disagree".to_owned(),
                    ));
                }
            },
        })
    }
}

impl ControlIntent {
    /// The runs a `CloseSession` intent closed; empty for every other kind.
    #[must_use]
    pub fn closed_runs(&self) -> &[TurnId] {
        match &self.kind {
            ControlIntentKind::CloseSession { runs } => runs,
            _ => &[],
        }
    }

    /// The instant-free identity of the intent's decision: its id, session
    /// and kind. A retried writer answers the same decision in another
    /// state.
    #[must_use]
    pub fn same_decision(&self, other: &Self) -> bool {
        self.id == other.id && self.session_id == other.session_id && self.kind == other.kind
    }
}

/// What [`ControlIntentStore::claim_intent_application`] answers: whether the
/// engine half should run now. The state is re-read in the store's
/// transaction, so an intent a later one superseded never reaches its engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentApplication {
    /// Nothing has decided the engine half: run it.
    Apply(ControlIntent),
    /// A later intent superseded it before it applied: run nothing.
    Superseded(ControlIntent),
    /// Its engine half is acknowledged, or refused for good: run nothing.
    Done(ControlIntent),
}

impl IntentApplication {
    /// The intent, whatever the answer.
    #[must_use]
    pub fn intent(&self) -> &ControlIntent {
        match self {
            Self::Apply(intent) | Self::Superseded(intent) | Self::Done(intent) => intent,
        }
    }
}

/// Decide a claim of an intent's application at `at_ms` against its stored
/// state and, for a redrive, the session's park `park` as the claim's
/// transaction read it. The answer carries the intent as it is to be stored:
/// a store writes it when it differs from `stored`.
///
/// A redrive applies only while its park still names it — the same run, the
/// same park, and the park's `resume_intent` this redrive. Otherwise the run
/// ran past it: it committed (the park is gone), or it parked again (a
/// re-park clears `resume_intent`), so resuming now would wake an execution
/// that already re-decided, or one the store has ended. Such a redrive is
/// settled `Acknowledged` at `at_ms` without running and answered `Done`.
#[must_use]
pub fn decide_intent_application(
    stored: ControlIntent,
    park: Option<&super::TurnPark>,
    at_ms: u64,
) -> IntentApplication {
    match stored.state {
        ControlIntentState::Pending => {
            if let ControlIntentKind::Redrive {
                run, park: parked, ..
            } = &stored.kind
                && !park.is_some_and(|park| {
                    park.turn_id == *run
                        && park.park_id == *parked
                        && park.resume_intent == Some(stored.id)
                })
            {
                let mut settled = stored;
                settled.state = ControlIntentState::Acknowledged { at_ms };
                return IntentApplication::Done(settled);
            }
            IntentApplication::Apply(stored)
        }
        ControlIntentState::Superseded { .. } => IntentApplication::Superseded(stored),
        ControlIntentState::Acknowledged { .. } | ControlIntentState::Refused { .. } => {
            IntentApplication::Done(stored)
        }
    }
}

/// What a claim-fenced write of an intent's engine half answers (ADR 0109
/// §1.3): [`ControlIntentStore::acknowledge_intent`] and
/// [`ControlIntentStore::refuse_intent`] compare the intent's
/// obligation claim token before they write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentSettle {
    /// The claim held: the intent as stored after the write — unchanged
    /// when it was no longer pending.
    Held(Box<ControlIntent>),
    /// The obligation is no longer claimed under the caller's token: another
    /// relay retook it after the claim lapsed, or it settled. Nothing was
    /// written.
    ClaimLost,
}

/// The state an acknowledgement writes over `stored`: `None` when the intent
/// is not pending (already acknowledged, superseded or refused), so a
/// retried acknowledgement writes nothing.
#[must_use]
pub fn decide_intent_acknowledgement(
    stored: &ControlIntentState,
    at_ms: u64,
) -> Option<ControlIntentState> {
    matches!(stored, ControlIntentState::Pending)
        .then_some(ControlIntentState::Acknowledged { at_ms })
}

/// The state a permanent refusal writes over `stored`: `None` when the
/// intent is no longer pending, so a late refusal never reopens an
/// acknowledged or superseded intent.
#[must_use]
pub fn decide_intent_refusal(
    stored: &ControlIntentState,
    cause: &DeliveryError,
) -> Option<ControlIntentState> {
    matches!(stored, ControlIntentState::Pending).then(|| ControlIntentState::Refused {
        cause: cause.clone(),
    })
}

/// The stored columns of an intent's state: its code and its JSON.
pub fn stored_intent_state(
    state: &ControlIntentState,
) -> Result<(&'static str, String), super::StoreError> {
    let code = state.code();
    let json =
        serde_json::to_string(state).map_err(|error| super::StoreError::RecordEncodingFailed {
            record_kind: "ControlIntent".to_string(),
            message: error.to_string(),
        })?;
    Ok((code, json))
}

/// The stored JSON of an intent's kind.
pub fn stored_intent_kind(kind: &ControlIntentKind) -> Result<String, super::StoreError> {
    serde_json::to_string(kind).map_err(|error| super::StoreError::RecordEncodingFailed {
        record_kind: "ControlIntent".to_string(),
        message: error.to_string(),
    })
}

/// The verb an operator applies to a parked run (ADR 0104 O4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunVerb {
    /// Resume the run's stopped execution under the same fence.
    Redrive,
    /// End the run `Cancelled`, settling the inputs it held.
    Cancel,
    /// End the run and execute the inputs it held under a new run.
    Fork,
}

/// An operator's verb on the parked run `run` of `session_id`, compared
/// against the park it saw (`park`, the CAS token).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunIntentRequest {
    pub session_id: SessionId,
    pub run: TurnId,
    pub park: ParkId,
    pub verb: RunVerb,
}

/// Why a run verb's store half refused: nothing was written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunIntentRefused {
    /// The run holds no park.
    #[error("the run is not parked")]
    NotParked,
    /// The run parked again since the caller read it: act on `current`.
    #[error("the park was superseded by park {current}")]
    ParkSuperseded { current: ParkId },
    /// A redrive of the run already runs, or is on its way: cancel the
    /// running run cooperatively instead, or wait for it to park again.
    #[error("the run is being redriven by intent {intent}")]
    Redriving { intent: ControlIntentId },
    /// A cancel or fork of the run is still open.
    #[error("intent {intent} is still open on the run")]
    IntentOpen { intent: ControlIntentId },
    /// The session was deleted; its close intent remains durable.
    #[error("the session was deleted")]
    SessionDeleted,
    /// The session is closing: its `CloseSession` intent ends every run.
    #[error("the session is closing")]
    SessionClosing,
    /// The store did not answer.
    #[error(transparent)]
    Store(#[from] super::StoreError),
}

/// What a run verb's store transaction read, for [`decide_run_intent`].
#[derive(Clone, Copy, Debug)]
pub struct RunIntentFacts<'a> {
    /// The session's `CloseSession` intent, when it is closing.
    pub closing: Option<ControlIntentId>,
    /// The session's park.
    pub park: Option<&'a super::TurnPark>,
    /// The session's owed verbs (every intent whose engine half is owed,
    /// but its close).
    pub open_verbs: &'a [ControlIntent],
    /// The redrive the park's `resume_intent` names, as stored.
    pub resume: Option<&'a ControlIntent>,
}

/// What a run verb's store half writes besides its own intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunIntentPlan {
    /// The park the verb acts on.
    pub park: super::TurnPark,
    /// Open redrives of the run a cancel or fork supersedes.
    pub supersede: Vec<ControlIntent>,
}

/// Decide `request` against what its transaction read (D2 §1.4, §2): the
/// park must be the run's and the one the caller saw; no other cancel or
/// fork may be open; a cancel or fork refuses while the redrive the park
/// names has resumed the run, and otherwise supersedes every redrive of the
/// run still open — the one the park names and any older one a re-park left
/// behind — so no redrive can resume the run after it ends.
///
/// # Errors
/// The refusal the verb answers; nothing is written.
pub fn decide_run_intent(
    request: &RunIntentRequest,
    facts: &RunIntentFacts<'_>,
) -> Result<RunIntentPlan, RunIntentRefused> {
    if facts.closing.is_some() {
        return Err(RunIntentRefused::SessionClosing);
    }
    let run_verb = |intent: &&ControlIntent| match &intent.kind {
        ControlIntentKind::Redrive { run, .. }
        | ControlIntentKind::Cancel { run, .. }
        | ControlIntentKind::Fork { run, .. } => *run == request.run,
        ControlIntentKind::CloseSession { .. } => false,
    };
    if let Some(open) = facts.open_verbs.iter().filter(run_verb).find(|intent| {
        matches!(
            intent.kind,
            ControlIntentKind::Cancel { .. } | ControlIntentKind::Fork { .. }
        )
    }) {
        return Err(RunIntentRefused::IntentOpen { intent: open.id });
    }
    let park = facts
        .park
        .filter(|park| park.turn_id == request.run)
        .ok_or(RunIntentRefused::NotParked)?;
    if park.park_id != request.park {
        return Err(RunIntentRefused::ParkSuperseded {
            current: park.park_id,
        });
    }
    // A redrive the park names: owed means it has not resumed the run yet;
    // acknowledged means it did, and the run executes until it parks again
    // (which clears `resume_intent`) or commits (which clears the park).
    let redrive = facts.resume.filter(|intent| {
        intent.engine_half_owed() || matches!(intent.state, ControlIntentState::Acknowledged { .. })
    });
    match (request.verb, redrive) {
        (RunVerb::Redrive, Some(redrive)) => {
            return Err(RunIntentRefused::Redriving { intent: redrive.id });
        }
        (_, Some(redrive)) if !redrive.engine_half_owed() => {
            return Err(RunIntentRefused::Redriving { intent: redrive.id });
        }
        _ => {}
    }
    let supersede = if request.verb == RunVerb::Redrive {
        Vec::new()
    } else {
        let mut open: Vec<ControlIntent> = facts
            .open_verbs
            .iter()
            .filter(run_verb)
            .filter(|intent| matches!(intent.kind, ControlIntentKind::Redrive { .. }))
            .cloned()
            .collect();
        if let Some(named) = redrive.filter(|named| !open.iter().any(|o| o.id == named.id)) {
            open.push(named.clone());
        }
        open.sort_by_key(|intent| intent.id);
        open
    };
    Ok(RunIntentPlan {
        park: park.clone(),
        supersede,
    })
}

/// The run a fork of input run `run` executes its held inputs under:
/// `{run}~fork{intent}` (D2 §1.4). The transaction reserves the intent id
/// before binding the released members, so the name is unique and stable.
#[must_use]
pub fn forked_run(run: &TurnId, intent: ControlIntentId) -> TurnId {
    run.with_suffix(format_args!("~fork{intent}"))
}

/// The deployment's control-intent ledger (FIG-3600 S7, ADR 0104 O4, astra
/// B6), carried by the session store factory.
///
/// Every method is required — [`open_run_intent`](Self::open_run_intent)
/// excepted while the verb stores land: a factory states its answer, and a
/// decorator forwards to the catalog it wraps. A factory with no ledger
/// returns `StoreError::UnsupportedStoreOperation`, which fails a session
/// deletion closed instead of deleting a session whose runs nothing closed.
#[async_trait::async_trait]
pub trait ControlIntentStore: Send + Sync {
    /// Begin closing session `session_id`: the store half of its
    /// `CloseSession` intent, in one transaction. It
    ///
    /// - records the intent on the session (`session_meta.closing_intent`):
    ///   acceptance then refuses the session, and admission answers idle;
    /// - raises the session's shift epoch, so every fence an earlier
    ///   admission sealed is stale;
    /// - ends every run without terminal evidence `Cancelled` with cause
    ///   [`SessionDeleted`](super::RunTerminalCause::SessionDeleted), deletes
    ///   the session's park (feed `Cancelled{SessionDeleted}`) and settles its
    ///   open queued run;
    /// - supersedes every owed intent of the session;
    /// - inserts the `CloseSession { runs }` intent, `Pending`, naming the
    ///   runs it ended, with its `ControlIntent` obligation armed due now.
    ///
    /// Idempotent: a retry finds the session's intent and answers it, also
    /// after the session is deleted (the intent is its tombstone). `None`
    /// when the session has no durable record and was never closed: there
    /// is nothing to close, and nothing is written.
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, super::StoreError>;

    /// Decide the application of intent `id`'s engine half at `at_ms`:
    /// re-read its state, and for a redrive its session's park, in one
    /// transaction, and write what [`decide_intent_application`] decides — a
    /// redrive the run ran past settled. An unknown id is
    /// `StoreError::ControlIntentUnknown`.
    async fn claim_intent_application(
        &self,
        id: ControlIntentId,
        at_ms: u64,
    ) -> Result<IntentApplication, super::StoreError>;

    /// Acknowledge intent `id`'s engine half under the obligation claim
    /// `claim`, in one transaction that compares it: [`IntentSettle::ClaimLost`]
    /// and nothing written when the obligation is no longer claimed under
    /// `claim`. A no-op once the intent is not pending.
    async fn acknowledge_intent(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        at_ms: u64,
    ) -> Result<IntentSettle, super::StoreError>;

    /// Refuse intent `id`'s engine half for good, for `cause`, under the
    /// obligation claim `claim`, compared as
    /// [`acknowledge_intent`](Self::acknowledge_intent) compares it: the
    /// intent is `Refused { cause }`, visible for an operator, until its
    /// obligation is re-armed. A retryable failure is never written here: it
    /// is the obligation's alone. A no-op once the intent is not pending.
    async fn refuse_intent(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        cause: &DeliveryError,
        at_ms: u64,
    ) -> Result<IntentSettle, super::StoreError>;

    /// Intent `id` as stored, if it exists.
    async fn load_intent(
        &self,
        id: ControlIntentId,
    ) -> Result<Option<ControlIntent>, super::StoreError>;

    /// Open an operator's verb on a parked run: the store half of its
    /// intent, in one transaction, decided by [`decide_run_intent`].
    ///
    /// - **Redrive** records the intent on the park (`resume_intent`) and
    ///   feeds `RedriveRequested`. The shift epoch does not move: the parked
    ///   run's sealed fence stays current, so the resumed execution replays
    ///   under it.
    /// - **Cancel** writes the run's terminal evidence
    ///   (`OperatorCancelled`), settles the inputs it held `Cancelled` and
    ///   hands its claims' other rows back open (a queued run's execution settles
    ///   with it), deletes the park (feed `Cancelled{Operator}`), supersedes
    ///   an open redrive, and raises the shift epoch under admission
    ///   `intent:{id}`.
    /// - **Fork** is a cancel whose held inputs return open, bound to the
    ///   new run [`forked_run`] (an input run) in their original order;
    ///   a queued run's members return open unbound, and the next admission
    ///   mints their run. The cause is `Forked`.
    ///
    /// The intent is `Pending`, carrying the park's engine handle for the
    /// engine half, with its `ControlIntent` obligation armed due now.
    ///
    /// A factory with no verb store answers `UnsupportedStoreOperation`.
    async fn open_run_intent(
        &self,
        _request: &RunIntentRequest,
        _at_ms: u64,
    ) -> Result<ControlIntent, RunIntentRefused> {
        Err(super::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::open_run_intent",
        }
        .into())
    }
}

impl crate::store::DurableRecord for ControlIntent {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::control_intent::CONTROL_INTENT_FORMAT);
}

impl crate::store::DurableRecord for ControlIntentId {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(state: ObligationState) -> Option<IntentObligation> {
        Some(IntentObligation {
            id: ObligationId::new("control_intent:4"),
            state,
        })
    }

    fn intent(state: ControlIntentState) -> ControlIntent {
        ControlIntent {
            id: ControlIntentId::from_sequence(4),
            session_id: SessionId::from("s"),
            format: CONTROL_INTENT_FORMAT,
            kind: ControlIntentKind::CloseSession {
                runs: vec![TurnId::from("r")],
            },
            state,
            created_at_ms: 1,
            engine: None,
            obligation: armed(ObligationState::Due),
        }
    }

    fn refused() -> ControlIntentState {
        ControlIntentState::Refused {
            cause: DeliveryError::new(crate::RuntimeErrorCode::EngineHandleMismatch, "x"),
        }
    }

    /// F09 (FIG-4648): the engine half is owed exactly while nothing has
    /// decided it and its obligation is due or claimed. A stalled obligation
    /// owes nothing, whatever stalled it, and neither does a decided intent.
    #[test]
    fn the_engine_half_is_owed_only_while_pending_and_its_obligation_is_live() {
        for obligation in ObligationState::ALL {
            let pending = ControlIntent {
                obligation: armed(obligation),
                ..intent(ControlIntentState::Pending)
            };
            assert_eq!(
                pending.engine_half_owed(),
                matches!(obligation, ObligationState::Due | ObligationState::Claimed),
                "pending, obligation {obligation:?}"
            );
            for decided in [
                ControlIntentState::Acknowledged { at_ms: 2 },
                ControlIntentState::Superseded {
                    by: ControlIntentId::from_sequence(9),
                },
                refused(),
            ] {
                let decided = ControlIntent {
                    obligation: armed(obligation),
                    ..intent(decided)
                };
                assert!(
                    !decided.engine_half_owed(),
                    "{:?}, obligation {obligation:?}",
                    decided.state
                );
            }
        }
        let unarmed = ControlIntent {
            obligation: None,
            ..intent(ControlIntentState::Pending)
        };
        assert!(!unarmed.engine_half_owed());
    }

    fn redrive(state: ControlIntentState) -> ControlIntent {
        ControlIntent {
            kind: ControlIntentKind::Redrive {
                run: TurnId::from("r"),
                park: super::super::ParkId::from_feed_sequence(3),
            },
            ..intent(state)
        }
    }

    fn park(resume_intent: Option<u64>) -> super::super::TurnPark {
        super::super::TurnPark {
            session_id: SessionId::from("s"),
            turn_id: TurnId::from("r"),
            reason: super::super::ParkReason::ReplayDivergence {
                message: "m".into(),
            },
            park_id: super::super::ParkId::from_feed_sequence(3),
            since_ms: 1,
            last_refused_ms: 1,
            attempts: 1,
            engine: None,
            resume_intent: resume_intent.map(ControlIntentId::from_sequence),
            build_generation: None,
        }
    }

    /// F09: a redrive whose obligation stalled owes nothing, so the run it
    /// would have resumed takes a new verb instead of answering `Redriving`
    /// until an operator re-arms the old one.
    #[test]
    fn a_stalled_redrive_does_not_hold_its_run() {
        let stalled = ControlIntent {
            obligation: armed(ObligationState::Stalled),
            ..redrive(ControlIntentState::Pending)
        };
        let named = park(Some(4));
        for verb in [RunVerb::Redrive, RunVerb::Cancel, RunVerb::Fork] {
            let request = RunIntentRequest {
                session_id: SessionId::from("s"),
                run: TurnId::from("r"),
                park: super::super::ParkId::from_feed_sequence(3),
                verb,
            };
            decide_run_intent(
                &request,
                &RunIntentFacts {
                    closing: None,
                    park: Some(&named),
                    open_verbs: &[],
                    resume: Some(&stalled),
                },
            )
            .unwrap_or_else(|refused| panic!("{verb:?} behind a stalled redrive: {refused}"));
        }
    }

    #[test]
    fn a_late_acknowledgement_or_refusal_never_reopens_a_decided_intent() {
        let cause = DeliveryError::new(crate::RuntimeErrorCode::EngineHandleMismatch, "late");
        let acknowledged = ControlIntentState::Acknowledged { at_ms: 2 };
        assert_eq!(decide_intent_acknowledgement(&acknowledged, 5), None);
        assert_eq!(decide_intent_refusal(&acknowledged, &cause), None);
        assert_eq!(decide_intent_acknowledgement(&refused(), 5), None);
        assert_eq!(
            decide_intent_refusal(&ControlIntentState::Pending, &cause),
            Some(ControlIntentState::Refused {
                cause: cause.clone()
            })
        );
    }
}
