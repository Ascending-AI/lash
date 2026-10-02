//! Host-owned selection of how much turn-lane work one run takes.
//!
//! Lash owns the *laws* of a composition: of queued work, the queue head must
//! be turn work, a delivery boundary must admit it, and only batchable turn
//! work sharing the head's delivery policy may travel with it; of next-turn
//! host input, only inputs sharing the head's run spec may travel with it.
//! Either stops at the other family's earliest open row (ADR 0101 §5.2).
//! What Lash does not own is *how much* of that legal, strictly FIFO-ordered
//! prefix a host wants to execute in one turn. That is a product decision —
//! throughput against per-turn context pressure, and which producers may
//! share a turn — so it is a host policy seam ([`QueuedDrainPolicy`]) rather
//! than kernel arithmetic. One policy decides for both families; each
//! candidate names its [`QueuedDrainFamily`].
//!
//! Each candidate carries its own `authority` and `merge_key`. Lash does not
//! compare them: it applies no authorization policy. A host that keeps
//! principals apart returns the length of the run that shares the head's
//! principal:
//!
//! ```
//! use lash_core_store::queued_drain_policy::{
//!     QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection,
//! };
//!
//! /// Drains the leading run of rows stamped with the head's principal.
//! #[derive(Debug)]
//! struct OnePrincipalPerTurn;
//!
//! impl QueuedDrainPolicy for OnePrincipalPerTurn {
//!     fn name(&self) -> &str {
//!         "one_principal_per_turn"
//!     }
//!
//!     fn select_drain(&self, request: &QueuedDrainRequest<'_>) -> QueuedDrainSelection {
//!         let candidates = request.candidates();
//!         let Some(head) = candidates.first() else {
//!             return QueuedDrainSelection::head_only();
//!         };
//!         let run = candidates
//!             .iter()
//!             .take_while(|candidate| candidate.authority.principal == head.authority.principal)
//!             .count();
//!         QueuedDrainSelection::leading(run)
//!     }
//! }
//! ```
//!
//! The same shape keeps merge-key groups apart by comparing `merge_key`.
//!
//! The shipped default performs no token arithmetic at all: it is the two-mode
//! [`DrainModePolicy`], defaulting to [`DrainMode::OneAtATime`]. A drain then
//! either fits the model window or names an irreducibly oversized row, and the
//! provider stays the authority on everything in between.
//!
//! The selection a policy returns is journaled by the claim itself: the durable
//! rows carry the resulting claim id, and redriving an interrupted claim
//! restores that exact composition without consulting the policy again. A host
//! may therefore change or replace its policy without forking in-flight
//! history.

use std::sync::Arc;

use crate::{AdmissionBoundary, QueuedWorkAuthority};

/// The admission family of the rows one drain offers.
///
/// A composition takes rows of one family only: it stops at the other
/// family's earliest open row (ADR 0101 §5.2), so every candidate of one
/// [`QueuedDrainRequest`] names the same family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QueuedDrainFamily {
    /// Next-turn host input sharing the head's run spec, at an idle
    /// admission. Host input carries no merge key and no authority: its
    /// candidates offer `None` and the default authority.
    HostInput,
    /// Batchable queued turn work, such as process wakes, sharing the head's
    /// delivery policy.
    QueuedTurnWork,
}

/// One claimable turn-lane row offered to a [`QueuedDrainPolicy`].
///
/// Candidates are presented in durable `enqueue_seq` order and are already
/// filtered to rows that may legally share this turn with the queue head.
/// Their `authority` and `merge_key` are per-row data: candidates may differ
/// in both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedDrainCandidate {
    /// Durable queue position, ascending and unique per session.
    pub enqueue_seq: u64,
    /// The admission family this row belongs to.
    pub family: QueuedDrainFamily,
    /// This row's producer-selected grouping label.
    pub merge_key: Option<String>,
    /// This row's producer-stamped execution authority.
    pub authority: QueuedWorkAuthority,
    /// Conservative model-context cost Lash already computed for this row
    /// alone, charging one serialized UTF-8 byte as one token.
    pub projected_tokens: usize,
    /// How long this row has waited since it was enqueued.
    pub pending_age_ms: u64,
}

