//! The claim, settlement and withdrawal planners every session-ingress
//! backend shares (ADR 0101).
//!
//! A backend observes the rows an operation reads under its own lock — the
//! SQLite `BEGIN IMMEDIATE` write transaction, or the PostgreSQL session
//! advisory lock every ingress writer takes — and hands them here in
//! `enqueue_seq` order. The planner decides which rows move and how; the
//! backend executes the planned writes with its own statements, whose
//! conditional predicates stay as write backstops.

use super::{
    AdmissionId, DriveFence, IngressClaimPolicy, IngressClaimRef, IngressClaimSettlement,
    IngressCommandResult, IngressSettlementIntent, IngressTurnCancel, StoreError,
};
use crate::session_ingress_vocabulary::{
    ClaimMode, Delivery, IngressAffectedItem, IngressCancelReason, IngressClaim,
    IngressClaimIdentity, IngressItem, IngressItemId, IngressKind, IngressTerminalCause,
    IngressUndeliveredDisposition, IngressWithdrawSelector,
};
use crate::{ProcessId, SessionId, TurnId};

/// The most `ApplyConfigPatch` commands one head commit coalesces.
pub const MAX_COALESCED_CONFIG_PATCHES: usize = 64;

/// A claim's persisted identity on one observed row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressRowClaim {
    pub identity: IngressClaimIdentity,
    /// The drive epoch the claim pins.
    pub drive_epoch: u64,
    /// The turn a checkpoint claim delivers into; `None` for an idle claim.
    pub claim_turn_id: Option<TurnId>,
}

/// One non-terminal row a claim attempt observed, in `enqueue_seq` order.
#[derive(Clone, Debug)]
pub struct IngressClaimCandidate {
    pub item: IngressItem,
    /// The row's current claim-fencing token; a claim writes its successor.
    pub claim_fencing_token: u64,
    pub claim: Option<IngressRowClaim>,
    /// For a `Turn{T}` item: whether T's final commit is recorded, which makes
    /// it `NextTurn` by rule (ADR 0101 §5.1).
    pub addressed_turn_ended: bool,
    /// For a row an interrupted checkpoint claim of turn T holds: whether T's
    /// final commit is recorded, so no redrive of T will re-take it.
    pub claim_turn_ended: bool,
}

/// How a claim attempt sees one row's hold.
enum RowHold<'a> {
    Free,
    /// Held by a claim pinning the claimant's own, current drive epoch.
    Live,
    /// Held by a claim whose drive epoch a later admission superseded.
    Interrupted(&'a IngressRowClaim),
}

impl IngressClaimCandidate {
    fn hold(&self, drive_epoch: u64) -> RowHold<'_> {
        match &self.claim {
            None => RowHold::Free,
            Some(claim) if claim.drive_epoch == drive_epoch => RowHold::Live,
            Some(claim) => RowHold::Interrupted(claim),
        }
    }

    fn projected_tokens(&self) -> usize {
        serde_json::to_vec(&self.item.payload).map_or(usize::MAX, |bytes| bytes.len())
    }
}

/// Derive a claim's token from the session, the admission and the claim id
/// alone. No clock enters it, so a replay of the claim step re-derives the
/// same token.
#[must_use]
pub fn derive_ingress_claim_token(
    session_id: &SessionId,
    admission: &AdmissionId,
    claim_id: &str,
) -> String {
    crate::stable_hash::blake3_hex(
        "lash-session-ingress-claim/v1",
        format!(
            "{}:{}:{}:{}:{}",
            session_id.as_str().len(),
            session_id,
            admission.as_str().len(),
            admission.as_str(),
            claim_id
        )
        .as_bytes(),
    )
}

/// The claim authority a plan installs on every row it takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressClaimStamp {
    pub claim_id: String,
    pub claim_token: String,
    /// The claim's own fencing token: the head row's successor token.
    pub fencing_token: u64,
    pub drive_epoch: u64,
    pub admission: AdmissionId,
}

impl IngressClaimStamp {
    fn derive(head: &IngressClaimCandidate, fence: &DriveFence) -> Result<Self, StoreError> {
        let fencing_token = StoreError::checked_monotonic_increment(
            "session_ingress_claim_fencing_token",
            head.claim_fencing_token,
        )?;
        let claim_id = format!("ingc:{}:{fencing_token}", head.item.enqueue_seq);
        let claim_token = derive_ingress_claim_token(fence.session(), fence.admission(), &claim_id);
        Ok(Self {
            claim_id,
            claim_token,
            fencing_token,
            drive_epoch: fence.epoch(),
            admission: fence.admission().clone(),
        })
    }
}

