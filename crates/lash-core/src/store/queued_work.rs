//! Dialect-independent queued-work claim logic shared by durable backends.
//!
//! The SQL backends (sqlite, postgres) load candidate batch rows ordered by
//! `enqueue_seq` and pre-filtered to ready batches that are not held by a
//! live claim, then apply the same pure state machine: a delivery-policy
//! boundary gate, compatibility/merge-key prefix grouping, and fencing-token /
//! lease derivation. That state machine lives here so the backends own only
//! their SQL reads and writes while the claim contract has a single
//! implementation, exercised against every backend by the shared
//! `runtime_persistence` conformance suite.

use super::LeaseOwnerIdentity;
use crate::{
    DeliveryPolicy, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkClaimPolicy, QueuedWorkKind, QueuedWorkPayload, StoreError, TurnCause,
};

/// Result of resolving a host-selected queued-work set against durable rows.
///
/// IDs with no remaining row are already satisfied. Any returned claim covers
/// only rows acquired by this call; present rows that could not join that claim
/// remain visible to the runtime as a selected-drain refusal.
#[derive(Clone, Debug)]
pub struct SelectedQueuedWorkClaimOutcome {
    /// Newly acquired rows, if the present selection was claimable.
    pub claim: Option<QueuedWorkClaim>,
    /// Requested IDs for which no durable queue row remained.
    pub already_satisfied_batch_ids: Vec<String>,
}

impl SelectedQueuedWorkClaimOutcome {
    /// Builds an exact-claim resolution from newly acquired rows and requested
    /// IDs whose durable rows were already gone.
    ///
    /// Store implementations must not classify a present but unclaimable row
    /// as already satisfied. The runtime turns that case into a selected-drain
    /// refusal rather than reporting idempotent success.
    pub fn new(claim: Option<QueuedWorkClaim>, already_satisfied_batch_ids: Vec<String>) -> Self {
        Self {
            claim,
            already_satisfied_batch_ids,
        }
    }

    /// Reports whether this store resolution acquired no new durable rows.
    ///
    /// At this layer, `true` does not by itself prove that the complete drain
    /// was satisfied: callers must also distinguish IDs in
    /// [`Self::already_satisfied_batch_ids`] from present IDs that could not be
    /// claimed. The facade's selected-drain outcome carries the stronger
    /// successful, fully-satisfied meaning.
    pub fn acquired_no_rows(&self) -> bool {
        self.claim.is_none()
    }

    /// Transforms only newly acquired rows, preserving `None` when no claim was
    /// created; this projection discards the already-satisfied ID evidence.
    pub fn map<U>(self, f: impl FnOnce(QueuedWorkClaim) -> U) -> Option<U> {
        self.claim.map(f)
    }

    /// Returns the newly acquired claim or constructs an error when no rows
    /// were acquired; this projection discards the already-satisfied ID
    /// evidence.
    pub fn ok_or_else<E>(self, f: impl FnOnce() -> E) -> Result<QueuedWorkClaim, E> {
        self.claim.ok_or_else(f)
    }

    /// Returns the newly acquired claim or panics with `message` when no rows
    /// were acquired; this projection discards the already-satisfied ID
    /// evidence.
    #[track_caller]
    pub fn expect(self, message: &str) -> QueuedWorkClaim {
        self.claim.expect(message)
    }
}

/// Why a turn-work claim attempt acquired no rows.
///
/// These are the refusal facts the claim state machine already computes while
/// deciding a wake (see `record_turn_claim_decision`), plus the ones only a
/// backend can observe: whether the lane still holds deferred work, and whether
/// a concurrent writer took the selected rows. Every empty automatic drain
/// carries one, so a host never has to reconstruct the reason from side
/// evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QueuedWorkClaimRefusal {
    /// The host's claim policy admitted zero rows.
    ZeroLimit,
    /// The durable queue holds no pending work for this lane: every row it ever
    /// held was consumed. Nothing is coming without a fresh enqueue.
    Empty,
    /// Pending work exists for this lane, but its earliest `available_at_ms`
    /// has not arrived, so no row was claimable yet. The work is intact and
    /// will drain on a later attempt; the re-poll cadence is the host's, so no
    /// timestamp is part of this contract.
    NotYetAvailable,
    /// A session command sits at the queue head and is never skipped.
    CommandAtHead,
    /// The head batch may not cross the active turn's delivery boundary.
    DeliveryBoundaryBlocked,
    /// Rows were selected, but the physically earliest candidate was withheld,
    /// so no contiguous prefix remained for a backend that claims by prefix.
    ///
    /// No shipped backend reaches this today: the sqlite and postgres
    /// head-candidate queries never offer a withheld head to the prefix helper,
    /// and the in-memory and perf stores claim by index instead of by prefix.
    /// It guards a third-party prefix-claiming backend, whose selection this
    /// same helper would otherwise silently truncate to nothing.
    HeadWithheld,
    /// The selection was legal, but another writer took the rows first.
    ClaimRaceLost,
}

