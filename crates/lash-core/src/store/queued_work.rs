//! Dialect-independent queued-work claim logic shared by durable backends.
//!
//! The SQL backends (sqlite, postgres) load candidate batch rows ordered by
//! `enqueue_seq` and pre-filtered to ready batches that are not held by a
//! live claim, then apply the same pure state machine: a delivery-policy
//! boundary gate, slot-policy/merge-key prefix grouping, and fencing-token /
//! lease derivation. That state machine lives here so the backends own only
//! their SQL reads and writes while the claim contract has a single
//! implementation, exercised against every backend by the shared
//! `runtime_persistence` conformance suite.

use sha2::{Digest, Sha256};

use super::LeaseOwnerIdentity;
use crate::{DeliveryPolicy, MergeKey, QueuedWorkClaimBoundary, SlotPolicy};

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
    pub enqueue_seq: u64,
    pub claim_fencing_token: u64,
    pub work_class: QueuedWorkClass,
    pub delivery_policy: DeliveryPolicy,
    pub slot_policy: SlotPolicy,
    pub merge_key: MergeKey,
}

/// How many candidate rows a backend should scan when selecting up to
/// `max_batches` claimable batches. Joinable groups are matched as a prefix,
/// so scanning a bounded surplus keeps one round trip sufficient.
pub fn claim_scan_limit(max_batches: usize) -> i64 {
    (max_batches as i64).saturating_add(32)
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

/// Select the prefix of turn-work `candidates` that a single claim may take.
///
/// Returns the number of leading candidates to claim (`0` means no claim):
///
/// * The queue head must be [`QueuedWorkClass::TurnWork`]. Earlier ready
///   session commands are never skipped or materialized as turn input.
/// * An [`QueuedWorkClaimBoundary::ActiveTurnCheckpoint`] boundary only
///   admits work whose head batch is
///   [`DeliveryPolicy::EarliestSafeBoundary`].
/// * An [`SlotPolicy::Exclusive`] head claims exactly one batch.
/// * A [`SlotPolicy::Join`] head extends through immediately following
///   `Join` batches with the same delivery policy and merge key, up to
///   `max_batches`.
pub fn select_turn_work_claim_prefix(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    max_batches: usize,
) -> usize {
    if max_batches == 0 {
        return record_turn_claim_decision(candidates, boundary, max_batches, 0, "zero_limit");
    }
    let Some(first) = candidates.first() else {
        return record_turn_claim_decision(candidates, boundary, max_batches, 0, "empty");
    };
    if first.work_class != QueuedWorkClass::TurnWork {
        return record_turn_claim_decision(candidates, boundary, max_batches, 0, "command_at_head");
    }
    if boundary == QueuedWorkClaimBoundary::ActiveTurnCheckpoint
        && first.delivery_policy != DeliveryPolicy::EarliestSafeBoundary
    {
        return record_turn_claim_decision(
            candidates,
            boundary,
            max_batches,
            0,
            "delivery_boundary_blocked",
        );
    }
    if first.slot_policy != SlotPolicy::Join || first.merge_key == MergeKey::Never {
        return record_turn_claim_decision(candidates, boundary, max_batches, 1, "single_wake");
    }
    let mut selected = 1;
    for candidate in &candidates[1..] {
        if selected >= max_batches
            || candidate.work_class != QueuedWorkClass::TurnWork
            || candidate.slot_policy != SlotPolicy::Join
            || candidate.delivery_policy != first.delivery_policy
            || candidate.merge_key != first.merge_key
        {
            break;
        }
        selected += 1;
    }
    record_turn_claim_decision(
        candidates,
        boundary,
        max_batches,
        selected,
        "coalesced_prefix",
    )
}

fn record_turn_claim_decision(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    max_batches: usize,
    selected: usize,
    outcome: &'static str,
) -> usize {
    tracing::info!(
        target: "lash::wake_turn_policy",
        ?boundary,
        max_batches,
        candidates = ?candidates,
        selected,
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
    ) -> Self {
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
    ) -> Self {
        let fencing_token = claim_fencing_token.saturating_add(1);
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
        Self {
            claim_id,
            lease_token,
            fencing_token,
            session_lease_generation,
        }
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

    fn candidate(
        enqueue_seq: u64,
        work_class: QueuedWorkClass,
        delivery_policy: DeliveryPolicy,
        slot_policy: SlotPolicy,
        merge_key: MergeKey,
    ) -> ClaimCandidate {
        ClaimCandidate {
            enqueue_seq,
            claim_fencing_token: 0,
            work_class,
            delivery_policy,
            slot_policy,
            merge_key,
        }
    }

    #[test]
    fn exclusive_head_claims_exactly_one() {
        let candidates = vec![
            candidate(
                1,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                MergeKey::Never,
            ),
            candidate(
                2,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                MergeKey::Never,
            ),
        ];
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 8),
            1
        );
    }

    #[test]
    fn join_head_groups_matching_prefix_up_to_max() {
        let join = |seq| {
            candidate(
                seq,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::PayloadDefault,
            )
        };
        let candidates = vec![join(1), join(2), join(3), join(4)];
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 3),
            3
        );
    }

    #[test]
    fn join_slot_with_never_merge_key_claims_each_wake_separately() {
        let candidates = vec![
            candidate(
                1,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::Never,
            ),
            candidate(
                2,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::Never,
            ),
        ];
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 8),
            1
        );
    }

    #[test]
    fn exclusive_slot_with_group_key_still_claims_one() {
        let candidates = vec![
            candidate(
                1,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                MergeKey::Group("inert-under-exclusive".to_string()),
            ),
            candidate(
                2,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                MergeKey::Group("inert-under-exclusive".to_string()),
            ),
        ];
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 8),
            1
        );
    }

    #[test]
    fn join_group_breaks_on_policy_or_merge_key_mismatch() {
        let candidates = vec![
            candidate(
                1,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::Group("a".to_string()),
            ),
            candidate(
                2,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::Group("b".to_string()),
            ),
        ];
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 8),
            1
        );
    }

    #[test]
    fn active_turn_checkpoint_boundary_gates_on_delivery_policy() {
        let candidates = vec![candidate(
            1,
            QueuedWorkClass::TurnWork,
            DeliveryPolicy::AfterCurrentTurnCommit,
            SlotPolicy::Exclusive,
            MergeKey::Never,
        )];
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                8
            ),
            0
        );
    }

    #[test]
    fn leading_session_command_blocks_turn_work_claim() {
        let candidates = vec![
            candidate(
                1,
                QueuedWorkClass::SessionCommand,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                MergeKey::Never,
            ),
            candidate(
                2,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Exclusive,
                MergeKey::Never,
            ),
        ];
        assert_eq!(select_leading_session_command(&candidates), 1);
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 8),
            0
        );
    }

    #[test]
    fn later_session_command_does_not_join_turn_work_claim() {
        let candidates = vec![
            candidate(
                1,
                QueuedWorkClass::TurnWork,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::PayloadDefault,
            ),
            candidate(
                2,
                QueuedWorkClass::SessionCommand,
                DeliveryPolicy::EarliestSafeBoundary,
                SlotPolicy::Join,
                MergeKey::PayloadDefault,
            ),
        ];
        assert_eq!(select_leading_session_command(&candidates), 0);
        assert_eq!(
            select_turn_work_claim_prefix(&candidates, QueuedWorkClaimBoundary::Idle, 8),
            1
        );
    }

    #[test]
    fn lease_derivation_is_deterministic_and_advances_fencing() {
        let head = ClaimCandidate {
            enqueue_seq: 7,
            claim_fencing_token: 2,
            work_class: QueuedWorkClass::TurnWork,
            delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
            slot_policy: SlotPolicy::Exclusive,
            merge_key: MergeKey::Never,
        };
        let owner = LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
        let lease = WorkClaimLease::derive_queued_work(&head, "session", &owner, 1_000, 5);
        assert_eq!(lease.fencing_token, 3);
        assert_eq!(lease.claim_id, "qwc:7:3");
        assert_eq!(lease.session_lease_generation, 5);
        let again = WorkClaimLease::derive_queued_work(&head, "session", &owner, 1_000, 5);
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