/// One row's claim write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressClaimWrite {
    pub item_id: IngressItemId,
    /// The fencing token the observed row carried, the write's backstop.
    pub observed_claim_fencing_token: u64,
    /// The fencing token the write installs: the observed token's successor.
    pub next_claim_fencing_token: u64,
}

/// A claim attempt's plan: install the claim on each written row, in order.
#[derive(Clone, Debug)]
pub struct IngressClaimPlan {
    session_id: SessionId,
    mode: ClaimMode,
    stamp: IngressClaimStamp,
    writes: Vec<IngressClaimWrite>,
    items: Vec<IngressItem>,
    predecessor: Option<IngressClaimIdentity>,
}

impl IngressClaimPlan {
    #[must_use]
    pub fn stamp(&self) -> &IngressClaimStamp {
        &self.stamp
    }

    /// The turn a checkpoint claim delivers into, persisted on each row so a
    /// later claim can tell whose redrive owns an interrupted hold.
    #[must_use]
    pub fn claim_turn_id(&self) -> Option<&TurnId> {
        self.mode.turn_id()
    }

    #[must_use]
    pub fn writes(&self) -> &[IngressClaimWrite] {
        &self.writes
    }

    /// The claim the committed attempt reports.
    #[must_use]
    pub fn into_claim(self) -> IngressClaim {
        let mut items = self.items;
        items.sort_by_key(|item| item.enqueue_seq);
        IngressClaim {
            session_id: self.session_id,
            claim_id: self.stamp.claim_id,
            claim_token: self.stamp.claim_token,
            fencing_token: self.stamp.fencing_token,
            drive_epoch: self.stamp.drive_epoch,
            admission: self.stamp.admission,
            mode: self.mode,
            items,
            predecessor: self.predecessor,
        }
    }
}

/// The inputs one claim attempt decides from. The backend has already
/// checked, in the same transaction, that `fence` is the session's current
/// drive fence.
pub struct IngressClaimAttempt<'a> {
    pub fence: &'a DriveFence,
    /// Read only for the claim policy's pending-age bound; never an order key
    /// and never part of the claim identity.
    pub now_epoch_ms: u64,
}

impl IngressClaimAttempt<'_> {
    fn drive_epoch(&self) -> u64 {
        self.fence.epoch()
    }
}

struct Selection {
    taken: Vec<usize>,
    predecessor: Option<IngressClaimIdentity>,
}

impl Selection {
    fn fresh() -> Self {
        Self {
            taken: Vec::new(),
            predecessor: None,
        }
    }

    /// Every row carrying the interrupted claim `identity`, in order: the
    /// exact composition its drive already committed to (ADR 0101 §7).
    fn exact(candidates: &[IngressClaimCandidate], identity: &IngressClaimIdentity) -> Self {
        Self {
            taken: candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate
                        .claim
                        .as_ref()
                        .is_some_and(|claim| &claim.identity == identity)
                })
                .map(|(index, _)| index)
                .collect(),
            predecessor: Some(identity.clone()),
        }
    }
}

fn build_plan(
    attempt: &IngressClaimAttempt<'_>,
    mode: ClaimMode,
    candidates: &[IngressClaimCandidate],
    selection: Selection,
) -> Result<Option<IngressClaimPlan>, StoreError> {
    let mut taken = selection.taken;
    taken.sort_by_key(|index| candidates[*index].item.enqueue_seq);
    let Some(head) = taken.first() else {
        return Ok(None);
    };
    let stamp = IngressClaimStamp::derive(&candidates[*head], attempt.fence)?;
    let mut writes = Vec::with_capacity(taken.len());
    let mut items = Vec::with_capacity(taken.len());
    for index in taken {
        let candidate = &candidates[index];
        writes.push(IngressClaimWrite {
            item_id: candidate.item.item_id.clone(),
            observed_claim_fencing_token: candidate.claim_fencing_token,
            next_claim_fencing_token: StoreError::checked_monotonic_increment(
                "session_ingress_claim_fencing_token",
                candidate.claim_fencing_token,
            )?,
        });
        items.push(candidate.item.clone());
    }
    Ok(Some(IngressClaimPlan {
        session_id: attempt.fence.session().clone(),
        mode,
        stamp,
        writes,
        items,
        predecessor: selection.predecessor,
    }))
}