impl QueuedWorkClaimRefusal {
    /// The stable snake_case spelling used in claim-decision diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZeroLimit => "zero_limit",
            Self::Empty => "empty",
            Self::NotYetAvailable => "not_yet_available",
            Self::CommandAtHead => "command_at_head",
            Self::DeliveryBoundaryBlocked => "delivery_boundary_blocked",
            Self::HeadWithheld => "head_withheld",
            Self::ClaimRaceLost => "claim_race_lost",
        }
    }
}

/// The outcome the claim state machine recorded for one wake.
///
/// Refusing variants carry the [`QueuedWorkClaimRefusal`] that reaches the host;
/// the remaining variants name why rows *were* selected and stay diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnClaimOutcome {
    Refused(QueuedWorkClaimRefusal),
    InterruptedClaimRedrive,
    SingleRow,
    MaxPendingAgeReached,
    SingleEligibleRow,
    HostDrainPolicy,
}

impl TurnClaimOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Refused(refusal) => refusal.as_str(),
            Self::InterruptedClaimRedrive => "interrupted_claim_redrive",
            Self::SingleRow => "single_row",
            Self::MaxPendingAgeReached => "max_pending_age_reached",
            Self::SingleEligibleRow => "single_eligible_row",
            Self::HostDrainPolicy => "host_drain_policy",
        }
    }
}

/// Candidate indices one claim may take, or the refusal that took none.
///
/// `refusal` is `Some` exactly when `indices` is empty.
#[derive(Clone, Debug)]
pub struct TurnWorkClaimSelection {
    pub indices: Vec<usize>,
    pub refusal: Option<QueuedWorkClaimRefusal>,
}

/// The contiguous leading run one prefix-claiming backend may take, or the
/// refusal that left it empty.
///
/// `refusal` is `Some` exactly when `len` is zero.
#[derive(Clone, Copy, Debug)]
pub struct TurnWorkClaimPrefix {
    pub len: usize,
    pub refusal: Option<QueuedWorkClaimRefusal>,
}

/// Whether a claim acquired rows, or why it did not.
///
/// This is the automatic counterpart to [`SelectedQueuedWorkClaimOutcome`]: an
/// automatic drain names no batch ids, so the refusal itself is the answer the
/// runtime hands back to the host.
#[derive(Clone, Debug)]
pub enum QueuedWorkClaimOutcome {
    Claimed(QueuedWorkClaim),
    Refused(QueuedWorkClaimRefusal),
}

impl QueuedWorkClaimOutcome {
    /// The acquired claim, discarding the refusal evidence.
    pub fn claim(self) -> Option<QueuedWorkClaim> {
        match self {
            Self::Claimed(claim) => Some(claim),
            Self::Refused(_) => None,
        }
    }

    /// The refusal, when this attempt acquired nothing.
    pub fn refusal(&self) -> Option<QueuedWorkClaimRefusal> {
        match self {
            Self::Claimed(_) => None,
            Self::Refused(refusal) => Some(*refusal),
        }
    }
}

/// Whether a durable queued-work row carries a session command or turn work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkClass {
    SessionCommand,
    TurnWork,
}

