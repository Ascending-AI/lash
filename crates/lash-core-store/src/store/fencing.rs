//! One verdict function per fencing decision.
//!
//! Every fencing decision lash makes — "did the head move?", "is this wake
//! delivery still mine?" — has exactly one answer, and that answer lives here
//! (the admission verdicts live in [`admission_plan`](super::admission_plan)). A backend contributes only two things:
//! the locked read that produces the facts, and the write that acts on the
//! verdict. Neither backend decides.
//!
//! # The contract every converted site follows
//!
//! 1. **Lock and read.** PostgreSQL takes `FOR UPDATE` (or the session-keyed
//!    advisory transaction lock); SQLite reads inside its `BEGIN IMMEDIATE`
//!    write transaction, which is a global single-writer lock.
//! 2. **Ask the verdict function.** It is pure, takes `now` as an argument, and
//!    names no driver type. The verdict *is* the decision.
//! 3. **Write, keeping the SQL predicate as a backstop.** The predicate stays
//!    on the statement, but it is no longer consulted for permission: a store
//!    never proceeds to the write without a positive verdict, and never treats
//!    rows-affected as permission. If the write then changes no row, the
//!    locked read and the predicate disagree about the same locked row, which
//!    no concurrent writer can cause. The store fails closed with the **same
//!    domain refusal that site returned before this layer existed** — a lost
//!    lease still reads as a lost lease to its caller — and records the
//!    disagreement as evidence at error level. Never silent, never retried,
//!    never success. That is [`require_fenced_write_applied`].
//!
//! Where a predicate is the *only* guard — the PostgreSQL concurrent-first-commit
//! upsert, `FOR UPDATE SKIP LOCKED` pops, the read side of an admission scan
//! whose predicate is also its `LIMIT` filter, the batch forms, and SQLite's
//! `INDEXED BY` scans — it stays and is named as such at its call site.
//!
//! # Time authority
//!
//! No function here reads a clock. `now_epoch_ms` is always an argument, and
//! the caller names the authority it sampled:
//!
//! * PostgreSQL passes the **database** clock (`transaction_timestamp()`), so
//!   every host writing one database compares expiry against one clock. This is
//!   deliberate (see `crates/lash-postgres-store/src/postgres/support.rs`): a
//!   lease decision must not depend on the wall clock of whichever runtime
//!   happens to execute it.
//! * SQLite passes the **host** `Clock` it was constructed with, because an
//!   embedded database has exactly one host and that host's injected clock is
//!   what the simulator steers.
//!
use super::StoreError;
use crate::SessionId;

/// Tracing target every fencing-verdict diagnostic is emitted under.
///
/// Named once so a test can subscribe to exactly this target and a runbook can
/// filter on it.
pub const FENCING_TRACE_TARGET: &str = "lash_core::fencing";

/// The `event` field of the backstop's disagreement record.
pub const FENCED_WRITE_DISAGREEMENT_EVENT: &str = "fencing.backstop_disagreed_with_verdict";

/// A fenced write whose verdict was already taken in shared code.
///
/// Naming the write is what makes [`StoreError::FencedWriteVerdictDisagreed`]
/// actionable: it says which decision's locked read and which statement's
/// backstop predicate disagreed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FencedWrite {
    /// An admission binding a row to its root (FIG-3927).
    IngressAdmission,
    /// A root's commit settling or releasing a row it admitted (FIG-3927).
    IngressSettlement,
    /// Session-head publication (`D4`).
    SessionHeadPublication,
    /// Wake-delivery settlement out of the enqueuing claim (`D8`).
    WakeDeliverySettlement,
}

impl FencedWrite {
    /// Stable label for diagnostics and tests.
    pub fn label(self) -> &'static str {
        match self {
            Self::IngressAdmission => "ingress.admit",
            Self::IngressSettlement => "ingress.settle",
            Self::SessionHeadPublication => "session_head.publish",
            Self::WakeDeliverySettlement => "wake_delivery.settle",
        }
    }
}

/// The backstop contract, step one: record the disagreement.
///
/// Call this immediately after a conditional write whose fencing predicate the
/// shared verdict already authorized. Exactly one row must change. Any other
/// count means the locked read that fed the verdict and the statement's own
/// predicate disagree about the same locked row, which no concurrent writer
/// can cause — it is a defect in the store.
///
/// Returns `true` when the write applied, and otherwise emits the evidence an
/// operator needs (decision name, backend, row identity, rows affected) at
/// error level before returning `false`. It does **not** decide what the
/// caller receives: see [`require_fenced_write_applied`].
#[must_use]
pub fn fenced_write_applied(
    write: FencedWrite,
    backend: &'static str,
    row_identity: &str,
    rows_affected: u64,
) -> bool {
    if rows_affected == 1 {
        return true;
    }
    tracing::error!(
        target: FENCING_TRACE_TARGET,
        event = FENCED_WRITE_DISAGREEMENT_EVENT,
        fenced_write = write.label(),
        backend,
        row_identity,
        rows_affected,
        outcome = "fenced_write_lost",
        "a fenced write authorized by its shared verdict changed a row count other than one",
    );
    false
}