/// Plan a command-lane claim over the lane's non-terminal rows, in order
/// (ADR 0101 §4).
///
/// The head is claimed alone unless it is an `ApplyConfigPatch`, in which case
/// every adjacent free config patch joins it, up to
/// [`MAX_COALESCED_CONFIG_PATCHES`]. A head held by the claimant's own
/// drive epoch yields nothing; a head an interrupted claim holds is re-derived
/// exactly.
pub fn plan_command_claim(
    attempt: &IngressClaimAttempt<'_>,
    candidates: &[IngressClaimCandidate],
) -> Result<Option<IngressClaimPlan>, StoreError> {
    let Some(head) = candidates.first() else {
        return Ok(None);
    };
    let selection = match head.hold(attempt.drive_epoch()) {
        RowHold::Live => return Ok(None),
        RowHold::Interrupted(claim) => Selection::exact(candidates, &claim.identity),
        RowHold::Free => {
            let mut selection = Selection::fresh();
            if head.item.payload.is_config_patch() {
                for (index, candidate) in candidates.iter().enumerate() {
                    if selection.taken.len() == MAX_COALESCED_CONFIG_PATCHES
                        || !candidate.item.payload.is_config_patch()
                        || !matches!(candidate.hold(attempt.drive_epoch()), RowHold::Free)
                    {
                        break;
                    }
                    selection.taken.push(index);
                }
            } else {
                selection.taken.push(0);
            }
            selection
        }
    };
    build_plan(attempt, ClaimMode::Idle, candidates, selection)
}

/// What a resumed claim's reclaim does (FIG-3552).
#[derive(Debug)]
pub enum IngressReclaimDecision {
    /// Every row already carries the claim at the claimant's drive epoch, as
    /// stored: the claim as the rows record it.
    Held(Box<IngressClaim>),
    /// Move every row to one new claim under the claimant's drive epoch.
    Reclaim(Box<IngressClaimPlan>),
    /// A peer superseded a row through the claim CAS: cede, write nothing.
    Ceded,
}

/// Plan the reclaim of `claim` by a resumed run. `observed` holds the current
/// state of every row of `claim` that still exists in its session.
///
/// Ownership moves only through the claim CAS, and only whole: the reclaim
/// moves every row or none.
pub fn plan_reclaim(
    attempt: &IngressClaimAttempt<'_>,
    claim: &IngressClaim,
    observed: &[IngressClaimCandidate],
) -> Result<IngressReclaimDecision, StoreError> {
    let identity = claim.identity();
    let owns_every_row = observed.len() == claim.items.len()
        && claim.items.iter().all(|item| {
            observed.iter().any(|row| {
                row.item.item_id == item.item_id
                    && !row.item.state.is_terminal()
                    && row
                        .claim
                        .as_ref()
                        .is_some_and(|held| held.identity == identity)
            })
        });
    if !owns_every_row {
        return Ok(IngressReclaimDecision::Ceded);
    }
    // Whether the claim is already this epoch's is the stored rows' answer,
    // never the caller's copy of the claim.
    if observed.iter().all(|row| {
        row.claim
            .as_ref()
            .is_some_and(|held| held.drive_epoch == attempt.drive_epoch())
    }) {
        return Ok(IngressReclaimDecision::Held(Box::new(IngressClaim {
            drive_epoch: attempt.drive_epoch(),
            admission: attempt.fence.admission().clone(),
            items: observed.iter().map(|row| row.item.clone()).collect(),
            ..claim.clone()
        })));
    }
    let selection = Selection {
        taken: (0..observed.len()).collect(),
        predecessor: Some(identity),
    };
    Ok(
        build_plan(attempt, claim.mode.clone(), observed, selection)?
            .map_or(IngressReclaimDecision::Ceded, |plan| {
                IngressReclaimDecision::Reclaim(Box::new(plan))
            }),
    )
}

struct Caps {
    inputs: usize,
    wakes: usize,
    items: usize,
    tokens: usize,
}

impl Caps {
    fn new() -> Self {
        Self {
            inputs: 0,
            wakes: 0,
            items: 0,
            tokens: 0,
        }
    }