/// The payload-free fields that establish one pending work item's position.
///
/// `enqueue_seq` totally orders *one* ingress family: each family draws it from
/// its own counter (a PostgreSQL sequence, a SQLite rowid, an in-memory
/// mutex-guarded integer), so the two families' sequences are not comparable —
/// their relative values are decided by unrelated enqueue traffic elsewhere in
/// the store. That is why [`Ord`] is deliberately not derived: within-family
/// ordering happens in each store's own `ORDER BY enqueued_at_ms, enqueue_seq`,
/// and the only cross-family comparison — [`PendingSessionWorkOrdering::
/// session_command_precedes_turn_input`] — reads the timestamp alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingWorkOrderingKey {
    pub enqueued_at_ms: u64,
    pub enqueue_seq: u64,
}

/// The earliest pending session-command and next-turn-input positions.
///
/// Stores project only these scalar keys so the idle drain can arbitrate the
/// two ingress families without hydrating either family's payloads. The
/// session-command side is exactly the queued-work rows whose durable
/// `work_kind` is [`crate::QueuedWorkKind::Control`].
///
/// [`crate::QueuedWorkKind::Cancel`] rows belong to neither field: cancellation
/// preempts through its own path and must not shift which ingress family drains
/// first, so a pending cancel leaves both keys untouched.
///
/// [`Default`] is deliberately not derived. Both fields are public and both
/// `None` means "nothing is pending on either side", which is a real answer
/// about the store — an out-of-tree implementation must not be able to satisfy
/// the projection with `Default::default()` and silently report an idle session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingSessionWorkOrdering {
    pub session_command: Option<PendingWorkOrderingKey>,
    pub turn_input: Option<PendingWorkOrderingKey>,
}

