//! Claim eligibility and settlement planners shared by all three store
//! backends (FIG-1065).
//!
//! [`fencing`](super::fencing) holds the per-row verdicts; this module holds
//! the plans built from them, completing the claim half of the planner pattern
//! [`runtime_commit_plan`](super::runtime_commit_plan) established. A backend
//! contributes the locked observation of every selected or covered row; the
//! planner returns a [`ClaimPlanDecision`] or a [`SettlementDecision`]; the
//! backend executes the plan's ordered writes with its own statements and
//! keeps its conditional predicates as write backstops.
//!
//! The decision vocabulary is the one FIG-1065 names:
//!
//! * `Complete` — every observed row authorizes the operation; the plan's
//!   writes execute in order.
//! * `Defer` — a claim attempt found a selected row already held by the
//!   claiming generation itself. SQL backends roll the claim transaction
//!   back; the in-memory store stages nothing. Settlement has no deferral: a
//!   covered row either authorizes its write or supersedes the claim.
//! * `Superseded` — a settlement found a covered row absent or held by
//!   another claim; the commit fails with the carried diagnostics.
//!
//! What stays per-backend, by design: the lock that authorizes the
//! observation (PostgreSQL `FOR UPDATE` row locks and its wake-source
//! advisory lock, SQLite's `BEGIN IMMEDIATE` write transaction, the in-memory
//! store's staged mutation), the write statements themselves, and the
//! rows-affected backstop over them.

use super::StoreError;
use super::fencing::{
    FencedWrite, QueuedWorkSettlementFacts, TurnInputSettlementFacts, WorkRowClaimFacts,
    queued_work_batch_claimability, require_settleable_queued_work, require_settleable_turn_input,
    turn_input_claimability,
};
use super::queued_work::{
    ClaimCandidate, ClaimIdDialect, QueuedWorkClaimRefusal, TurnWorkEmptyScanDiagnostic,
    WorkClaimLease, select_turn_work_claim_prefix,
};
use crate::{LeaseOwnerIdentity, QueuedWorkClaimPolicy, SessionId};

// ---------------------------------------------------------------------------
// Claim acquisition
// ---------------------------------------------------------------------------

/// The shared eligibility decision for one claim attempt (FIG-1065).
///
/// `Complete` and `Defer` are the two live answers; `Empty` names the
/// degenerate selection every caller already short-circuits, kept so the
/// decision is total rather than implicit.
#[derive(Debug)]
pub enum ClaimPlanDecision<Plan> {
    /// Nothing was selected: the attempt commits with no claim.
    Empty,
    /// A selected row is already held by the claiming generation itself.
    /// Re-claiming it would hand one generation two claims over one row
    /// (ADR 0029), so the attempt defers: SQL backends roll the claim
    /// transaction back and the in-memory store stages nothing.
    Defer,
    /// Every selected row is claimable: execute the plan's ordered writes.
    Complete(Plan),
}

/// One selected queued-work row's observation under the backend's claim
/// authority (FIG-1065).
///
/// PostgreSQL read the row `FOR UPDATE … SKIP LOCKED`, SQLite read it inside
/// its `BEGIN IMMEDIATE` write transaction, and the in-memory store holds it
/// under its write lock. The planner decides over these fields alone.
#[derive(Debug)]
pub struct QueuedWorkClaimRow {
    /// The selection's candidate view of the row: identity, durable order,
    /// and the interrupted predecessor identity an abandon must restore.
    pub candidate: ClaimCandidate,
    /// The hydrated batch the claim record carries.
    pub batch: crate::QueuedWorkBatch,
    /// The row's live claim token, or `None` when unclaimed. A restored
    /// predecessor token counts as present; it pairs with generation `0` and
    /// is never live.
    pub claim_token: Option<String>,
    /// The session-execution-lease generation the row's claim pins (`0` on a
    /// released or restored row).
    pub claim_session_lease_generation: u64,
}

impl QueuedWorkClaimRow {
    fn claim_facts(&self) -> WorkRowClaimFacts<'_> {
        WorkRowClaimFacts {
            claim_token: self.claim_token.as_deref(),
            claim_session_lease_generation: self.claim_session_lease_generation,
        }
    }
}