    /// Whether `candidate` fits beside what is already taken. The first row
    /// of a claim always fits the token bound, so an oversized row is
    /// claimed alone rather than never.
    fn admit(&mut self, candidate: &IngressClaimCandidate, policy: &IngressClaimPolicy) -> bool {
        let tokens = candidate.projected_tokens();
        let kind_full = match candidate.item.kind() {
            IngressKind::Input => self.inputs >= policy.max_inputs,
            IngressKind::ProcessWake => self.wakes >= policy.max_wakes,
            IngressKind::SessionCommand => true,
        };
        if kind_full
            || self.items >= policy.max_items
            || (self.items > 0 && self.tokens.saturating_add(tokens) > policy.available_tokens)
        {
            return false;
        }
        match candidate.item.kind() {
            IngressKind::Input => self.inputs += 1,
            IngressKind::ProcessWake => self.wakes += 1,
            IngressKind::SessionCommand => {}
        }
        self.items += 1;
        self.tokens = self.tokens.saturating_add(tokens);
        true
    }
}

/// Whether an unaddressed-prefix row can be delivered by `mode`, with a
/// `Turn{T}` item for an ended T counting as `NextTurn` (ADR 0101 §5.2).
fn prefix_deliverable(candidate: &IngressClaimCandidate, mode: &ClaimMode) -> bool {
    let effective_next_turn = match &candidate.item.delivery {
        Delivery::Turn { .. } if candidate.addressed_turn_ended => true,
        // A turn that has not ended is running elsewhere: never deliverable
        // outside its own checkpoints.
        Delivery::Turn { .. } => return false,
        Delivery::AnyBoundary => return true,
        Delivery::NextTurn => true,
    };
    effective_next_turn && matches!(mode, ClaimMode::Idle)
}

fn addressed_to<'a>(
    candidate: &'a IngressClaimCandidate,
    turn_id: &TurnId,
) -> Option<&'a Delivery> {
    match &candidate.item.delivery {
        delivery @ Delivery::Turn {
            turn_id: addressed, ..
        } if addressed == turn_id => Some(delivery),
        _ => None,
    }
}

/// Plan a turn-lane claim at `mode` over the lane's non-terminal rows, in
/// order (ADR 0101 §5.2).
pub fn plan_turn_claim(
    attempt: &IngressClaimAttempt<'_>,
    mode: ClaimMode,
    policy: &IngressClaimPolicy,
    candidates: &[IngressClaimCandidate],
) -> Result<Option<IngressClaimPlan>, StoreError> {
    let drive_epoch = attempt.drive_epoch();
    let mut selection = Selection::fresh();
    let mut caps = Caps::new();

    // 1. Addressed items: a checkpoint of turn t takes the open Turn{t} items
    //    its checkpoint admits, whatever precedes them.
    if let ClaimMode::Checkpoint {
        turn_id,
        checkpoint,
    } = &mode
    {
        for (index, candidate) in candidates.iter().enumerate() {
            let Some(Delivery::Turn { min_boundary, .. }) = addressed_to(candidate, turn_id) else {
                continue;
            };
            if !min_boundary.admits(*checkpoint) {
                continue;
            }
            if matches!(candidate.hold(drive_epoch), RowHold::Live) {
                continue;
            }
            if !caps.admit(candidate, policy) {
                break;
            }
            selection.taken.push(index);
        }
    }

    // 2. The FIFO prefix of unaddressed rows: stop, never skip.
    let mut prefix_head_enqueued_at: Option<u64> = None;
    let mut prefix_len = 0usize;
    for (index, candidate) in candidates.iter().enumerate() {
        if mode
            .turn_id()
            .is_some_and(|turn_id| addressed_to(candidate, turn_id).is_some())
        {
            continue;
        }
        if !prefix_deliverable(candidate, &mode) {
            break;
        }
        match candidate.hold(drive_epoch) {
            RowHold::Live => break,
            RowHold::Free => {}
            RowHold::Interrupted(claim) => {
                let owned_elsewhere = match &claim.claim_turn_id {
                    // An interrupted idle claim is re-derived only whole, and
                    // only from the head of an idle prefix.
                    None => {
                        if prefix_len == 0
                            && selection.taken.is_empty()
                            && matches!(mode, ClaimMode::Idle)
                        {
                            return build_plan(
                                attempt,
                                mode,
                                candidates,
                                Selection::exact(candidates, &claim.identity),
                            );
                        }
                        true
                    }
                    // A running turn's interrupted checkpoint hold belongs to
                    // that turn's redrive; an ended turn's is nobody's.
                    Some(held_by) => !candidate.claim_turn_ended && mode.turn_id() != Some(held_by),
                };
                if owned_elsewhere {
                    break;
                }
            }
        }
        let head_enqueued_at =
            *prefix_head_enqueued_at.get_or_insert(candidate.item.enqueued_at_ms);
        if prefix_len > 0
            && attempt.now_epoch_ms.saturating_sub(head_enqueued_at) >= policy.max_pending_age_ms
        {
            break;
        }
        if !caps.admit(candidate, policy) {
            break;
        }
        selection.taken.push(index);
        prefix_len += 1;
    }
    build_plan(attempt, mode, candidates, selection)
}