impl PendingSessionWorkOrdering {
    /// Whether the earliest pending session command sorts before the earliest
    /// pending next-turn input.
    ///
    /// The comparison is on `enqueued_at_ms` alone, and a tie resolves to the
    /// turn input. `enqueue_seq` is *not* a tiebreak here: the two families
    /// number themselves from independent counters, so a cross-family sequence
    /// comparison would hand the decision to unrelated enqueue traffic in other
    /// sessions. Only a real timestamp ordering may reorder the families.
    pub fn session_command_precedes_turn_input(self) -> bool {
        match (self.session_command, self.turn_input) {
            (Some(command), Some(input)) => command.enqueued_at_ms < input.enqueued_at_ms,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// Claim-id spelling selected by each store family.
///
/// These prefixes are durable bytes. SQLite and Postgres share the production
/// spellings; recording and performance stores retain their existing diagnostic
/// dialects. Centralizing them prevents backend-local construction drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimIdDialect {
    QueuedWork,
    TurnInput,
    RecordingQueuedWork,
    RecordingTurnInput,
    PerformanceQueuedWork,
    PerformanceTurnInput,
}

/// Builds a claim id without changing the chosen dialect's exact spelling.
pub fn derive_claim_id(dialect: ClaimIdDialect, enqueue_seq: u64, fencing_token: u64) -> String {
    let prefix = match dialect {
        ClaimIdDialect::QueuedWork => "qwc",
        ClaimIdDialect::TurnInput => "tic",
        ClaimIdDialect::RecordingQueuedWork => "recording-qwc",
        ClaimIdDialect::RecordingTurnInput => "recording-tic",
        ClaimIdDialect::PerformanceQueuedWork => "perf-qwc",
        ClaimIdDialect::PerformanceTurnInput => "perf-tic",
    };
    format!("{prefix}:{enqueue_seq}:{fencing_token}")
}

/// Decoded claim-relevant fields of one ready queued-work batch row.
///
/// Backends build these from their candidate rows, presented in
/// `enqueue_seq` ascending order and already filtered to
/// `available_at_ms <= now` with no live claim.
#[derive(Clone, Debug)]
pub struct ClaimCandidate {
    /// Durable batch identity, used to name a row in claim diagnostics.
    pub batch_id: String,
    pub enqueue_seq: u64,
    pub claim_fencing_token: u64,
    /// Durable claim identity left by an interrupted predecessor generation.
    /// Matching identities describe the exact batch composition that already
    /// escaped into that generation's journaled command.
    pub prior_claim_id: Option<String>,
    pub work_class: QueuedWorkClass,
    /// Whether this row is exactly one `ApplyConfigPatch` command and can
    /// therefore share a drain commit with adjacent config patches.
    pub config_patch_command: bool,
    pub delivery_policy: DeliveryPolicy,
    pub kind: QueuedWorkKind,
    pub authority: QueuedWorkAuthority,
    pub merge_key: Option<String>,
    pub enqueued_at_ms: u64,
    turn_causes: Vec<TurnCause>,
    input_texts: Vec<String>,
}

impl ClaimCandidate {
    pub fn from_batch(
        batch: &QueuedWorkBatch,
        claim_fencing_token: u64,
        prior_claim_id: Option<String>,
    ) -> Self {
        let mut turn_causes = Vec::new();
        let mut input_texts = Vec::new();
        let config_patch_command = matches!(
            batch.items.as_slice(),
            [crate::QueuedWorkItem {
                payload: QueuedWorkPayload::SessionCommand { command },
                ..
            }] if matches!(command.as_ref(), crate::SessionCommand::ApplyConfigPatch { .. })
        );
        for item in &batch.items {
            match &item.payload {
                QueuedWorkPayload::ProcessWake { wake } => {
                    turn_causes.push(crate::process_wake_turn_cause(wake));
                }
                QueuedWorkPayload::AgentFrameTask { task, .. } => input_texts.push(task.clone()),
                QueuedWorkPayload::SessionCommand { .. } => {}
            }
        }
        Self {
            batch_id: batch.batch_id.clone(),
            enqueue_seq: batch.enqueue_seq,
            claim_fencing_token,
            prior_claim_id,
            work_class: batch.work_class().unwrap_or(QueuedWorkClass::TurnWork),
            config_patch_command,
            delivery_policy: batch.delivery_policy,
            kind: batch.kind,
            authority: batch.authority.clone(),
            merge_key: batch.merge_key.clone(),
            enqueued_at_ms: batch.enqueued_at_ms,
            turn_causes,
            input_texts,
        }
    }
}

/// How many candidate rows a backend should scan when selecting up to
/// `max_batches` claimable batches. Joinable groups are matched as a prefix,
/// so scanning a bounded surplus keeps one round trip sufficient.
pub fn claim_scan_limit(max_batches: usize) -> i64 {
    i64::try_from(max_batches)
        .unwrap_or(i64::MAX)
        .min(i64::MAX - 32)
        + 32
}

/// Maximum number of adjacent config commands one session-command claim may
/// coalesce. A longer FIFO prefix remains queued and drains through later
/// commits; bounding this claim also bounds every SQL candidate scan that
/// feeds it.
#[doc(hidden)]
pub const MAX_SESSION_COMMAND_BATCHES_PER_CLAIM: usize = 64;

/// Select a leading session-command claim.
///
/// Non-config commands remain exclusive. A leading `ApplyConfigPatch` extends
/// through the complete adjacent config-patch prefix so one drain can apply N
/// ordered patches in one head commit while completing all N batches.
pub fn select_leading_session_command(candidates: &[ClaimCandidate]) -> usize {
    let Some(first) = candidates.first() else {
        return 0;
    };
    if first.work_class != QueuedWorkClass::SessionCommand {
        return 0;
    }
    if !first.config_patch_command {
        return 1;
    }
    candidates
        .iter()
        .take(MAX_SESSION_COMMAND_BATCHES_PER_CLAIM)
        .take_while(|candidate| {
            candidate.work_class == QueuedWorkClass::SessionCommand
                && candidate.config_patch_command
        })
        .count()
}

/// Select the turn-work `candidates` that a single claim may take.
///
/// Fresh claims return a leading prefix. Interrupted claims return every
/// candidate carrying the head row's prior claim identity, including rows
/// separated by newly ready unrelated work.
///
/// * The queue head must be [`QueuedWorkClass::TurnWork`]. Earlier ready
///   session commands are never skipped or materialized as turn input.
/// * An [`QueuedWorkClaimBoundary::ActiveTurnCheckpoint`] boundary only
///   admits work whose head batch is
///   [`DeliveryPolicy::EarliestSafeBoundary`].
/// * An absent merge key, or a control/cancel kind, claims exactly one batch.
/// * A batchable head extends through immediately following rows with the same
///   delivery policy, merge key, and authority/elevation, within the host's row
///   and age bounds. How much of that eligible prefix actually drains is the
///   host's [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) decision.
pub fn select_turn_work_claim_indices(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<TurnWorkClaimSelection, StoreError> {
    if policy.max_rows == 0 {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            QueuedWorkClaimRefusal::ZeroLimit,
        ));
    }
    let Some(first) = candidates.first() else {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            QueuedWorkClaimRefusal::Empty,
        ));
    };
    if first.work_class != QueuedWorkClass::TurnWork {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            QueuedWorkClaimRefusal::CommandAtHead,
        ));
    }
    if boundary == QueuedWorkClaimBoundary::ActiveTurnCheckpoint
        && first.delivery_policy != DeliveryPolicy::EarliestSafeBoundary
    {
        if let Some(withheld_claim_id) = first.prior_claim_id.as_deref() {
            let remaining_indices = candidates
                .iter()
                .enumerate()
                .filter_map(|(index, candidate)| {
                    (candidate.prior_claim_id.as_deref() != Some(withheld_claim_id))
                        .then_some(index)
                })
                .collect::<Vec<_>>();
            let remaining = remaining_indices
                .iter()
                .map(|index| candidates[*index].clone())
                .collect::<Vec<_>>();
            let selected =
                select_turn_work_claim_indices(&remaining, boundary, policy, now_epoch_ms)?;
            if !selected.indices.is_empty() {
                return Ok(select(
                    selected
                        .indices
                        .into_iter()
                        .map(|index| remaining_indices[index])
                        .collect(),
                ));
            }
        }
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            QueuedWorkClaimRefusal::DeliveryBoundaryBlocked,
        ));
    }
    if let Some(prior_claim_id) = first.prior_claim_id.as_deref() {
        let selected = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                (candidate.prior_claim_id.as_deref() == Some(prior_claim_id)).then_some(index)
            })
            .collect::<Vec<_>>();
        let rendered_tokens = selected.iter().fold(0usize, |total, index| {
            total.saturating_add(rendered_token_upper_bound(std::slice::from_ref(
                &candidates[*index],
            )))
        });
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            selected.len(),
            rendered_tokens,
            TurnClaimOutcome::InterruptedClaimRedrive,
        );
        return Ok(select(selected));
    }

    if policy.action_token_reserve >= policy.max_context_tokens {
        return Err(StoreError::QueuedWorkActionReserveExhaustsContext {
            max_context_tokens: policy.max_context_tokens,
            action_token_reserve: policy.action_token_reserve,
        });
    }
    let available_tokens = policy
        .max_context_tokens
        .saturating_sub(policy.action_token_reserve);
    let first_tokens = rendered_token_upper_bound(&candidates[..1]);
    if first_tokens > policy.max_context_tokens {
        return Err(StoreError::QueuedWorkRowExceedsContextWindow {
            batch_id: first.batch_id.clone(),
            batch_enqueue_seq: first.enqueue_seq,
            rendered_tokens: first_tokens,
            max_context_tokens: policy.max_context_tokens,
        });
    }
    if !first.kind.is_batchable() || first.merge_key.is_none() {
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            TurnClaimOutcome::SingleRow,
        );
        return Ok(select(vec![0]));
    }
    if now_epoch_ms.saturating_sub(first.enqueued_at_ms) >= policy.max_pending_age_ms {
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            TurnClaimOutcome::MaxPendingAgeReached,
        );
        return Ok(select(vec![0]));
    }

    let mut compatible_prefix_len = 1;
    for candidate in &candidates[1..] {
        if compatible_prefix_len >= policy.max_rows
            || candidate.work_class != QueuedWorkClass::TurnWork
            || !candidate.kind.is_batchable()
            || candidate.delivery_policy != first.delivery_policy
            || candidate.merge_key != first.merge_key
            || candidate.authority != first.authority
        {
            break;
        }
        compatible_prefix_len += 1;
    }

    // Lash has now applied every claim law: what remains is a legal, strictly
    // FIFO prefix that *may* share this turn. How much of it actually drains is
    // the host's `QueuedDrainPolicy` decision (FIG-1313), not kernel token
    // arithmetic. The shipped default drains the head alone.
    if compatible_prefix_len == 1 {
        // A lone eligible row always drains: no selection is expressible, so
        // the policy is not consulted and its per-row projections are not
        // rendered.
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            TurnClaimOutcome::SingleEligibleRow,
        );
        return Ok(select(vec![0]));
    }
    let drain_candidates = candidates[..compatible_prefix_len]
        .iter()
        .map(|candidate| crate::QueuedDrainCandidate {
            enqueue_seq: candidate.enqueue_seq,
            kind: candidate.kind,
            merge_key: candidate.merge_key.clone(),
            authority: candidate.authority.clone(),
            projected_tokens: rendered_token_upper_bound(std::slice::from_ref(candidate)),
            pending_age_ms: now_epoch_ms.saturating_sub(candidate.enqueued_at_ms),
        })
        .collect::<Vec<_>>();
    let request = crate::QueuedDrainRequest::new(
        &drain_candidates,
        available_tokens,
        policy.max_context_tokens,
        policy.max_rows,
        boundary,
    );
    let requested = policy.drain_policy.select_drain(&request).drain_count();
    let selected = requested.clamp(1, compatible_prefix_len);
    // A non-head row larger than the whole window is not this drain's problem to
    // refuse: the selection simply stops before it. The fitting head still
    // drains, the oversized row becomes the head of a later wake, and the head
    // check above refuses it there by name. Carrying it into this claim instead
    // would fail a claim that could have made progress, and the
    // interrupted-claim redrive would restore that doomed composition forever.
    let selected = candidates[..selected]
        .iter()
        .position(|candidate| {
            rendered_token_upper_bound(std::slice::from_ref(candidate)) > policy.max_context_tokens
        })
        .map_or(selected, |oversized_index| oversized_index.max(1));
    let rendered_tokens = rendered_token_upper_bound(&candidates[..selected]);
    tracing::debug!(
        target: "lash::queued_work_batching",
        drain_policy = policy.drain_policy.name(),
        offered = compatible_prefix_len,
        requested,
        selected,
        "queued drain policy selection"
    );
    record_turn_claim_decision(
        candidates,
        boundary,
        policy,
        now_epoch_ms,
        selected,
        rendered_tokens,
        TurnClaimOutcome::HostDrainPolicy,
    );
    Ok(select((0..selected).collect()))
}

