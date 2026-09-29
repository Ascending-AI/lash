//! Dialect-independent queued-work composition shared by durable backends.
//!
//! The SQL backends (sqlite, postgres) load candidate batch rows ordered by
//! `enqueue_seq` and pre-filtered to open batches no root admitted, then
//! apply the same pure state machine: a delivery-policy boundary gate and
//! compatibility/merge-key prefix grouping. That state machine lives here so
//! the backends own only their SQL reads and writes while the composition
//! rule has a single implementation, exercised against every backend by the
//! shared `runtime_persistence` conformance suite.

use crate::{
    AdmissionBoundary, DeliveryPolicy, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkKind,
    QueuedWorkPayload, StoreError, TurnCause, TurnLaneAdmissionPolicy,
};

/// Why a turn-work admission took no rows.
///
/// These are the refusal facts the composition state machine already
/// computes while deciding a wake (see `record_turn_admission_decision`),
/// plus the one only a backend can observe: whether a concurrent writer took
/// the selected rows. Every empty automatic drain carries one, so a host never
/// has to reconstruct the reason from side evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdmissionRefusal {
    /// The host's admission policy admitted zero rows.
    ZeroLimit,
    /// The durable queue holds no pending work for this lane: every row it ever
    /// held was consumed. Nothing is coming without a fresh enqueue.
    Empty,
    /// A session command sits at the queue head and is never skipped.
    CommandAtHead,
    /// The head batch may not cross the active turn's delivery boundary.
    DeliveryBoundaryBlocked,
    /// Rows were selected, but the physically earliest candidate was withheld,
    /// so no contiguous prefix remained for a backend that admits by prefix.
    ///
    /// No shipped backend reaches this today: the sqlite and postgres
    /// head-candidate queries never offer a withheld head to the prefix helper,
    /// and the in-memory and perf stores admit by index instead of by prefix.
    /// It guards a third-party prefix-admitting backend, whose selection this
    /// same helper would otherwise silently truncate to nothing.
    HeadWithheld,
    /// The selection was legal, but another writer took the rows first.
    AdmissionRaceLost,
    /// A pending follow-on owns the session (ADR 0101 §3): nothing is
    /// admitted until its turn commits. Not an error; the drain answers it
    /// after the follow-on.
    FollowOnPending,
}

impl AdmissionRefusal {
    /// The stable snake_case spelling used in admission-decision diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZeroLimit => "zero_limit",
            Self::Empty => "empty",
            Self::CommandAtHead => "command_at_head",
            Self::DeliveryBoundaryBlocked => "delivery_boundary_blocked",
            Self::HeadWithheld => "head_withheld",
            Self::AdmissionRaceLost => "admission_race_lost",
            Self::FollowOnPending => "follow_on_pending",
        }
    }
}

/// The outcome the composition state machine recorded for one wake.
///
/// Refusing variants carry the [`AdmissionRefusal`] that reaches the host;
/// the remaining variants name why rows *were* selected and stay diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnAdmissionOutcome {
    Refused(AdmissionRefusal),
    SingleRow,
    MaxPendingAgeReached,
    SingleEligibleRow,
    HostDrainPolicy,
}

impl TurnAdmissionOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Refused(refusal) => refusal.as_str(),
            Self::SingleRow => "single_row",
            Self::MaxPendingAgeReached => "max_pending_age_reached",
            Self::SingleEligibleRow => "single_eligible_row",
            Self::HostDrainPolicy => "host_drain_policy",
        }
    }
}

/// Candidate indices one admission may take, or the refusal that took none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnWorkSelection {
    Selected { indices: Vec<usize> },
    Refused { reason: AdmissionRefusal },
}

/// The contiguous leading run one prefix-admitting backend may take, or its refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnWorkPrefix {
    Selected { len: usize },
    Refused { reason: AdmissionRefusal },
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
/// Every ingress producer shares one per-session sequence. Timestamps are
/// informational; commands take priority at a turn boundary.
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
    /// Whether the command lane must drain before a boundary turn admission.
    pub fn session_command_precedes_turn_input(self) -> bool {
        self.session_command.is_some()
    }
}