/// One observed row a settlement reads, with the claim identity it carries
/// now and the drive epoch that claim pins.
#[derive(Clone, Debug)]
pub struct IngressSettlementRow {
    pub item: IngressItem,
    pub claim: Option<IngressClaimIdentity>,
    pub claim_epoch: Option<u64>,
}

/// One planned row write of a settlement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngressRowSettlement {
    /// Tombstone the row `completed` with `cause`.
    Complete {
        item_id: IngressItemId,
        cause: IngressTerminalCause,
    },
    /// Tombstone the row `cancelled` with `reason`.
    Drop {
        item_id: IngressItemId,
        reason: IngressCancelReason,
    },
    /// Return the row to `open` at its own position, its claim cleared.
    Release { item_id: IngressItemId },
}

/// A settlement's plan: row writes in order, the wake floors every wake
/// terminal raises in the same transaction, and the affected-item records.
#[derive(Clone, Debug, Default)]
pub struct IngressSettlementPlan {
    pub writes: Vec<IngressRowSettlement>,
    pub floor_raises: Vec<(ProcessId, u64)>,
    /// Records in `enqueue_seq` order. The backend fills each dropped wake's
    /// `fence_floor_after` once it has raised the floor.
    pub affected: Vec<IngressAffectedItem>,
}

impl IngressSettlementPlan {
    fn complete(&mut self, item: &IngressItem, cause: IngressTerminalCause) {
        if let Some((process_id, sequence)) = item.payload.wake_source() {
            self.floor_raises.push((process_id.clone(), sequence));
        }
        self.writes.push(IngressRowSettlement::Complete {
            item_id: item.item_id.clone(),
            cause,
        });
    }

    fn affect(
        &mut self,
        item: &IngressItem,
        disposition: IngressUndeliveredDisposition,
        reason: IngressCancelReason,
        claimed: bool,
    ) {
        match disposition {
            IngressUndeliveredDisposition::Drop => {
                if let Some((process_id, sequence)) = item.payload.wake_source() {
                    self.floor_raises.push((process_id.clone(), sequence));
                }
                self.writes.push(IngressRowSettlement::Drop {
                    item_id: item.item_id.clone(),
                    reason: reason.clone(),
                });
            }
            IngressUndeliveredDisposition::Defer if claimed => {
                self.writes.push(IngressRowSettlement::Release {
                    item_id: item.item_id.clone(),
                });
            }
            IngressUndeliveredDisposition::Defer => {}
        }
        self.affected.push(IngressAffectedItem {
            item_id: item.item_id.clone(),
            kind: item.kind(),
            source_key: item.source_key.clone(),
            enqueue_seq: item.enqueue_seq,
            disposition,
            reason,
            payload: item.payload.clone(),
            fence_floor_after: None,
        });
    }
}

/// The by-author disposition of one undelivered item of a cancelled turn
/// (ADR 0101 §10): a wake is always deferred; host input addressed to the
/// cancelled turn takes the request's disposition; anything else the turn
/// held is released at its position.
fn cancel_disposition(
    item: &IngressItem,
    cancel: &IngressTurnCancel,
) -> IngressUndeliveredDisposition {
    match item.kind() {
        IngressKind::Input if item.delivery.addressed_turn() == Some(&cancel.turn_id) => {
            cancel.undelivered.into()
        }
        IngressKind::Input | IngressKind::ProcessWake | IngressKind::SessionCommand => {
            IngressUndeliveredDisposition::Defer
        }
    }
}

fn refused(
    settlement: &IngressClaimSettlement,
    item_id: &IngressItemId,
    reason: &'static str,
) -> StoreError {
    StoreError::IngressSettlementRefused {
        session_id: settlement.session_id.clone(),
        item_id: item_id.to_string(),
        reason,
    }
}