/// The queue snapshot and window budget handed to a policy for one wake.
///
/// Fields are exposed through accessors so later budget evidence can be added
/// without breaking host implementations.
#[derive(Clone, Copy, Debug)]
pub struct QueuedDrainRequest<'a> {
    candidates: &'a [QueuedDrainCandidate],
    available_tokens: usize,
    max_context_tokens: usize,
    max_rows: usize,
    boundary: AdmissionBoundary,
}

impl<'a> QueuedDrainRequest<'a> {
    pub fn new(
        candidates: &'a [QueuedDrainCandidate],
        available_tokens: usize,
        max_context_tokens: usize,
        max_rows: usize,
        boundary: AdmissionBoundary,
    ) -> Self {
        Self {
            candidates,
            available_tokens,
            max_context_tokens,
            max_rows,
            boundary,
        }
    }

    /// Rows eligible to drain together on this wake, in strict queue order.
    ///
    /// Always holds at least two rows: a lone eligible row leaves nothing to
    /// select, so Lash drains it without consulting a policy.
    pub fn candidates(&self) -> &'a [QueuedDrainCandidate] {
        self.candidates
    }

    /// Context capacity left for queued rows after the host's model-action
    /// reserve is withheld.
    ///
    /// Advisory evidence for a custom policy. The shipped default ignores it.
    pub fn available_tokens(&self) -> usize {
        self.available_tokens
    }

    /// The model's full context window in conservative tokens.
    pub fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    /// The host's fresh-claim row bound for this family; the candidate list
    /// is already capped by it.
    pub fn max_rows(&self) -> usize {
        self.max_rows
    }

    /// The claim boundary this wake is draining at.
    pub fn boundary(&self) -> AdmissionBoundary {
        self.boundary
    }
}

/// How many of the offered candidates drain on this wake.
///
/// A selection is a leading count rather than an arbitrary row set: strict FIFO
/// is a Lash law, so reordering or skipping queued work is not expressible.
/// Lash clamps the count into `1..=candidates.len()`; the rows beyond it stay
/// queued and the existing high-water wake path drains them next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuedDrainSelection {
    drain_count: usize,
}

impl QueuedDrainSelection {
    /// Drains the leading `count` candidates and re-queues the remainder.
    pub const fn leading(count: usize) -> Self {
        Self { drain_count: count }
    }

    /// Drains exactly the queue head.
    pub const fn head_only() -> Self {
        Self::leading(1)
    }

    /// Drains every offered candidate.
    pub fn everything(request: &QueuedDrainRequest<'_>) -> Self {
        Self::leading(request.candidates().len())
    }

    /// The requested leading row count, before Lash clamps it to the offered
    /// candidates.
    pub const fn drain_count(self) -> usize {
        self.drain_count
    }
}

/// Host-owned choice of how much queued work one wake drains.
///
/// Implementations must be deterministic for a given request: the selection is
/// committed with the claim, and a host that later changes its mind changes
/// only future drains, never journaled history.
pub trait QueuedDrainPolicy: std::fmt::Debug + Send + Sync {
    /// Stable identifier recorded in drain traces, e.g. `"one_at_a_time"`.
    fn name(&self) -> &str;

    /// Called only when more than one row is eligible, and the returned count
    /// is clamped into `1..=candidates.len()`: a policy can neither starve its
    /// own queue nor reach past the rows Lash offered.
    ///
    /// # Where this runs
    ///
    /// Inside the store's claim critical section — an open Postgres
    /// transaction, or SQLite's blocking connection closure — behind the
    /// session lease fence, with the claim about to commit. An implementation
    /// must therefore be:
    ///
    /// * **deterministic** for a given request, since the answer is committed
    ///   with the claim and redriving that claim never asks again;
    /// * **non-blocking**: no I/O, no locks, no async, no store calls. A slow
    ///   selection holds a database transaction open for every session.
    /// * **panic-free**: a panic here unwinds the claim, not just the turn.
    ///
    /// Exact host-named selections do not call this at all: the host already
    /// chose the composition.
    fn select_drain(&self, request: &QueuedDrainRequest<'_>) -> QueuedDrainSelection;
}

/// The two shipped drain shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DrainMode {
    /// One row per run, strict FIFO. The Lash default: each turn carries
    /// the prompt plus exactly one row, which either fits the model window or
    /// is irreducibly oversized and named as such, and each next-turn host
    /// input is its own run.
    #[default]
    OneAtATime,
    /// Every eligible pending row in one run, strict FIFO, for large-window
    /// hosts that want throughput and accept the provider as the authority on
    /// what fits. Host inputs that share a run share its answer and its
    /// cancellation.
    All,
}