/// One selected pending-turn-input row's observation under the backend's
/// claim authority (FIG-1065).
#[derive(Debug)]
pub struct TurnInputClaimRow {
    /// The hydrated input the claim record carries; an active-turn claim
    /// transitions its state to `accepted`.
    pub input: crate::PendingTurnInput,
    /// The row's durable enqueue order; the head row's value seeds the claim
    /// id.
    pub enqueue_seq: u64,
    /// The row's current claim-fencing token; the claim writes its successor.
    pub claim_fencing_token: u64,
    /// The row's live claim token, or `None` when unclaimed.
    pub claim_token: Option<String>,
    /// The session-execution-lease generation the row's claim pins.
    pub claim_session_lease_generation: u64,
}

impl TurnInputClaimRow {
    fn claim_facts(&self) -> WorkRowClaimFacts<'_> {
        WorkRowClaimFacts {
            claim_token: self.claim_token.as_deref(),
            claim_session_lease_generation: self.claim_session_lease_generation,
        }
    }
}

/// One row's claim write: install the claim columns and advance the fencing
/// token (FIG-1065).
#[derive(Clone, Debug)]
pub struct QueuedWorkClaimWrite {
    /// The row identity the backend's claim statement names.
    pub batch_id: crate::BatchId,
    /// The fencing token the write installs: the observed token's successor.
    pub next_claim_fencing_token: u64,
}

/// The ordered plan a `Complete` claim decision carries for queued work:
/// one claim write per selected row in selection order, then the claim
/// record (FIG-1065).
#[derive(Debug)]
pub struct QueuedWorkClaimPlan {
    session_id: SessionId,
    owner: LeaseOwnerIdentity,
    lease: WorkClaimLease,
    writes: Vec<QueuedWorkClaimWrite>,
    batches: Vec<crate::QueuedWorkBatch>,
    abandon_restore_claim_id: Option<String>,
    abandon_restore_claim_token: Option<String>,
}

impl QueuedWorkClaimPlan {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn owner(&self) -> &LeaseOwnerIdentity {
        &self.owner
    }

    /// The claim identity every write installs.
    pub fn claim_id(&self) -> &str {
        self.lease.claim_id.as_str()
    }

    /// The lease token every write installs.
    pub fn lease_token(&self) -> &str {
        self.lease.lease_token.as_str()
    }

    /// The session-execution-lease generation every write pins.
    pub fn session_lease_generation(&self) -> u64 {
        self.lease.session_lease_generation
    }

    /// The claim's own fencing token — the head row's successor token.
    pub fn fencing_token(&self) -> u64 {
        self.lease.fencing_token
    }

    /// The ordered claim writes, one per selected row.
    pub fn writes(&self) -> &[QueuedWorkClaimWrite] {
        &self.writes
    }

    /// The claim record the committed attempt reports.
    pub fn into_claim(self) -> Result<crate::QueuedWorkClaim, StoreError> {
        Ok(crate::QueuedWorkClaim {
            session_id: self.session_id,
            claim_id: self.lease.claim_id,
            owner: self.owner,
            lease_token: self.lease.lease_token,
            fencing_token: self.lease.fencing_token,
            session_lease_generation: self.lease.session_lease_generation,
            data: crate::store_backend_support::queued_work_claim_data(
                self.batches,
                self.abandon_restore_claim_id,
                self.abandon_restore_claim_token,
            )?,
        })
    }
}

/// One row's claim write for a turn-input claim (FIG-1065).
#[derive(Clone, Debug)]
pub struct TurnInputClaimWrite {
    /// The row identity the backend's claim statement names.
    pub input_id: crate::InputId,
    /// The fencing token the write installs: the observed token's successor.
    pub next_claim_fencing_token: u64,
    /// The state this row's ingress permits after acquiring the claim.
    pub state_after_claim: crate::TurnInputStateKind,
}

/// The ordered plan a `Complete` claim decision carries for pending turn
/// inputs: one claim write per selected row, then the claim record
/// (FIG-1065).
#[derive(Debug)]
pub struct TurnInputClaimPlan {
    session_id: SessionId,
    owner: LeaseOwnerIdentity,
    lease: WorkClaimLease,
    mode: crate::TurnInputClaimMode,
    writes: Vec<TurnInputClaimWrite>,
    inputs: Vec<crate::PendingTurnInput>,
}