/// Verify every row a settlement's claims name still carries that claim at
/// the settling fence's drive epoch, that the settlement names every row its
/// claims hold, and return the named rows in `enqueue_seq` order.
fn claimed_rows<'a>(
    settlement: &IngressClaimSettlement,
    fence: &DriveFence,
    observed: &'a [IngressSettlementRow],
) -> Result<Vec<&'a IngressSettlementRow>, StoreError> {
    let mut rows = Vec::new();
    for IngressClaimRef { identity, item_ids } in &settlement.claims {
        for item_id in item_ids {
            let row = observed
                .iter()
                .find(|row| &row.item.item_id == item_id)
                .filter(|row| !row.item.state.is_terminal() && row.claim.as_ref() == Some(identity))
                .ok_or_else(|| StoreError::IngressClaimSuperseded {
                    session_id: settlement.session_id.clone(),
                    claim_id: identity.claim_id.clone(),
                    item_id: item_id.to_string(),
                })?;
            // A claim is live only while the epoch it pins is the session's:
            // a claim a superseded drive took settles nothing until the
            // current epoch reclaims it through the claim CAS.
            if row.claim_epoch != Some(fence.epoch()) {
                return Err(StoreError::StaleDriveFence {
                    session_id: settlement.session_id.clone(),
                    fence_epoch: row.claim_epoch.unwrap_or_default(),
                    current_epoch: fence.epoch(),
                });
            }
            if rows
                .iter()
                .any(|seen: &&IngressSettlementRow| seen.item.item_id == row.item.item_id)
            {
                return Err(refused(settlement, item_id, "the item is named twice"));
            }
            rows.push(row);
        }
    }
    for row in observed {
        let Some(identity) = row.claim.as_ref() else {
            continue;
        };
        if !row.item.state.is_terminal()
            && settlement
                .claims
                .iter()
                .any(|claim| &claim.identity == identity)
            && !rows
                .iter()
                .any(|named| named.item.item_id == row.item.item_id)
        {
            return Err(refused(
                settlement,
                &row.item.item_id,
                "a settlement names every row its claims hold",
            ));
        }
    }
    rows.sort_by_key(|row| row.item.enqueue_seq);
    Ok(rows)
}

/// Plan one settlement (ADR 0101 §7, §10, §12).
///
/// `observed` holds the current state of every row the settlement's claims
/// name and every row that still carries one of its claims. `addressed`
/// holds, for a turn cancel, the non-terminal rows addressed to the cancelled
/// turn: open rows, and rows an interrupted claim of a superseded drive epoch
/// still holds. Every claimed row must still carry its claim at `fence`'s
/// epoch, or the whole settlement is refused.
pub fn plan_ingress_settlement(
    settlement: &IngressClaimSettlement,
    fence: &DriveFence,
    observed: &[IngressSettlementRow],
    addressed: &[IngressSettlementRow],
) -> Result<IngressSettlementPlan, StoreError> {
    let rows = claimed_rows(settlement, fence, observed)?;
    let mut plan = IngressSettlementPlan::default();
    match &settlement.intent {
        IngressSettlementIntent::Turn { delivered, cancel } => {
            for item_id in delivered {
                let Some(row) = rows.iter().find(|row| &row.item.item_id == item_id) else {
                    return Err(refused(
                        settlement,
                        item_id,
                        "a delivered item must be held by the settling claims",
                    ));
                };
                if row.item.kind() == IngressKind::SessionCommand {
                    return Err(refused(
                        settlement,
                        item_id,
                        "a turn never delivers a command",
                    ));
                }
            }
            for row in &rows {
                let item = &row.item;
                if item.kind() == IngressKind::SessionCommand {
                    return Err(refused(
                        settlement,
                        &item.item_id,
                        "a turn never holds a command",
                    ));
                }
                if delivered.contains(&item.item_id) {
                    plan.complete(item, IngressTerminalCause::Delivered);
                } else if let Some(cancel) = cancel {
                    plan.affect(
                        item,
                        cancel_disposition(item, cancel),
                        turn_cancelled(cancel),
                        true,
                    );
                } else {
                    plan.writes.push(IngressRowSettlement::Release {
                        item_id: item.item_id.clone(),
                    });
                }
            }
            if let Some(cancel) = cancel {
                // ADR 0101 §10: the disposition reaches every addressed item,
                // whether it is still open or an interrupted claim of a
                // superseded epoch holds it. A row a live claim of this epoch
                // holds is that claim's to settle.
                for row in addressed {
                    let item = &row.item;
                    if item.state.is_terminal()
                        || item.delivery.addressed_turn() != Some(&cancel.turn_id)
                        || rows.iter().any(|named| named.item.item_id == item.item_id)
                        || row.claim_epoch == Some(fence.epoch())
                    {
                        continue;
                    }
                    plan.affect(
                        item,
                        cancel_disposition(item, cancel),
                        turn_cancelled(cancel),
                        row.claim.is_some(),
                    );
                }
            }
        }
        IngressSettlementIntent::Commands { outcomes } => {
            for outcome in outcomes {
                if !rows.iter().any(|row| row.item.item_id == outcome.item_id) {
                    return Err(refused(
                        settlement,
                        &outcome.item_id,
                        "a settled command must be held by the settling claims",
                    ));
                }
            }
            for row in &rows {
                let item = &row.item;
                if item.kind() != IngressKind::SessionCommand {
                    return Err(refused(
                        settlement,
                        &item.item_id,
                        "a command drain never holds turn-lane items",
                    ));
                }
                match outcomes
                    .iter()
                    .find(|outcome| outcome.item_id == item.item_id)
                {
                    Some(outcome) => plan.complete(
                        item,
                        match outcome.result {
                            IngressCommandResult::Applied => IngressTerminalCause::Applied,
                            IngressCommandResult::StaleConfigRevision { base, head } => {
                                IngressTerminalCause::StaleConfigRevision { base, head }
                            }
                        },
                    ),
                    None => plan.writes.push(IngressRowSettlement::Release {
                        item_id: item.item_id.clone(),
                    }),
                }
            }
        }
    }
    plan.affected.sort_by_key(|record| record.enqueue_seq);
    Ok(plan)
}

