//! Dialect-independent queued-work admission shared by durable backends.
//!
//! Every queued-work row is a session command, and every command applies
//! alone. The SQL backends (sqlite, postgres) load candidate batch rows
//! ordered by `enqueue_seq` and pre-filtered to open batches no run admitted,
//! and take the leading command.

/// version_surface = "coexist"
/// version_guard(items(LASH_QUEUED_WORK_BATCH_DOMAIN_VERSION, derive_batch_id))
const LASH_QUEUED_WORK_BATCH_DOMAIN_VERSION: &str = "lash-queued-work-batch/v2";

use crate::{DeliveryPolicy, QueuedWorkAuthority, QueuedWorkBatch};

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
/// session-command side is exactly the open queued-work rows.
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

/// Decoded admission-relevant fields of one open queued-work batch row.
///
/// Backends build these from their candidate rows, presented in
/// `enqueue_seq` ascending order and already filtered to open rows no run
/// admitted.
#[derive(Clone, Debug)]
pub struct TurnLaneCandidate {
    /// Durable batch identity, used to name a row in admission diagnostics.
    pub batch_id: crate::BatchId,
    pub enqueue_seq: u64,
    pub delivery_policy: DeliveryPolicy,
    pub authority: QueuedWorkAuthority,
    pub merge_key: Option<String>,
    pub enqueued_at_ms: u64,
}

impl TurnLaneCandidate {
    pub fn from_batch(batch: &QueuedWorkBatch) -> Self {
        Self {
            batch_id: batch.batch_id.clone(),
            enqueue_seq: batch.enqueue_seq,
            delivery_policy: batch.delivery_policy,
            authority: batch.authority.clone(),
            merge_key: batch.merge_key.clone(),
            enqueued_at_ms: batch.enqueued_at_ms,
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

/// How many open batches a session-command run takes: every session command
/// applies alone, in the commit that settles it (FIG-4202, FIG-4379).
pub const SESSION_COMMAND_BATCHES_PER_RUN: usize = 1;

/// The leading session-command run of the open candidates: the head batch,
/// or nothing when no batch is open.
pub fn select_leading_session_command(candidates: &[TurnLaneCandidate]) -> usize {
    if candidates.is_empty() {
        0
    } else {
        SESSION_COMMAND_BATCHES_PER_RUN
    }
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
        crate::stable_hash::blake3_hex(LASH_QUEUED_WORK_BATCH_DOMAIN_VERSION, seed.as_bytes())
    )
}