impl TurnInputClaimPlan {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn owner(&self) -> &LeaseOwnerIdentity {
        &self.owner
    }

    /// The claim identity every write installs.
    pub fn claim_id(&self) -> &str {
        self.lease.claim_id.as_str()
    }

    /// The lease token every write installs.
    pub fn lease_token(&self) -> &str {
        self.lease.lease_token.as_str()
    }

    /// The session-execution-lease generation every write pins.
    pub fn session_lease_generation(&self) -> u64 {
        self.lease.session_lease_generation
    }

    /// The ordered claim writes, one per selected row.
    pub fn writes(&self) -> &[TurnInputClaimWrite] {
        &self.writes
    }

    /// The claim record the committed attempt reports.
    pub fn into_claim(self) -> crate::turn_input_vocabulary::TurnInputClaim {
        crate::turn_input_vocabulary::TurnInputClaim {
            session_id: self.session_id,
            claim_id: self.lease.claim_id,
            owner: self.owner,
            lease_token: self.lease.lease_token,
            fencing_token: self.lease.fencing_token,
            session_lease_generation: self.lease.session_lease_generation,
            data: crate::TurnInputClaimData {
                mode: self.mode,
                inputs: self.inputs,
                applications: Vec::new(),
            },
        }
    }
}

/// The durable state a claimed row transitions to, per claim mode.
fn turn_input_state_after_claim(mode: &crate::TurnInputClaimMode) -> crate::TurnInputStateKind {
    match mode {
        crate::TurnInputClaimMode::ActiveTurn { .. } => crate::TurnInputStateKind::Accepted,
        crate::TurnInputClaimMode::NextTurn => crate::TurnInputStateKind::DeferredNextTurn,
    }
}