fn turn_cancelled(cancel: &IngressTurnCancel) -> IngressCancelReason {
    IngressCancelReason::TurnCancelled {
        request_id: cancel.request_id.clone(),
        mode: cancel.mode,
    }
}

/// What a host withdrawal does to one row.
#[derive(Clone, Debug)]
pub enum IngressWithdrawDecision {
    /// Tombstone it `cancelled`, raise its wake floor if it is a wake, and
    /// report the record.
    Withdraw {
        reason: IngressCancelReason,
        floor_raise: Option<(ProcessId, u64)>,
        record: Box<IngressAffectedItem>,
    },
    /// A claim pinning the session's current drive epoch holds it.
    Held,
    AlreadyCompleted,
    AlreadyCancelled,
}

/// Decide one host withdrawal (ADR 0101 §10). `claim_epoch` is the drive
/// epoch the row's claim pins, if a claim holds it; `current_epoch` is the
/// session's drive epoch now.
#[must_use]
pub fn plan_withdrawal(
    item: &IngressItem,
    claim_epoch: Option<u64>,
    current_epoch: u64,
    selector: IngressWithdrawSelector,
) -> IngressWithdrawDecision {
    match item.state {
        crate::session_ingress_vocabulary::IngressState::Completed => {
            return IngressWithdrawDecision::AlreadyCompleted;
        }
        crate::session_ingress_vocabulary::IngressState::Cancelled => {
            return IngressWithdrawDecision::AlreadyCancelled;
        }
        crate::session_ingress_vocabulary::IngressState::Open
        | crate::session_ingress_vocabulary::IngressState::Accepted => {}
    }
    if claim_epoch == Some(current_epoch) {
        return IngressWithdrawDecision::Held;
    }
    let reason = IngressCancelReason::HostWithdrawn { selector };
    IngressWithdrawDecision::Withdraw {
        floor_raise: item
            .payload
            .wake_source()
            .map(|(process_id, sequence)| (process_id.clone(), sequence)),
        record: Box::new(IngressAffectedItem {
            item_id: item.item_id.clone(),
            kind: item.kind(),
            source_key: item.source_key.clone(),
            enqueue_seq: item.enqueue_seq,
            disposition: IngressUndeliveredDisposition::Drop,
            reason: reason.clone(),
            payload: item.payload.clone(),
            fence_floor_after: None,
        }),
        reason,
    }
}

#[cfg(test)]
#[path = "session_ingress_plan/tests.rs"]
mod tests;