/// Decoded composition-relevant fields of one open queued-work batch row.
///
/// Backends build these from their candidate rows, presented in
/// `enqueue_seq` ascending order and already filtered to open rows no root
/// admitted.
#[derive(Clone, Debug)]
pub struct TurnLaneCandidate {
    /// Durable batch identity, used to name a row in admission diagnostics.
    pub batch_id: crate::BatchId,
    pub enqueue_seq: u64,
    pub config_patch_command: bool,
    pub delivery_policy: DeliveryPolicy,
    pub kind: QueuedWorkKind,
    pub authority: QueuedWorkAuthority,
    pub merge_key: Option<String>,
    pub enqueued_at_ms: u64,
    turn_causes: Vec<TurnCause>,
}

impl TurnLaneCandidate {
    pub fn from_batch(batch: &QueuedWorkBatch) -> Self {
        let mut turn_causes = Vec::new();
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
                QueuedWorkPayload::SessionCommand { .. } => {}
            }
        }
        Self {
            batch_id: batch.batch_id.clone(),
            enqueue_seq: batch.enqueue_seq,
            config_patch_command,
            delivery_policy: batch.delivery_policy,
            kind: batch.kind,
            authority: batch.authority.clone(),
            merge_key: batch.merge_key.clone(),
            enqueued_at_ms: batch.enqueued_at_ms,
            turn_causes,
        }
    }
}

/// How many candidate rows a backend should scan when selecting up to
/// `max_batches` open batches. Joinable groups are matched as a prefix, so
/// scanning a bounded surplus keeps one round trip sufficient.
pub fn admission_scan_limit(max_batches: usize) -> i64 {
    i64::try_from(max_batches)
        .unwrap_or(i64::MAX)
        .min(i64::MAX - 32)
        + 32
}

/// A longer FIFO prefix remains queued and drains through later commits; bounding this run
/// also bounds every SQL candidate scan that feeds it.
pub const MAX_SESSION_COMMAND_BATCHES_PER_RUN: usize = 64;

/// Non-config commands remain exclusive. A leading `ApplyConfigPatch` extends
/// through the complete adjacent config-patch prefix so one drain can apply N
/// ordered patches in one head commit while completing all N batches.
pub fn select_leading_session_command(candidates: &[TurnLaneCandidate]) -> usize {
    let Some(first) = candidates.first() else {
        return 0;
    };
    if first.kind.work_class() != QueuedWorkClass::SessionCommand {
        return 0;
    }
    if !first.config_patch_command {
        return 1;
    }
    candidates
        .iter()
        .take(MAX_SESSION_COMMAND_BATCHES_PER_RUN)
        .take_while(|candidate| {
            candidate.kind.work_class() == QueuedWorkClass::SessionCommand
                && candidate.config_patch_command
        })
        .count()
}

/// An admission takes a leading prefix of the open candidates.
///
/// * The queue head must be [`QueuedWorkClass::TurnWork`]. Earlier pending
///   session commands are never skipped or materialized as turn input.
/// * An [`AdmissionBoundary::ActiveTurnCheckpoint`] boundary only
///   admits work whose head batch is
///   [`DeliveryPolicy::EarliestSafeBoundary`].
/// * An absent merge key, or a control/cancel kind, admits exactly one batch.
/// * A batchable head extends through immediately following rows with the same
///   delivery policy, merge key, and authority/elevation, within the host's row
///   and age bounds. How much of that eligible prefix actually drains is the
///   host's [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) decision.
pub fn select_turn_work_indices(
    candidates: &[TurnLaneCandidate],
    boundary: AdmissionBoundary,
    policy: &TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
) -> Result<TurnWorkSelection, StoreError> {
    if policy.max_rows == 0 {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            AdmissionRefusal::ZeroLimit,
        ));
    }
    let Some(first) = candidates.first() else {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            AdmissionRefusal::Empty,
        ));
    };
    if first.kind.work_class() != QueuedWorkClass::TurnWork {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            AdmissionRefusal::CommandAtHead,
        ));
    }
    if boundary == AdmissionBoundary::ActiveTurnCheckpoint
        && first.delivery_policy != DeliveryPolicy::EarliestSafeBoundary
    {
        return Ok(refuse(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            AdmissionRefusal::DeliveryBoundaryBlocked,
        ));
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
        record_turn_admission_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            TurnAdmissionOutcome::SingleRow,
        );
        return Ok(select(vec![0]));
    }
    if now_epoch_ms.saturating_sub(first.enqueued_at_ms) >= policy.max_pending_age_ms {
        record_turn_admission_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            TurnAdmissionOutcome::MaxPendingAgeReached,
        );
        return Ok(select(vec![0]));
    }

    let mut compatible_prefix_len = 1;
    for candidate in &candidates[1..] {
        if compatible_prefix_len >= policy.max_rows
            || candidate.kind.work_class() != QueuedWorkClass::TurnWork
            || !candidate.kind.is_batchable()
            || candidate.delivery_policy != first.delivery_policy
            || candidate.merge_key != first.merge_key
            || candidate.authority != first.authority
        {
            break;
        }
        compatible_prefix_len += 1;
    }

    // Lash has now applied every admission law: what remains is a legal, strictly
    // FIFO prefix that *may* share this turn. How much of it actually drains is
    // the host's `QueuedDrainPolicy` decision (FIG-1313), not kernel token
    // arithmetic. The shipped default drains the head alone.
    if compatible_prefix_len == 1 {
        // A lone eligible row always drains: no selection is expressible, so
        // the policy is not consulted and its per-row projections are not
        // rendered.
        record_turn_admission_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            TurnAdmissionOutcome::SingleEligibleRow,
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
    // check above refuses it there by name. Carrying it into this admission
    // instead would fail an admission that could have made progress, and the
    // root's recorded admission would replay that doomed composition forever.
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
    record_turn_admission_decision(
        candidates,
        boundary,
        policy,
        now_epoch_ms,
        selected,
        rendered_tokens,
        TurnAdmissionOutcome::HostDrainPolicy,
    );
    Ok(select((0..selected).collect()))
}

