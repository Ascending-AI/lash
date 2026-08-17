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

use sha2::{Digest, Sha256};

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

/// Whether a durable queued-work row carries a session command or turn work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkClass {
    SessionCommand,
    TurnWork,
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

/// Select a leading session-command batch.
///
/// Returns `1` only when the earliest ready claimable batch is a
/// [`QueuedWorkClass::SessionCommand`]. Session commands are intentionally
/// claimed one batch at a time so the runtime applies each mutation and
/// completion through its normal fenced commit path before moving to the next.
pub fn select_leading_session_command(candidates: &[ClaimCandidate]) -> usize {
    if candidates
        .first()
        .is_some_and(|candidate| candidate.work_class == QueuedWorkClass::SessionCommand)
    {
        1
    } else {
        0
    }
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
) -> Result<Vec<usize>, StoreError> {
    if policy.max_rows == 0 {
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            0,
            0,
            "zero_limit",
        );
        return Ok(Vec::new());
    }
    let Some(first) = candidates.first() else {
        record_turn_claim_decision(candidates, boundary, policy, now_epoch_ms, 0, 0, "empty");
        return Ok(Vec::new());
    };
    if first.work_class != QueuedWorkClass::TurnWork {
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            0,
            0,
            "command_at_head",
        );
        return Ok(Vec::new());
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
            if !selected.is_empty() {
                return Ok(selected
                    .into_iter()
                    .map(|index| remaining_indices[index])
                    .collect());
            }
        }
        record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            0,
            0,
            "delivery_boundary_blocked",
        );
        return Ok(Vec::new());
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
            "interrupted_claim_redrive",
        );
        return Ok(selected);
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
        let selected = record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            "single_row",
        );
        return Ok((0..selected).collect());
    }
    if now_epoch_ms.saturating_sub(first.enqueued_at_ms) >= policy.max_pending_age_ms {
        let selected = record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            "max_pending_age_reached",
        );
        return Ok((0..selected).collect());
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
        let selected = record_turn_claim_decision(
            candidates,
            boundary,
            policy,
            now_epoch_ms,
            1,
            first_tokens,
            "single_eligible_row",
        );
        return Ok((0..selected).collect());
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
    // Every row a policy pulls in must clear the oversize law on its own, not
    // just the head: a non-head row larger than the whole window would be
    // rejected by the provider on every attempt, and the interrupted-claim
    // redrive would restore that same doomed composition forever.
    for candidate in &candidates[..selected] {
        let candidate_tokens = rendered_token_upper_bound(std::slice::from_ref(candidate));
        if candidate_tokens > policy.max_context_tokens {
            return Err(StoreError::QueuedWorkRowExceedsContextWindow {
                batch_id: candidate.batch_id.clone(),
                batch_enqueue_seq: candidate.enqueue_seq,
                rendered_tokens: candidate_tokens,
                max_context_tokens: policy.max_context_tokens,
            });
        }
    }
    let rendered_tokens = rendered_token_upper_bound(&candidates[..selected]);
    tracing::debug!(
        target: "lash::queued_work_batching",
        drain_policy = policy.drain_policy.name(),
        offered = compatible_prefix_len,
        requested,
        selected,
        "queued drain policy selection"
    );
    let selected = record_turn_claim_decision(
        candidates,
        boundary,
        policy,
        now_epoch_ms,
        selected,
        rendered_tokens,
        "host_drain_policy",
    );
    Ok((0..selected).collect())
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
) -> Result<usize, StoreError> {
    let selected = select_turn_work_claim_indices(candidates, boundary, policy, now_epoch_ms)?;
    Ok(selected
        .iter()
        .copied()
        .enumerate()
        .take_while(|(prefix_index, selected_index)| prefix_index == selected_index)
        .count())
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
/// Every Lash claim law still applies: the head class, the delivery boundary,
/// merge-key/authority/kind compatibility, `max_rows`, and the oversized-row
/// refusal all bound the exact request exactly as they bound an automatic one.
pub fn select_exact_turn_work_claim_prefix(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<usize, StoreError> {
    let policy = QueuedWorkClaimPolicy {
        drain_policy: crate::runtime::exact_selection_drain_policy(),
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
    outcome: &'static str,
) -> usize {
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
        outcome,
        "wake turn claim decision"
    );
    selected
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
        let lease_token = format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{}:{}:{}:{}:{}",
                    session_id, owner.owner_id, owner.incarnation_id, claim_id, now_epoch_ms
                )
                .as_bytes(),
            )
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
    format!("qwb:{:x}", Sha256::digest(seed.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{
        collection::vec,
        prelude::*,
        test_runner::{Config, RngSeed, TestRunner},
    };

    #[test]
    fn claim_id_dialects_preserve_existing_spelling() {
        let cases = [
            (ClaimIdDialect::QueuedWork, "qwc:7:3"),
            (ClaimIdDialect::TurnInput, "tic:7:3"),
            (ClaimIdDialect::RecordingQueuedWork, "recording-qwc:7:3"),
            (ClaimIdDialect::RecordingTurnInput, "recording-tic:7:3"),
            (ClaimIdDialect::PerformanceQueuedWork, "perf-qwc:7:3"),
            (ClaimIdDialect::PerformanceTurnInput, "perf-tic:7:3"),
        ];
        for (dialect, expected) in cases {
            assert_eq!(derive_claim_id(dialect, 7, 3), expected);
        }
    }

    fn candidate(enqueue_seq: u64, merge_key: Option<&str>) -> ClaimCandidate {
        ClaimCandidate {
            batch_id: format!("qwb-{enqueue_seq}"),
            enqueue_seq,
            claim_fencing_token: 0,
            prior_claim_id: None,
            work_class: QueuedWorkClass::TurnWork,
            delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
            kind: QueuedWorkKind::Turn,
            authority: QueuedWorkAuthority::new("principal"),
            merge_key: merge_key.map(str::to_string),
            enqueued_at_ms: 900,
            turn_causes: Vec::new(),
            input_texts: vec!["wake".to_string()],
        }
    }

    fn rendered_candidate_strategy() -> impl Strategy<Value = ClaimCandidate> {
        let merge_key = prop_oneof![
            Just(None),
            Just(Some("wake".to_string())),
            Just(Some("other".to_string())),
        ];
        let kind = prop_oneof![
            Just(QueuedWorkKind::Turn),
            Just(QueuedWorkKind::Control),
            Just(QueuedWorkKind::Cancel),
        ];
        let work_class = prop_oneof![
            Just(QueuedWorkClass::TurnWork),
            Just(QueuedWorkClass::SessionCommand),
        ];
        let delivery_policy = prop_oneof![
            Just(DeliveryPolicy::EarliestSafeBoundary),
            Just(DeliveryPolicy::AfterCurrentTurnCommit),
        ];
        let authority = prop_oneof![
            Just(QueuedWorkAuthority::default()),
            Just(QueuedWorkAuthority::new("principal-a")),
            Just(QueuedWorkAuthority::new("principal-b").with_elevation("root")),
        ];
        let input_texts = vec(0usize..=256, 0..=3).prop_map(|lengths| {
            lengths
                .into_iter()
                .enumerate()
                .map(|(index, length)| ((b'a' + index as u8) as char).to_string().repeat(length))
                .collect::<Vec<_>>()
        });
        let turn_causes = vec((0usize..=128, 0u8..3), 0..=4).prop_map(|causes| {
            causes
                .into_iter()
                .enumerate()
                .map(|(index, (text_len, origin_kind))| TurnCause {
                    id: format!("cause-{index}"),
                    event_type: format!("event-{origin_kind}"),
                    origin: match origin_kind {
                        0 => crate::MessageOrigin::Plugin {
                            plugin_id: format!("plugin-{index}"),
                            transient: index % 2 == 0,
                        },
                        1 => crate::MessageOrigin::Process {
                            process_id: format!("process-{index}"),
                            event_type: "wake".to_string(),
                            sequence: index as u64,
                            wake_id: Some(format!("wake-{index}")),
                            caused_by: None,
                        },
                        _ => crate::MessageOrigin::TurnInput {
                            turn_id: format!("turn-{index}"),
                            input_id: (index % 2 == 0).then(|| format!("input-{index}")),
                        },
                    },
                    text: "x".repeat(text_len),
                })
                .collect::<Vec<_>>()
        });

        (
            any::<u64>(),
            merge_key,
            kind,
            work_class,
            delivery_policy,
            authority,
            input_texts,
            turn_causes,
        )
            .prop_map(
                |(
                    enqueue_seq,
                    merge_key,
                    kind,
                    work_class,
                    delivery_policy,
                    authority,
                    input_texts,
                    turn_causes,
                )| ClaimCandidate {
                    batch_id: format!("qwb-{enqueue_seq}"),
                    enqueue_seq,
                    claim_fencing_token: 0,
                    prior_claim_id: None,
                    work_class,
                    delivery_policy,
                    kind,
                    authority,
                    merge_key,
                    enqueued_at_ms: 900,
                    turn_causes,
                    input_texts,
                },
            )
    }

    #[test]
    fn exact_selection_requires_the_literal_interrupted_composition() {
        let candidates = vec![
            ("w1".to_string(), Some("claim-a".to_string())),
            ("fresh".to_string(), None),
            ("w2".to_string(), Some("claim-a".to_string())),
        ];
        assert_eq!(
            select_interrupted_exact_claim_indices(&candidates, &["w1".to_string()]),
            Err(vec!["w1".to_string(), "w2".to_string()])
        );
        assert_eq!(
            select_interrupted_exact_claim_indices(
                &candidates,
                &["w1".to_string(), "w2".to_string()],
            ),
            Ok(Some(vec![0, 2]))
        );

        let two_claims = vec![
            ("a1".to_string(), Some("claim-a".to_string())),
            ("a2".to_string(), Some("claim-a".to_string())),
            ("b1".to_string(), Some("claim-b".to_string())),
            ("b2".to_string(), Some("claim-b".to_string())),
        ];
        assert_eq!(
            select_interrupted_exact_claim_indices(
                &two_claims,
                &["a1".to_string(), "a2".to_string(), "b1".to_string()],
            ),
            Err(vec!["b1".to_string(), "b2".to_string()])
        );
        assert_eq!(
            select_interrupted_exact_claim_indices(
                &two_claims,
                &[
                    "a1".to_string(),
                    "a2".to_string(),
                    "b1".to_string(),
                    "b2".to_string(),
                ],
            ),
            Ok(Some(vec![0, 1]))
        );
    }

    fn policy(max_context_tokens: usize, action_token_reserve: usize) -> QueuedWorkClaimPolicy {
        QueuedWorkClaimPolicy {
            max_context_tokens,
            action_token_reserve,
            max_rows: 64,
            max_pending_age_ms: 1_000,
            drain_policy: crate::default_queued_drain_policy(),
        }
    }

    #[test]
    fn absent_merge_key_never_merges() {
        let candidates = vec![candidate(1, None), candidate(2, None)];
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 100),
                1_000,
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn matching_key_groups_prefix_up_to_row_bound() {
        let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
        let mut claim_policy = policy(1_000, 100);
        claim_policy.max_rows = 1;
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &claim_policy,
                1_000,
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn authority_and_elevation_are_independent_compatibility_gates() {
        let first = candidate(1, Some("wake"));
        let mut different_principal = candidate(2, Some("wake"));
        different_principal.authority = QueuedWorkAuthority::new("other");
        let mut different_elevation = candidate(2, Some("wake"));
        different_elevation.authority =
            QueuedWorkAuthority::new("principal").with_elevation("root");
        for candidates in [
            vec![first.clone(), different_principal],
            vec![first.clone(), different_elevation],
        ] {
            assert_eq!(
                select_turn_work_claim_prefix(
                    &candidates,
                    QueuedWorkClaimBoundary::Idle,
                    &policy(1_000, 100),
                    1_000
                )
                .unwrap(),
                1
            );
        }
    }

    #[test]
    fn control_and_cancel_kinds_never_batch() {
        for kind in [QueuedWorkKind::Control, QueuedWorkKind::Cancel] {
            let mut first = candidate(1, Some("wake"));
            first.kind = kind;
            let candidates = vec![first, candidate(2, Some("wake"))];
            assert_eq!(
                select_turn_work_claim_prefix(
                    &candidates,
                    QueuedWorkClaimBoundary::Idle,
                    &policy(1_000, 100),
                    1_000
                )
                .unwrap(),
                1
            );
        }
    }

    #[test]
    fn merge_key_delivery_and_work_class_mismatches_break_prefix() {
        let first = candidate(1, Some("a"));
        let mut different_delivery = candidate(2, Some("a"));
        different_delivery.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;
        let mut command = candidate(2, Some("a"));
        command.work_class = QueuedWorkClass::SessionCommand;
        for candidates in [
            vec![first.clone(), candidate(2, Some("b"))],
            vec![first.clone(), different_delivery],
            vec![first.clone(), command],
        ] {
            assert_eq!(
                select_turn_work_claim_prefix(
                    &candidates,
                    QueuedWorkClaimBoundary::Idle,
                    &policy(1_000, 100),
                    1_000
                )
                .unwrap(),
                1
            );
        }
    }

    #[test]
    fn the_default_drain_policy_claims_one_row_however_much_window_is_free() {
        let mut first = candidate(1, Some("wake"));
        first.input_texts = vec!["a".repeat(4)];
        let mut second = candidate(2, Some("wake"));
        second.input_texts = vec!["b".repeat(4)];
        // Both rows fit the window several times over; the shipped
        // one-at-a-time policy still drains only the head (FIG-1313).
        assert_eq!(
            select_turn_work_claim_prefix(
                &[first, second],
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 300),
                1_000
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn all_mode_claims_the_whole_compatible_prefix_without_token_arithmetic() {
        let candidates = vec![
            candidate(1, Some("wake")),
            candidate(2, Some("wake")),
            candidate(3, Some("wake")),
        ];
        let mut claim_policy = policy(101, 30);
        claim_policy.drain_policy =
            std::sync::Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
        // The three rows render past this deliberately tiny window. `All` is a
        // host statement that the provider is the authority on what fits, so
        // Lash coalesces every compatible row anyway.
        assert_eq!(
            select_turn_work_claim_indices(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &claim_policy,
                1_000,
            )
            .unwrap(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn a_custom_policy_selection_is_clamped_to_the_legal_prefix() {
        #[derive(Debug)]
        struct GreedyPolicy;
        impl crate::QueuedDrainPolicy for GreedyPolicy {
            fn name(&self) -> &str {
                "test_greedy"
            }

            fn select_drain(
                &self,
                request: &crate::QueuedDrainRequest<'_>,
            ) -> crate::QueuedDrainSelection {
                // Every offered candidate carries a projection and a budget.
                assert!(
                    request
                        .candidates()
                        .iter()
                        .all(|candidate| candidate.projected_tokens > 0)
                );
                assert_eq!(request.max_context_tokens(), 1_000);
                crate::QueuedDrainSelection::leading(usize::MAX)
            }
        }

        let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
        let mut claim_policy = policy(1_000, 100);
        claim_policy.drain_policy = std::sync::Arc::new(GreedyPolicy);
        assert_eq!(
            select_turn_work_claim_indices(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &claim_policy,
                1_000,
            )
            .unwrap(),
            vec![0, 1]
        );

        #[derive(Debug)]
        struct EmptyPolicy;
        impl crate::QueuedDrainPolicy for EmptyPolicy {
            fn name(&self) -> &str {
                "test_empty"
            }

            fn select_drain(
                &self,
                _request: &crate::QueuedDrainRequest<'_>,
            ) -> crate::QueuedDrainSelection {
                crate::QueuedDrainSelection::leading(0)
            }
        }

        let mut empty_policy = policy(1_000, 100);
        empty_policy.drain_policy = std::sync::Arc::new(EmptyPolicy);
        // A policy cannot starve its own queue: the head always drains.
        assert_eq!(
            select_turn_work_claim_indices(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &empty_policy,
                1_000,
            )
            .unwrap(),
            vec![0]
        );
    }

    #[test]
    fn an_exact_host_selection_is_never_sized_by_the_automatic_drain_policy() {
        let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
        let claim_policy = policy(1_000, 100);
        assert_eq!(claim_policy.drain_policy.name(), "one_at_a_time");
        // Automatic drains take the head alone under the shipped default...
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &claim_policy,
                1_000,
            )
            .unwrap(),
            1
        );
        // ...but the host named this exact two-row composition, and a partial
        // exact claim is abandoned as unclaimable by the caller, so shrinking it
        // would wedge `stream_selected_queued_work` forever.
        assert_eq!(
            select_exact_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &claim_policy,
                1_000,
            )
            .unwrap(),
            2
        );
        // Lash's own laws still bound the exact request.
        let mut bounded = claim_policy.clone();
        bounded.max_rows = 1;
        assert_eq!(
            select_exact_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &bounded,
                1_000,
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn every_selected_row_must_fit_the_window_not_only_the_head() {
        let mut first = candidate(1, Some("wake"));
        first.input_texts = vec!["a".repeat(8)];
        let mut second = candidate(2, Some("wake"));
        second.input_texts = vec!["b".repeat(4_000)];
        let mut claim_policy = policy(1_000, 100);
        claim_policy.drain_policy =
            std::sync::Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
        // The head fits, so a head-only check would admit the oversized second
        // row; the provider would then reject the drain and the interrupted
        // redrive would restore that identical doomed composition forever.
        let error = select_turn_work_claim_indices(
            &[first.clone(), second],
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
            1_000,
        )
        .expect_err("an oversized non-head row must be refused by name");
        match error {
            StoreError::QueuedWorkRowExceedsContextWindow {
                batch_id,
                batch_enqueue_seq,
                rendered_tokens,
                max_context_tokens,
            } => {
                assert_eq!(batch_id, "qwb-2");
                assert_eq!(batch_enqueue_seq, 2);
                assert!(rendered_tokens > max_context_tokens);
                assert_eq!(max_context_tokens, 1_000);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // A row the policy did not select cannot refuse a drain it never joined.
        let mut third = candidate(2, Some("wake"));
        third.input_texts = vec!["c".repeat(4_000)];
        assert_eq!(
            select_turn_work_claim_prefix(
                &[first, third],
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 100),
                1_000,
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn an_interrupted_redrive_never_consults_the_drain_policy() {
        #[derive(Debug)]
        struct PanickingPolicy;
        impl crate::QueuedDrainPolicy for PanickingPolicy {
            fn name(&self) -> &str {
                "test_panicking"
            }

            fn select_drain(
                &self,
                _request: &crate::QueuedDrainRequest<'_>,
            ) -> crate::QueuedDrainSelection {
                panic!("replayed drains must serve the journaled selection");
            }
        }

        let mut first = candidate(1, Some("wake"));
        first.prior_claim_id = Some("qwc:1:1".to_string());
        let mut second = candidate(2, Some("wake"));
        second.prior_claim_id = Some("qwc:1:1".to_string());
        let mut claim_policy = policy(1_000, 100);
        claim_policy.drain_policy = std::sync::Arc::new(PanickingPolicy);
        assert_eq!(
            select_turn_work_claim_indices(
                &[first, second],
                QueuedWorkClaimBoundary::Idle,
                &claim_policy,
                1_000,
            )
            .unwrap(),
            vec![0, 1]
        );
    }

    #[test]
    fn rendered_bound_is_monotonic_over_prefixes() {
        const SEED: u64 = 0x5eed_f101_4004_0002;
        let mut runner = TestRunner::new(Config {
            cases: 512,
            failure_persistence: None,
            rng_seed: RngSeed::Fixed(SEED),
            ..Config::default()
        });

        runner
            .run(&vec(rendered_candidate_strategy(), 1..=8), |candidates| {
                for prefix_len in 1..candidates.len() {
                    let prefix_bound =
                        rendered_token_upper_bound(&candidates[..prefix_len]);
                    let extended_bound =
                        rendered_token_upper_bound(&candidates[..=prefix_len]);
                    prop_assert!(
                        prefix_bound <= extended_bound,
                        "seed={SEED:#x}, prefix_len={prefix_len}, prefix_bound={prefix_bound}, extended_bound={extended_bound}, candidates={candidates:#?}"
                    );
                }
                Ok(())
            })
            .expect("rendered token bound must be monotonic over prefixes");
    }

    #[test]
    fn oversized_for_reserve_but_fitting_context_is_attempted_alone() {
        let mut first = candidate(1, Some("wake"));
        first.input_texts = vec!["a".repeat(800)];
        assert_eq!(
            select_turn_work_claim_prefix(
                &[first],
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 300),
                1_000
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn row_that_cannot_fit_context_fails_loudly() {
        let mut first = candidate(7, Some("wake"));
        first.input_texts = vec!["a".repeat(1_001)];
        assert!(matches!(
            select_turn_work_claim_prefix(
                &[first],
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 300),
                1_000
            ),
            Err(StoreError::QueuedWorkRowExceedsContextWindow {
                batch_enqueue_seq: 7,
                ..
            })
        ));
    }

    #[test]
    fn active_turn_checkpoint_boundary_gates_on_delivery_policy() {
        let mut first = candidate(1, None);
        first.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;
        assert_eq!(
            select_turn_work_claim_prefix(
                &[first],
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                &policy(1_000, 100),
                1_000,
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn leading_session_command_blocks_turn_work_claim() {
        let mut command = candidate(1, None);
        command.work_class = QueuedWorkClass::SessionCommand;
        command.kind = QueuedWorkKind::Control;
        let candidates = vec![command, candidate(2, None)];
        assert_eq!(select_leading_session_command(&candidates), 1);
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 100),
                1_000
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn overdue_head_is_claimed_alone_at_claim_time() {
        let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 100),
                2_000
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn lease_derivation_is_deterministic_and_advances_fencing() {
        let head = ClaimCandidate {
            batch_id: "qwb-7".to_string(),
            enqueue_seq: 7,
            claim_fencing_token: 2,
            prior_claim_id: None,
            work_class: QueuedWorkClass::TurnWork,
            delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
            kind: QueuedWorkKind::Turn,
            authority: QueuedWorkAuthority::default(),
            merge_key: None,
            enqueued_at_ms: 0,
            turn_causes: Vec::new(),
            input_texts: Vec::new(),
        };
        let owner = LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
        let lease = WorkClaimLease::derive_queued_work(&head, "session", &owner, 1_000, 5)
            .expect("derive lease");
        assert_eq!(lease.fencing_token, 3);
        assert_eq!(lease.claim_id, "qwc:7:3");
        assert_eq!(lease.session_lease_generation, 5);
        let again = WorkClaimLease::derive_queued_work(&head, "session", &owner, 1_000, 5)
            .expect("derive lease again");
        assert_eq!(lease.lease_token, again.lease_token);
        assert_eq!(
            lease.lease_token,
            "1f1c63a8753631156dc9fd52d5493cd8855684f3a28d65dded7f8f1a57c7c48e"
        );
    }

    #[test]
    fn batch_id_includes_optional_nonce() {
        let plain = derive_batch_id("session", Some("key"), 1_000, None);
        let nonced = derive_batch_id("session", Some("key"), 1_000, Some(1));
        assert_ne!(plain, nonced);
        assert!(plain.starts_with("qwb:"));
    }
}