/// The backstop contract, step two: fail closed with the site's own refusal.
///
/// A disagreement is never silent, never retried and never turned into
/// success. It is also never turned into a *different* answer for the caller:
/// the runtime already knows how to act on a stale fence or a refused
/// settlement (stand down, commit nothing), and routing that through a new generic
/// store error would change behaviour in the most safety-sensitive path in the
/// repository for the sake of diagnosability. So `lost_fence` supplies exactly
/// the domain refusal this site returned before the verdict layer existed, and
/// the disagreement is recorded as evidence beside it.
///
/// Generic over the error type so the process, effect and wake families can
/// call it with `PluginError` or their controller error when they convert.
pub fn require_fenced_write_applied<E>(
    write: FencedWrite,
    backend: &'static str,
    row_identity: &str,
    rows_affected: u64,
    lost_fence: impl FnOnce() -> E,
) -> Result<(), E> {
    if fenced_write_applied(write, backend, row_identity, rows_affected) {
        Ok(())
    } else {
        Err(lost_fence())
    }
}

// ---------------------------------------------------------------------------
// D4 — "did the session head move?"
// ---------------------------------------------------------------------------

/// The one answer to "may this commit publish the session head?" (`D4`).
///
/// The comparison itself already lives in
/// [`RuntimeCommitPlanner::plan`](super::RuntimeCommitPlanner::plan), which
/// refuses a commit whose `expected_head_revision` does not match the revision
/// read under commit authority. What this function adds is the *publication*
/// step's own verdict: the revision the conditional upsert will name must be
/// exactly the revision the locked read produced, and the next revision must be
/// its successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadPublicationVerdict {
    /// Publish `next_head_revision` over `observed_head_revision`.
    Publish,
    /// The head moved between the locked read and the plan. Unreachable while
    /// the single-writer invariant holds; reported rather than retried.
    HeadMoved {
        planned_from_revision: u64,
        observed_head_revision: u64,
    },
}

/// Decide whether the locked head read still authorizes this publication.
pub fn head_publication_verdict(
    planned_from_revision: u64,
    observed_head_revision: u64,
) -> HeadPublicationVerdict {
    if planned_from_revision == observed_head_revision {
        HeadPublicationVerdict::Publish
    } else {
        HeadPublicationVerdict::HeadMoved {
            planned_from_revision,
            observed_head_revision,
        }
    }
}

/// Assert the single-writer invariant a head publication depends on.
///
/// The session head is published by exactly one code path,
/// `commit_runtime_state`, and both backends serialize that path per session
/// before reading the revision they will publish over:
///
/// * **SQLite** — every write runs inside one `BEGIN IMMEDIATE` transaction,
///   which is a database-wide single-writer lock. The head read and the head
///   write are inside the same transaction, so no revision can move between
///   them. That is why SQLite needs no CAS predicate on its head upsert and
///   why one does not exist there.
/// * **PostgreSQL** — the commit takes `pg_advisory_xact_lock` on the session
///   id before any read, then `SELECT head_revision … FOR UPDATE`. The
///   conditional upsert keeps its `WHERE lash_sessions.head_revision = :read`
///   predicate as the backstop for the concurrent *first* commit, where the
///   placeholder row is created inside the same transaction.
///
/// `inside_single_writer_transaction` is the backend's proof. SQLite passes
/// `!connection.is_autocommit()`, which is false exactly when the read has been
/// moved out of the write transaction — the failure this invariant exists to
/// catch.
pub fn require_single_writer_head_publication(
    session_id: &SessionId,
    backend: &'static str,
    inside_single_writer_transaction: bool,
) -> Result<(), StoreError> {
    if inside_single_writer_transaction {
        return Ok(());
    }
    tracing::error!(
        target: "lash_core::fencing",
        event = "fencing.head_publication_outside_single_writer",
        session_id = session_id.as_str(),
        backend,
        outcome = "internal_error",
        "session head publication read its revision outside the backend's single-writer transaction",
    );
    Err(StoreError::UnfencedHeadPublication {
        session_id: SessionId::from(session_id.to_string()),
        backend,
    })
}

// ---------------------------------------------------------------------------
// D7 — "is this wake delivery still in my enqueuing claim?"
// ---------------------------------------------------------------------------

/// The claim columns a locked wake-delivery row carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeDeliveryClaimFacts<'a> {
    /// The row's `state` column, compared against the enqueuing literal.
    pub state: &'a str,
    pub claim_token: Option<&'a str>,
}

/// The one answer to "is this wake delivery still in my enqueuing claim?"
/// (`D8`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeDeliveryClaimVerdict {
    /// The row is enqueuing under this claim token: settle it.
    Held,
    /// No delivery row exists.
    Absent,
    /// The row left the enqueuing state: another worker already settled it.
    NotEnqueuing,
    /// The row is enqueuing under a different claim token.
    Superseded,
}

impl WakeDeliveryClaimVerdict {
    /// Whether the claim still owns the delivery.
    pub fn is_held(self) -> bool {
        matches!(self, Self::Held)
    }

    /// Stable label for diagnostics and tests.
    pub fn label(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Absent => "absent",
            Self::NotEnqueuing => "not_enqueuing",
            Self::Superseded => "superseded",
        }
    }
}

/// Decide whether a wake delivery is still inside the presenter's enqueuing
/// claim (`D8`).
///
/// `enqueuing_state` is the backend's spelling of the enqueuing state literal,
/// passed in rather than hardcoded so the verdict and the SQL backstop read the
/// same vocabulary.
pub fn wake_delivery_claim_verdict(
    observed: Option<WakeDeliveryClaimFacts<'_>>,
    presented_claim_token: &str,
    enqueuing_state: &str,
) -> WakeDeliveryClaimVerdict {
    let Some(observed) = observed else {
        return WakeDeliveryClaimVerdict::Absent;
    };
    if observed.state != enqueuing_state {
        return WakeDeliveryClaimVerdict::NotEnqueuing;
    }
    if observed.claim_token != Some(presented_claim_token) {
        return WakeDeliveryClaimVerdict::Superseded;
    }
    WakeDeliveryClaimVerdict::Held
}