/// A selection that acquired `indices`.
fn select(indices: Vec<usize>) -> TurnWorkClaimSelection {
    debug_assert!(!indices.is_empty(), "a selection must acquire rows");
    TurnWorkClaimSelection {
        indices,
        refusal: None,
    }
}

/// A selection that acquired nothing, recorded under `refusal`.
fn refuse(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
    refusal: QueuedWorkClaimRefusal,
) -> TurnWorkClaimSelection {
    record_turn_claim_decision(
        candidates,
        boundary,
        policy,
        now_epoch_ms,
        0,
        0,
        TurnClaimOutcome::Refused(refusal),
    );
    TurnWorkClaimSelection {
        indices: Vec::new(),
        refusal: Some(refusal),
    }
}

/// Select the number of rows from a physically contiguous candidate set.
///
/// SQL automatic claims pre-filter interrupted rows by the head claim ID, and
/// exact-ID claims construct a contiguous candidate slice. In-memory automatic
/// claims use [`select_turn_work_claim_indices`] directly so identity gaps are
/// retained.
pub fn select_turn_work_claim_prefix(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<TurnWorkClaimPrefix, StoreError> {
    let selected = select_turn_work_claim_indices(candidates, boundary, policy, now_epoch_ms)?;
    let len = selected
        .indices
        .iter()
        .copied()
        .enumerate()
        .take_while(|(prefix_index, selected_index)| prefix_index == selected_index)
        .count();
    // A selection whose physically earliest row is withheld leaves a
    // prefix-claiming backend nothing to take. That is a refusal in its own
    // right, distinct from the state machine's four, and it is named here
    // because here is where the prefix is derived.
    let refusal = if len == 0 {
        Some(
            selected
                .refusal
                .unwrap_or(QueuedWorkClaimRefusal::HeadWithheld),
        )
    } else {
        None
    };
    Ok(TurnWorkClaimPrefix { len, refusal })
}

/// Size an exact, host-named drain composition.
///
/// The host already chose which rows travel together, so the configured
/// [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) is not consulted here: an
/// automatic policy that drains one row per wake would otherwise shrink an
/// exact two-row selection to one, and the caller — which requires the whole
/// requested composition or none of it — would abandon the partial claim as
/// unclaimable, permanently and deterministically. The policy answers *how much
/// of the pending queue to take*, a question an exact selection has already
/// answered.
///
/// `max_rows` is exempt for the same reason: it is a coalescing bound on how
/// many *pending* rows Lash gathers on its own, and truncating a host-named
/// composition with it wedges the claim on a second axis. Interrupted redrive
/// already exempts a committed composition from a successor's row limit.
///
/// The genuine claim laws still apply: the head class, the delivery boundary,
/// merge-key/authority/kind compatibility, the pending-age bound, and the
/// oversized-row refusal all bound an exact request as they bound an automatic
/// one.
pub fn select_exact_turn_work_claim_prefix(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<TurnWorkClaimPrefix, StoreError> {
    let policy = QueuedWorkClaimPolicy {
        drain_policy: crate::runtime::exact_selection_drain_policy(),
        max_rows: policy.max_rows.max(candidates.len()),
        ..policy.clone()
    };
    select_turn_work_claim_prefix(candidates, boundary, &policy, now_epoch_ms)
}

/// Resolve an exact-ID selection against interrupted predecessor identities.
///
/// `candidate_batch_claims` must contain every requested ready row plus every
/// member of each interrupted claim touched by the request, in durable enqueue
/// order. Every touched interrupted identity is validated before one is
/// selected. If any identity is only partially covered, the physically earliest
/// incomplete claim's literal composition is returned without selecting rows.
/// When every touched identity is complete, one selected drain reclaims exactly
/// the physically earliest interrupted composition; later complete identities
/// remain queued for a later drain.
#[doc(hidden)]
pub fn select_interrupted_exact_claim_indices(
    candidate_batch_claims: &[(String, Option<String>)],
    requested_batch_ids: &[String],
) -> Result<Option<Vec<usize>>, Vec<String>> {
    let requested = requested_batch_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let mut involved_claim_ids = Vec::new();
    for (batch_id, prior_claim_id) in candidate_batch_claims {
        let Some(prior_claim_id) = prior_claim_id.as_deref() else {
            continue;
        };
        if requested.contains(batch_id.as_str()) && !involved_claim_ids.contains(&prior_claim_id) {
            involved_claim_ids.push(prior_claim_id);
        }
    }
    let Some(earliest_claim_id) = involved_claim_ids.first().copied() else {
        return Ok(None);
    };

    for prior_claim_id in involved_claim_ids {
        let required_batch_ids = candidate_batch_claims
            .iter()
            .filter(|(_, candidate_claim_id)| candidate_claim_id.as_deref() == Some(prior_claim_id))
            .map(|(batch_id, _)| batch_id.clone())
            .collect::<Vec<_>>();
        if !required_batch_ids
            .iter()
            .all(|batch_id| requested.contains(batch_id.as_str()))
        {
            return Err(required_batch_ids);
        }
    }

    Ok(Some(
        candidate_batch_claims
            .iter()
            .enumerate()
            .filter_map(|(index, (_, candidate_claim_id))| {
                (candidate_claim_id.as_deref() == Some(earliest_claim_id)).then_some(index)
            })
            .collect(),
    ))
}

/// Conservative upper bound for the exact model-visible queued-work render.
///
/// Process wakes use the shared turn-events renderer. Agent-frame task text is
/// appended exactly as turn input. One UTF-8 byte is charged as one token: this
/// deliberately overestimates ordinary model tokenizers while remaining safe
/// without moving tokenizer selection from the host/provider boundary into
/// core.
fn rendered_token_upper_bound(candidates: &[ClaimCandidate]) -> usize {
    let causes = candidates
        .iter()
        .flat_map(|candidate| candidate.turn_causes.iter().cloned())
        .collect::<Vec<_>>();
    let mut rendered_bytes =
        crate::render_turn_causes_prompt(&causes).map_or(0, |rendered| rendered.len());
    let input_items = candidates
        .iter()
        .flat_map(|candidate| candidate.input_texts.iter())
        .map(|text| crate::InputItem::text(text.clone()))
        .collect::<Vec<_>>();
    if !input_items.is_empty() {
        let input = crate::TurnInput::items(input_items);
        let input_bytes = serde_json::to_vec(&input).map_or(usize::MAX, |rendered| rendered.len());
        rendered_bytes = rendered_bytes.saturating_add(input_bytes);
    }
    rendered_bytes
}

fn record_turn_claim_decision(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
    selected: usize,
    rendered_tokens: usize,
    outcome: TurnClaimOutcome,
) {
    let oldest_pending_age_ms = candidates
        .first()
        .map(|candidate| now_epoch_ms.saturating_sub(candidate.enqueued_at_ms));
    let pending_age_bound_reached =
        oldest_pending_age_ms.is_some_and(|age| age >= policy.max_pending_age_ms);
    tracing::info!(
        target: "lash::queued_work_batching",
        ?boundary,
        max_rows = policy.max_rows,
        max_context_tokens = policy.max_context_tokens,
        action_token_reserve = policy.action_token_reserve,
        max_pending_age_ms = policy.max_pending_age_ms,
        ?oldest_pending_age_ms,
        pending_age_bound_reached,
        candidates = ?candidates,
        selected,
        rendered_tokens,
        outcome = outcome.as_str(),
        "wake turn claim decision"
    );
}

/// A freshly derived lease for a selected claim prefix.
///
/// The fencing token advances past the head batch's last observed token, the
/// claim id is stable for (head batch, fencing token), and the lease token is
/// an opaque proof-of-ownership digest the backend stamps on every claimed
/// row. `session_lease_generation` is the caller's live session-execution-lease
/// fencing token; the backend records it to decide when another generation may
/// re-claim the row. Settlement is always keyed by claim id + lease token; a
/// re-claim replaces those ownership values (see ADR 0029).
#[derive(Clone, Debug)]
pub struct WorkClaimLease {
    pub claim_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
    pub session_lease_generation: u64,
}

impl WorkClaimLease {
    pub fn derive_queued_work(
        head: &ClaimCandidate,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        now_epoch_ms: u64,
        session_lease_generation: u64,
    ) -> Result<Self, StoreError> {
        Self::derive(
            ClaimIdDialect::QueuedWork,
            head.enqueue_seq,
            head.claim_fencing_token,
            session_id,
            owner,
            now_epoch_ms,
            session_lease_generation,
        )
    }

    /// Derives byte-identical production claim authority for either durable
    /// work family. Keeping the claim-id and lease-token seeds together makes
    /// SQLite and PostgreSQL consume one byte contract.
    #[allow(clippy::too_many_arguments)]
    pub fn derive(
        dialect: ClaimIdDialect,
        enqueue_seq: u64,
        claim_fencing_token: u64,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        now_epoch_ms: u64,
        session_lease_generation: u64,
    ) -> Result<Self, StoreError> {
        let fencing_token = StoreError::checked_monotonic_increment(
            "queued_work_claim_fencing_token",
            claim_fencing_token,
        )?;
        let claim_id = derive_claim_id(dialect, enqueue_seq, fencing_token);
        let lease_token = crate::stable_hash::blake3_hex(
            "lash-queued-work-claim-lease/v2",
            format!(
                "{}:{}:{}:{}:{}",
                session_id, owner.owner_id, owner.incarnation_id, claim_id, now_epoch_ms
            )
            .as_bytes(),
        );
        Ok(Self {
            claim_id,
            lease_token,
            fencing_token,
            session_lease_generation,
        })
    }
}

/// Derive the durable id for a newly enqueued batch.
///
/// `nonce` disambiguates batches enqueued within the same millisecond;
/// backends whose id uniqueness already comes from elsewhere pass `None`.
pub fn derive_batch_id(
    session_id: &str,
    source_key: Option<&str>,
    now_epoch_ms: u64,
    nonce: Option<u64>,
) -> String {
    let mut seed = format!("{session_id}:{source_key:?}:{now_epoch_ms}");
    if let Some(nonce) = nonce {
        seed.push_str(&format!(":{nonce}"));
    }
    format!(
        "qwb:{}",
        crate::stable_hash::blake3_hex("lash-queued-work-batch/v2", seed.as_bytes())
    )
}

#[cfg(test)]
mod tests;