/// Decide one queued-work claim attempt over its selected rows (FIG-1065).
///
/// `rows` is the write set — the resolved selection in claim order.
/// `validation_span` is the candidate span the fencing-token check covers:
/// exact claims pass their full interrupted or contiguous span, including
/// candidates outside the selected prefix, because an overflowing token
/// anywhere in the span means the claim cannot be named safely.
///
/// Evaluation order matches the hand-written bodies this replaces: the claim
/// lease derives from the head row first, the span's tokens validate next,
/// and only then is each row's claimability verdict taken. `Defer` therefore
/// reports the same fact the rolled-back transaction used to.
#[allow(clippy::too_many_arguments)]
pub fn plan_queued_work_claim(
    dialect: ClaimIdDialect,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    claiming_generation: u64,
    now_epoch_ms: u64,
    rows: Vec<QueuedWorkClaimRow>,
    validation_span: &[ClaimCandidate],
) -> Result<ClaimPlanDecision<QueuedWorkClaimPlan>, StoreError> {
    let Some(head) = rows.first() else {
        return Ok(ClaimPlanDecision::Empty);
    };
    let lease = WorkClaimLease::derive(
        dialect,
        head.candidate.enqueue_seq,
        head.candidate.claim_fencing_token,
        session_id,
        owner,
        now_epoch_ms,
        claiming_generation,
    )?;
    for token in validation_span {
        StoreError::checked_monotonic_increment(
            "queued_work_claim_fencing_token",
            token.claim_fencing_token,
        )?;
    }
    let abandon_restore_claim_id = head.candidate.prior_claim_id.clone();
    let abandon_restore_claim_token = head.candidate.prior_claim_token.clone();
    let writes = rows
        .iter()
        .map(|row| {
            Ok(QueuedWorkClaimWrite {
                batch_id: row.candidate.batch_id.clone(),
                next_claim_fencing_token: StoreError::checked_monotonic_increment(
                    "queued_work_claim_fencing_token",
                    row.candidate.claim_fencing_token,
                )?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let mut batches = Vec::with_capacity(rows.len());
    for row in rows {
        // The row was read under the backend's claim authority, so it cannot
        // move before the claim write below. The shared verdict decides; the
        // write-side copy of this predicate stays on the statement as its
        // backstop.
        if !queued_work_batch_claimability(row.claim_facts(), claiming_generation).is_claimable() {
            return Ok(ClaimPlanDecision::Defer);
        }
        batches.push(row.batch);
    }
    Ok(ClaimPlanDecision::Complete(QueuedWorkClaimPlan {
        session_id: SessionId::from(session_id.to_string()),
        owner: owner.clone(),
        lease,
        writes,
        batches,
        abandon_restore_claim_id,
        abandon_restore_claim_token,
    }))
}

/// Decide one pending-turn-input claim attempt over its selected rows
/// (FIG-1065). Same evaluation order as [`plan_queued_work_claim`].
#[allow(clippy::too_many_arguments)]
pub fn plan_turn_input_claim(
    dialect: ClaimIdDialect,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    claiming_generation: u64,
    now_epoch_ms: u64,
    mode: crate::TurnInputClaimMode,
    rows: Vec<TurnInputClaimRow>,
) -> Result<ClaimPlanDecision<TurnInputClaimPlan>, StoreError> {
    let Some(head) = rows.first() else {
        return Ok(ClaimPlanDecision::Empty);
    };
    let lease = WorkClaimLease::derive(
        dialect,
        head.enqueue_seq,
        head.claim_fencing_token,
        session_id,
        owner,
        now_epoch_ms,
        claiming_generation,
    )?;
    let mode_state_after_claim = turn_input_state_after_claim(&mode);
    let writes = rows
        .iter()
        .map(|row| {
            Ok(TurnInputClaimWrite {
                input_id: row.input.input_id.clone(),
                next_claim_fencing_token: StoreError::checked_monotonic_increment(
                    "turn_input_claim_fencing_token",
                    row.claim_fencing_token,
                )?,
                // A frozen queued-run member keeps its active-turn ingress
                // across lease generations. NextTurn describes the selection
                // boundary, not a rewrite of that member's admission scope.
                state_after_claim: if matches!(&mode, crate::TurnInputClaimMode::NextTurn)
                    && row.input.state.active_turn_id().is_some()
                {
                    crate::TurnInputStateKind::Accepted
                } else {
                    mode_state_after_claim
                },
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let mut inputs = Vec::with_capacity(rows.len());
    for (row, write) in rows.into_iter().zip(&writes) {
        // Same contract as the queued-work claim: the row was read under the
        // backend's claim authority, the shared verdict decides, and the
        // generation predicate stays on the claim statement as its backstop.
        if !turn_input_claimability(row.claim_facts(), claiming_generation).is_claimable() {
            return Ok(ClaimPlanDecision::Defer);
        }
        let mut input = row.input;
        if write.state_after_claim == crate::TurnInputStateKind::Accepted
            && let Some(accepted) = input.state.accepted()
        {
            input.state = accepted;
        }
        inputs.push(input);
    }
    Ok(ClaimPlanDecision::Complete(TurnInputClaimPlan {
        session_id: SessionId::from(session_id.to_string()),
        owner: owner.clone(),
        lease,
        mode,
        writes,
        inputs,
    }))
}

/// Classify the refusal behind an empty candidate scan (FIG-1065).
///
/// The candidate query enforces the delivery-boundary rule in SQL, so a scan
/// that comes back empty tells the shared claim state machine nothing. Asking
/// it again with the unfiltered ready head keeps the classification in one
/// place: whatever the head alone is refused for is what this claim is
/// refused for. With no ready head at all, a lane still holding deferred work
/// is not an exhausted lane.
pub fn classify_empty_claim_scan(
    head_candidates: &[ClaimCandidate],
    deferred_row_pending: bool,
    boundary: crate::QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<TurnWorkEmptyScanDiagnostic, StoreError> {
    if !head_candidates.is_empty() {
        return Ok(TurnWorkEmptyScanDiagnostic::from(
            select_turn_work_claim_prefix(head_candidates, boundary, policy, now_epoch_ms)?,
        ));
    }
    Ok(TurnWorkEmptyScanDiagnostic::Refused {
        reason: if deferred_row_pending {
            QueuedWorkClaimRefusal::NotYetAvailable
        } else {
            QueuedWorkClaimRefusal::Empty
        },
    })
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

/// The shared settlement decision for one completed claim (FIG-1065).
#[derive(Debug)]
pub enum SettlementDecision<Plan> {
    /// Every covered row still carries the completing claim's authority:
    /// execute the plan's ordered writes.
    Complete(Plan),
    /// A covered row is absent or another claim holds it: the commit fails
    /// with the carried diagnostics.
    Superseded(StoreError),
}

impl<Plan> SettlementDecision<Plan> {
    /// Lower the decision to the result the backend's transaction body needs.
    pub fn into_result(self) -> Result<Plan, StoreError> {
        match self {
            Self::Complete(plan) => Ok(plan),
            Self::Superseded(error) => Err(error),
        }
    }
}

/// The live claim columns a present queued-work settlement row carries
/// (FIG-1065).
#[derive(Clone, Debug)]
pub struct QueuedWorkSettlementRowClaim {
    pub claim_id: Option<String>,
    pub claim_token: Option<String>,
    pub claim_session_lease_generation: u64,
}

/// The process wake a batch carried when it left the queue, which its
/// session's redelivery fence must record (FIG-1065, FIG-3545).
///
/// Every terminal transition of a wake row — claim settlement and host
/// cancel alike — raises the fence to `max(floor, sequence)` in the same
/// transaction that removes the row. Otherwise a redelivery of the same
/// `(process, sequence)` after a producer crash, a failed terminal mark or a
/// lost claim finds neither a row nor a floor and re-admits the wake.
///
/// A process-wake batch is validated at enqueue to carry exactly one wake
/// payload, so "the wake the batch carried" is one fact no matter how a
/// backend reads it: the SQL settlements decode the head payload, the
/// in-memory store and every cancel read the hydrated batch.
#[derive(Clone, Debug)]
pub struct TerminalProcessWake {
    /// The batch's source key. PostgreSQL advisory-locks this identity before
    /// writing the fence; backends without advisory locks may leave it `None`.
    pub source_key: Option<String>,
    /// Structural producer identity the fence indexes on.
    pub process_id: crate::ProcessId,
    /// The terminal sequence the allocation floor rises to.
    pub sequence: u64,
}

impl TerminalProcessWake {
    /// The wake `payload` carries, if it is a process wake, under the batch's
    /// `source_key`.
    pub fn of_payload(
        source_key: Option<String>,
        payload: &crate::QueuedWorkPayload,
    ) -> Option<Self> {
        match payload {
            crate::QueuedWorkPayload::ProcessWake { wake } => Some(Self {
                source_key,
                process_id: wake.process_id.clone(),
                sequence: wake.sequence,
            }),
            crate::QueuedWorkPayload::SessionCommand { .. } => None,
        }
    }

    /// The wake a hydrated batch carries, if any.
    pub fn of_batch(batch: &crate::QueuedWorkBatch) -> Option<Self> {
        batch
            .items
            .iter()
            .find_map(|item| Self::of_payload(batch.source_key.clone(), &item.payload))
    }
}

/// One covered queued-work row's observation under the backend's commit
/// authority (FIG-1065).
pub struct QueuedWorkSettlementRow {
    /// The row identity the completion names.
    pub batch_id: crate::BatchId,
    /// The row's live claim columns; `None` when no row holds the identity.
    pub claim: Option<QueuedWorkSettlementRowClaim>,
    /// The wake the settlement must record in the session's redelivery
    /// fence before the row leaves the queue, when the batch carried one.
    pub terminal_wake: Option<TerminalProcessWake>,
}

/// One ordered write a queued-work settlement plan prescribes (FIG-1065).
#[derive(Clone, Debug)]
pub enum QueuedWorkSettlementWrite {
    /// Raise the session's redelivery fence for the settled wake.
    ///
    /// This write lands before the queue row leaves: a crash between the two
    /// would replay a wake the session already consumed. PostgreSQL takes the
    /// wake source's advisory lock immediately before executing it; SQLite
    /// executes it inside the commit's serialized write transaction; the
    /// in-memory store stages the floor update.
    FenceWakeRedelivery {
        /// The covered row whose wake the fence records.
        batch_id: crate::BatchId,
        /// The wake identity the floor rises to.
        wake: TerminalProcessWake,
    },
    /// Remove the settled batch row under the completion's claim authority.
    SettleClaimedBatch { batch_id: crate::BatchId },
}

/// The ordered settlement plan for one completed queued-work claim
/// (FIG-1065): for each covered row, the redelivery fence first — when the
/// batch carried a wake — then the row's removal.
#[derive(Debug)]
pub struct QueuedWorkSettlementPlan {
    session_id: SessionId,
    claim_id: String,
    lease_token: String,
    writes: Vec<QueuedWorkSettlementWrite>,
}

impl QueuedWorkSettlementPlan {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The claim identity the settlement writes predicate on.
    pub fn claim_id(&self) -> &str {
        self.claim_id.as_str()
    }

    /// The lease token the settlement writes predicate on.
    pub fn lease_token(&self) -> &str {
        self.lease_token.as_str()
    }

    /// The ordered writes, `FenceWakeRedelivery` before `SettleClaimedBatch`
    /// for each covered row.
    pub fn writes(&self) -> &[QueuedWorkSettlementWrite] {
        &self.writes
    }

    /// The site's fail-closed error when a conditional settle write misses.
    ///
    /// The verdict already authorized the write over the locked observation,
    /// so a miss means the predicate and the verdict disagree — recorded as
    /// evidence by the caller, reported with the supersession diagnostics the
    /// write-time check can still know.
    pub fn superseded_error(&self, batch_id: &crate::BatchId) -> StoreError {
        StoreError::QueuedWorkClaimSuperseded {
            session_id: self.session_id.clone(),
            claim_id: self.claim_id.clone(),
            row_id: Some(batch_id.as_str().to_string().into_boxed_str()),
            superseding_claim_id: None,
            superseding_session_lease_generation: None,
        }
    }
}

/// The live claim columns and state a present pending-turn-input settlement
/// row carries (FIG-1065).
#[derive(Clone, Debug)]
pub struct TurnInputSettlementRowFacts {
    pub claim_id: Option<String>,
    pub claim_token: Option<String>,
    pub claim_session_lease_generation: u64,
    /// The row's persisted `state` spelling.
    pub state: String,
}

/// One covered pending-turn-input row's observation under the backend's
/// commit authority (FIG-1065).
#[derive(Debug)]
pub struct TurnInputSettlementRow {
    /// The row identity the completion names.
    pub input_id: crate::InputId,
    /// The row's live claim columns and state; `None` when absent.
    pub facts: Option<TurnInputSettlementRowFacts>,
}

/// Which conditional write a turn-input settlement step executes
/// (ADR 0069 §5): one question, two regimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnInputSettlementRegime {
    /// The claim id/token strengthen the write's predicate.
    Claimed,
    /// The row must still be unclaimed and nonterminal.
    Unclaimed,
}

/// One step of a turn-input settlement plan (FIG-1065).
#[derive(Clone, Debug)]
pub struct TurnInputSettlementStep {
    /// The covered row this step settles.
    pub input_id: crate::InputId,
    /// The write regime the plan decided.
    pub regime: TurnInputSettlementRegime,
    /// The durable `state` the write installs.
    pub settle_state: crate::TurnInputStateKind,
}

/// The ordered settlement plan for one completed turn-input claim
/// (FIG-1065): one state transition per covered row, in the completion's
/// order.
#[derive(Debug)]
pub struct TurnInputSettlementPlan {
    session_id: SessionId,
    claim: Option<crate::TurnInputSettlementClaim>,
    steps: Vec<TurnInputSettlementStep>,
}

impl TurnInputSettlementPlan {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The claim authority the `Claimed` regime's writes predicate on;
    /// `None` for an unclaimed settlement.
    pub fn claim(&self) -> Option<&crate::TurnInputSettlementClaim> {
        self.claim.as_ref()
    }

    /// The ordered settlement steps, one per covered row.
    pub fn steps(&self) -> &[TurnInputSettlementStep] {
        &self.steps
    }

    /// The fenced-write label a step's backstop reports under.
    pub fn fenced_write(step: &TurnInputSettlementStep) -> FencedWrite {
        match step.regime {
            TurnInputSettlementRegime::Claimed => FencedWrite::TurnInputClaimSettlement,
            TurnInputSettlementRegime::Unclaimed => FencedWrite::UnclaimedTurnInputSettlement,
        }
    }

    /// The site's fail-closed error when a conditional settle write misses.
    pub fn superseded_error(&self, step: &TurnInputSettlementStep) -> StoreError {
        match self.claim.as_ref() {
            Some(claim) => StoreError::TurnInputClaimSuperseded {
                session_id: self.session_id.clone(),
                claim_id: claim.claim_id.clone(),
                row_id: Some(step.input_id.as_str().to_string().into_boxed_str()),
                superseding_claim_id: None,
                superseding_session_lease_generation: None,
            },
            None => StoreError::UnclaimedTurnInputSettlementSuperseded {
                session_id: self.session_id.clone(),
                input_id: step.input_id.clone(),
                observed_state: None,
                superseding_claim_id: None,
            },
        }
    }
}

/// Decide one queued-work claim's settlement over its covered rows
/// (FIG-1065).
///
/// `rows` must cover `completed.batch_ids` in order; any other shape is a
/// backend contract violation, reported as such rather than settled
/// partially.
pub fn plan_queued_work_settlement(
    completed: &crate::QueuedWorkCompletion,
    rows: Vec<QueuedWorkSettlementRow>,
) -> SettlementDecision<QueuedWorkSettlementPlan> {
    if rows.len() != completed.batch_ids.len() {
        return SettlementDecision::Superseded(StoreError::Backend(format!(
            "queued-work settlement observed {} rows for {} covered batches",
            rows.len(),
            completed.batch_ids.len(),
        )));
    }
    let mut writes = Vec::with_capacity(rows.len() * 2);
    for row in &rows {
        let facts = row.claim.as_ref().map(|claim| QueuedWorkSettlementFacts {
            claim_id: claim.claim_id.as_deref(),
            claim_token: claim.claim_token.as_deref(),
            claim_session_lease_generation: claim.claim_session_lease_generation,
        });
        if let Err(error) = require_settleable_queued_work(completed, row.batch_id.as_str(), facts)
        {
            return SettlementDecision::Superseded(error);
        }
        if let Some(wake) = row.terminal_wake.as_ref() {
            writes.push(QueuedWorkSettlementWrite::FenceWakeRedelivery {
                batch_id: row.batch_id.clone(),
                wake: wake.clone(),
            });
        }
        writes.push(QueuedWorkSettlementWrite::SettleClaimedBatch {
            batch_id: row.batch_id.clone(),
        });
    }
    SettlementDecision::Complete(QueuedWorkSettlementPlan {
        session_id: completed.session_id.clone(),
        claim_id: completed.claim_id.clone(),
        lease_token: completed.lease_token.clone(),
        writes,
    })
}

/// Decide one turn-input claim's settlement over its covered rows
/// (FIG-1065). Same coverage contract as [`plan_queued_work_settlement`].
pub fn plan_turn_input_settlement(
    completed: &crate::TurnInputCompletion,
    rows: Vec<TurnInputSettlementRow>,
) -> SettlementDecision<TurnInputSettlementPlan> {
    if rows.len() != completed.input_ids.len() {
        return SettlementDecision::Superseded(StoreError::Backend(format!(
            "turn-input settlement observed {} rows for {} covered inputs",
            rows.len(),
            completed.input_ids.len(),
        )));
    }
    let regime = match completed.claim.as_ref() {
        Some(_) => TurnInputSettlementRegime::Claimed,
        None => TurnInputSettlementRegime::Unclaimed,
    };
    let mut steps = Vec::with_capacity(rows.len());
    for row in &rows {
        let facts = row.facts.as_ref().map(|facts| TurnInputSettlementFacts {
            claim_id: facts.claim_id.as_deref(),
            claim_token: facts.claim_token.as_deref(),
            claim_session_lease_generation: facts.claim_session_lease_generation,
            state: facts.state.as_str(),
        });
        if let Err(error) = require_settleable_turn_input(completed, &row.input_id, facts) {
            return SettlementDecision::Superseded(error);
        }
        steps.push(TurnInputSettlementStep {
            input_id: row.input_id.clone(),
            regime,
            settle_state: crate::TurnInputStateKind::Completed,
        });
    }
    SettlementDecision::Complete(TurnInputSettlementPlan {
        session_id: completed.session_id.clone(),
        claim: completed.claim.clone(),
        steps,
    })
}