impl DrainMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneAtATime => "one_at_a_time",
            Self::All => "all",
        }
    }
}

/// The default [`QueuedDrainPolicy`]: a documented two-mode config with no
/// token arithmetic.
///
/// Window-fitted prefix selection is deliberately not shipped. A host that
/// wants it implements [`QueuedDrainPolicy`] itself, using the projections and
/// budget on [`QueuedDrainRequest`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DrainModePolicy {
    mode: DrainMode,
}

impl DrainModePolicy {
    pub const fn new(mode: DrainMode) -> Self {
        Self { mode }
    }

    pub const fn mode(self) -> DrainMode {
        self.mode
    }
}

impl QueuedDrainPolicy for DrainModePolicy {
    fn name(&self) -> &str {
        self.mode.as_str()
    }

    fn select_drain(&self, request: &QueuedDrainRequest<'_>) -> QueuedDrainSelection {
        match self.mode {
            DrainMode::OneAtATime => QueuedDrainSelection::head_only(),
            DrainMode::All => QueuedDrainSelection::everything(request),
        }
    }
}

/// Every shipped mode resolves to a process-wide singleton so that two
/// configurations naming the same mode hold the *same* policy, which is what
/// lets configuration equality compare policies by identity without lying about
/// custom implementations.
pub fn shared_drain_mode_policy(mode: DrainMode) -> Arc<dyn QueuedDrainPolicy> {
    static ONE_AT_A_TIME: std::sync::OnceLock<Arc<dyn QueuedDrainPolicy>> =
        std::sync::OnceLock::new();
    static ALL: std::sync::OnceLock<Arc<dyn QueuedDrainPolicy>> = std::sync::OnceLock::new();
    let slot = match mode {
        DrainMode::OneAtATime => &ONE_AT_A_TIME,
        DrainMode::All => &ALL,
    };
    Arc::clone(slot.get_or_init(|| Arc::new(DrainModePolicy::new(mode))))
}

/// The policy Lash uses when a host configures none: [`DrainMode::OneAtATime`].
///
/// Hosts reach this through
/// `QueuedWorkBatchingConfig::drain_policy`
/// rather than directly, so it stays crate-internal.
pub fn default_queued_drain_policy() -> Arc<dyn QueuedDrainPolicy> {
    shared_drain_mode_policy(DrainMode::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(enqueue_seq: u64) -> QueuedDrainCandidate {
        QueuedDrainCandidate {
            enqueue_seq,
            family: QueuedDrainFamily::QueuedTurnWork,
            merge_key: Some("wake".to_string()),
            authority: QueuedWorkAuthority::default(),
            projected_tokens: 128,
            pending_age_ms: 0,
        }
    }

    #[test]
    fn the_shipped_default_drains_one_row_regardless_of_budget() {
        let candidates = vec![candidate(1), candidate(2), candidate(3)];
        let policy = DrainModePolicy::default();
        assert_eq!(policy.mode(), DrainMode::OneAtATime);
        assert_eq!(policy.name(), "one_at_a_time");
        for available_tokens in [0, 1_000_000] {
            let request = QueuedDrainRequest::new(
                &candidates,
                available_tokens,
                available_tokens,
                64,
                AdmissionBoundary::Idle,
            );
            assert_eq!(
                policy.select_drain(&request),
                QueuedDrainSelection::leading(1)
            );
        }
    }

    #[test]
    fn all_mode_takes_the_whole_offered_prefix() {
        let candidates = vec![candidate(1), candidate(2), candidate(3)];
        let policy = DrainModePolicy::new(DrainMode::All);
        assert_eq!(policy.name(), "all");
        let request = QueuedDrainRequest::new(&candidates, 8, 8, 64, AdmissionBoundary::Idle);
        assert_eq!(
            policy.select_drain(&request),
            QueuedDrainSelection::leading(3)
        );
    }
}