/// A selection that acquired `indices`.
fn select(indices: Vec<usize>) -> TurnWorkSelection {
    debug_assert!(!indices.is_empty(), "a selection must acquire rows");
    TurnWorkSelection::Selected { indices }
}

/// A selection that acquired nothing, recorded under `refusal`.
fn refuse(
    candidates: &[TurnLaneCandidate],
    boundary: AdmissionBoundary,
    policy: &TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
    refusal: AdmissionRefusal,
) -> TurnWorkSelection {
    record_turn_admission_decision(
        candidates,
        boundary,
        policy,
        now_epoch_ms,
        0,
        0,
        TurnAdmissionOutcome::Refused(refusal),
    );
    TurnWorkSelection::Refused { reason: refusal }
}

/// SQL admissions take a contiguous prefix of their candidate scan. Stores
/// that admit by index use [`select_turn_work_indices`] directly.
pub fn select_turn_work_prefix(
    candidates: &[TurnLaneCandidate],
    boundary: AdmissionBoundary,
    policy: &TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
) -> Result<TurnWorkPrefix, StoreError> {
    match select_turn_work_indices(candidates, boundary, policy, now_epoch_ms)? {
        TurnWorkSelection::Refused { reason } => Ok(TurnWorkPrefix::Refused { reason }),
        TurnWorkSelection::Selected { indices } => {
            let len = indices
                .into_iter()
                .enumerate()
                .take_while(|(prefix_index, selected_index)| prefix_index == selected_index)
                .count();
            // Withholding the physically earliest row leaves no contiguous prefix.
            Ok(if len == 0 {
                TurnWorkPrefix::Refused {
                    reason: AdmissionRefusal::HeadWithheld,
                }
            } else {
                TurnWorkPrefix::Selected { len }
            })
        }
    }
}

/// Conservative upper bound for the exact model-visible queued-work render.
///
/// Process wakes use the shared turn-events renderer. One UTF-8 byte is
/// charged as one token: this
/// deliberately overestimates ordinary model tokenizers while remaining safe
/// without moving tokenizer selection from the host/provider boundary into
/// core.
fn rendered_token_upper_bound(candidates: &[TurnLaneCandidate]) -> usize {
    let causes = candidates
        .iter()
        .flat_map(|candidate| candidate.turn_causes.iter().cloned())
        .collect::<Vec<_>>();
    crate::render_turn_causes_prompt(&causes).map_or(0, |rendered| rendered.len())
}

fn record_turn_admission_decision(
    candidates: &[TurnLaneCandidate],
    boundary: AdmissionBoundary,
    policy: &TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
    selected: usize,
    rendered_tokens: usize,
    outcome: TurnAdmissionOutcome,
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
        "wake turn admission decision"
    );
}

/// Derive the durable id for a newly enqueued batch.
///
/// `nonce` disambiguates batches enqueued within the same millisecond;
/// backends whose id uniqueness already comes from elsewhere pass `None`.
pub fn derive_batch_id(
    session_id: &crate::SessionId,
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
