//! The crash-point matrix (FIG-3849): the 1.0 durability gate, on the
//! in-process Restate server double or a live `restate-server`
//! ([`engine`]; FIG-3872).
//!
//! Every case is one cell of {seam or obligation kind} × {crash point} ×
//! {seed}. A case builds a [`world::CrashWorld`] — lash-restate's engine on
//! the run's [`engine::Engine`] over a SQLite memory store set, with one
//! deployment (a [`lash::LashCore`], its session driver and its recovery
//! interval) — drives the seam's workload, and kills the deployment at the
//! named crash point: the host process dies where it stands, every attempt
//! the engine was running on it is dropped and replayed, and a fresh
//! deployment comes up after a seeded outage. The recovery interval then
//! ticks on the engine's clock until the [`invariants`] hold, and the case
//! fails when they never do, or hold only after the ADR 0109 §1.8 detection
//! bound. A cell is written once against [`engine::Engine`], so every cell
//! runs on both engines, one an S8 slice activates included.
//!
//! # Crash points
//!
//! [`CrashPoint`] names where a seam's two writes can be cut:
//!
//! - [`CrashPoint::AfterStateCommit`]: the producer's SQL commit landed and
//!   the host died before its engine delivery left the process.
//! - [`CrashPoint::DuringEngineDelivery`]: the delivery reached the engine and
//!   the deployment died while the engine ran it.
//! - [`CrashPoint::AfterDeliveryBeforeSettle`]: the engine effect ran and the
//!   write that settles it was lost.
//! - [`CrashPoint::MidJournalStep`]: the deployment died at a seeded journal
//!   step of the invocation that carries the seam.
//! - [`CrashPoint::InvocationLost`]: the engine lost the invocation that
//!   carries the seam (an operator kill), so no replay recovers it.
//! - [`CrashPoint::CallerKilled`]: an operator killed the host job that
//!   started the seam's work, and the kill cascaded into the work's own
//!   invocation, which ended without its terminal.
//! - [`CrashPoint::DeliveryRefused`]: the delivery is refused for good.
//! - [`CrashPoint::DeliveryRetryableForever`]: every delivery fails retryably.
//!
//! The last three are ADR 0109's stall surfaces rather than crashes: they
//! pin §1.8's "undecodable or refused" and "retryable failure" bounds.
//!
//! # How an S8 slice activates its cases
//!
//! [`MATRIX`] is the registry: one [`CaseSpec`] per cell, each with its
//! [`Activation`]. A cell whose end state today's `main` cannot reach is
//! registered `Activation::S8(slice)` and its generated test carries
//! `#[ignore = "FIG-3600 S8-<slice>"]` (`tests/crash_point_matrix.rs`). A
//! cell a defect the matrix found also blocks is registered
//! `Activation::Finding { finding, then }` and ignored as
//! `"FIG-3849 F<n>, then FIG-3600 S8-<slice>"`: the fix of the finding
//! narrows it to its slice. A slice that lands:
//!
//! 1. deletes the `ignore` of each of its cells in the test file and flips
//!    the cell's activation to [`Activation::Today`] here — the registry test
//!    refuses a mismatch between the two;
//! 2. extends [`invariants::ObligationProbe`] with its ledger when it adds
//!    one: [`invariants::obligation_probes`] is the list the checker reads, so
//!    the settled-or-stalled invariant starts reading the slice's
//!    `ObligationLedger` (`count_stalled`, `list_stalled`) the moment the
//!    probe is listed;
//! 3. adds a row for any seam boundary its obligation introduces (a new
//!    [`Seam`] variant or a new crash point on an existing one), with a
//!    scenario in [`cases`].

pub mod cases;
mod catalog_audit;
pub mod deployment;
pub mod engine;
pub mod invariants;
pub mod world;

use std::time::Duration;

/// The seam a case exercises: a store/engine dual write of the prospect seam
/// inventory, named by the ADR 0109 obligation kind that will own it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seam {
    /// S-1–S-4: an accepted input and the drive that consumes it.
    Ingress,
    /// S-6, S-7, S-9: a control intent's engine half (the session close's
    /// release of its roots).
    ControlIntent,
    /// S-8: a root's terminal and the close of its scope.
    ScopeClose,
    /// S-10, S-11: a parent-end plan and its children's cancels.
    ParentEnd,
    /// S-21: a session deletion's cleanup after its close.
    SessionDelete,
    /// A registered process start that must survive a host crash.
    ProcessStart,
    /// A start by definition id (ADR 0113 §3.6): the host's journaled start
    /// replays exactly, holding the definition through its own referrers.
    DefinitionStart,
    DefinitionCreate,
    DefinitionCarry,
    /// S-14: a process's terminal and its publication to engine waiters.
    ProcessTerminal,
    /// A control intent's cancel reaching a running effect-group child: the
    /// child side of the intent, which the child records through journaled
    /// reads of its durable cancel fact (ADR 0105 §4, FIG-3904). It writes
    /// no store row of its own, so no inventory row names it.
    ChildCancel,
}

impl Seam {
    /// Every seam, in registry order.
    pub const ALL: [Self; 11] = [
        Self::Ingress,
        Self::ControlIntent,
        Self::ScopeClose,
        Self::ParentEnd,
        Self::SessionDelete,
        Self::ProcessStart,
        Self::DefinitionStart,
        Self::DefinitionCreate,
        Self::DefinitionCarry,
        Self::ProcessTerminal,
        Self::ChildCancel,
    ];

    /// The ADR 0109 §1.2 obligation-kind label that owns this seam.
    #[must_use]
    pub const fn kind_label(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::ControlIntent => "control_intent",
            Self::ScopeClose => "scope_close",
            Self::ParentEnd => "parent_end",
            Self::SessionDelete => "session_delete",
            Self::ProcessStart => "process_start",
            Self::DefinitionStart => "definition_start",
            Self::DefinitionCreate => "definition_create",
            Self::DefinitionCarry => "definition_carry",
            Self::ProcessTerminal => "process_terminal",
            Self::ChildCancel => "child_cancel",
        }
    }

    /// The prospect seam-inventory rows this seam covers.
    #[must_use]
    pub const fn inventory(self) -> &'static [&'static str] {
        match self {
            Self::Ingress => &["S-1", "S-2", "S-3", "S-4"],
            Self::ControlIntent => &["S-6", "S-7", "S-9"],
            Self::ScopeClose => &["S-8"],
            Self::ParentEnd => &["S-10", "S-11"],
            Self::SessionDelete => &["S-21"],
            Self::ProcessStart => &[],
            Self::DefinitionStart => &[],
            Self::DefinitionCreate => &[],
            Self::DefinitionCarry => &[],
            Self::ProcessTerminal => &["S-14"],
            Self::ChildCancel => &[],
        }
    }
}

/// Where a case cuts its seam. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CrashPoint {
    AfterStateCommit,
    DuringEngineDelivery,
    AfterDeliveryBeforeSettle,
    MidJournalStep,
    InvocationLost,
    CallerKilled,
    DeliveryRefused,
    DeliveryRetryableForever,
}

impl CrashPoint {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AfterStateCommit => "after_state_commit",
            Self::DuringEngineDelivery => "during_engine_delivery",
            Self::AfterDeliveryBeforeSettle => "after_delivery_before_settle",
            Self::MidJournalStep => "mid_journal_step",
            Self::InvocationLost => "invocation_lost",
            Self::CallerKilled => "caller_killed",
            Self::DeliveryRefused => "delivery_refused",
            Self::DeliveryRetryableForever => "delivery_retryable_forever",
        }
    }
}

/// The obligation kinds of ADR 0109 §3 that classify a crash-matrix cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum S8Slice {
    /// Ingress: the outbox arm, parks, `TurnStatus::Stalled`.
    I,
    /// Control intents: ceiling, stall, the child-cancel wedge.
    C,
    /// Parent-end plans: undecodable rows, head-of-line.
    P,
    /// Scope close on the root row.
    S,
    /// Two-phase session delete.
    D,
    /// Process terminal publication.
    T,
}

impl S8Slice {
    /// The slice's name as the `ignore` reasons spell it: `S8-I`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::I => "S8-I",
            Self::C => "S8-C",
            Self::P => "S8-P",
            Self::S => "S8-S",
            Self::D => "S8-D",
            Self::T => "S8-T",
        }
    }

    /// The `#[ignore]` reason of a cell this slice owns.
    #[must_use]
    pub fn ignore_reason(self) -> String {
        format!("FIG-3600 {}", self.name())
    }
}

/// A defect of today's `main` the matrix found that no S8 slice owns. Each
/// is reported with a minimal repro (the cell) and blocks the cells it
/// breaks until it is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Finding {
    /// The facade's resolved session-work port (`ResolvedQueuedWork`,
    /// `crates/lash/src/core/work_drivers.rs`) does not forward
    /// `SessionWorkEngine::control`, so every caller that reaches the engine's
    /// control half through it — a session close's root release, the
    /// recovery tick's parks and intents arms — gets the trait default
    /// `NoEngineControl`: a release answers `NothingHeld` and kills nothing, a
    /// parks pass reads nothing. On Restate a deleted session's running root
    /// keeps running and its intent is acknowledged anyway.
    F1,
}

impl Finding {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::F1 => "FIG-3849 F1",
        }
    }
}

/// Whether a cell runs on today's `main`, or what it waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Today,
    S8(S8Slice),
    /// Blocked by a finding; once it is fixed, by `then` too when set.
    Finding {
        finding: Finding,
        then: Option<S8Slice>,
    },
}

impl Activation {
    /// The `#[ignore]` reason of a cell with this activation; `None` for a
    /// cell that runs today.
    #[must_use]
    pub fn ignore_reason(self) -> Option<String> {
        match self {
            Self::Today => None,
            Self::S8(slice) => Some(slice.ignore_reason()),
            Self::Finding {
                finding,
                then: None,
            } => Some(finding.name().to_owned()),
            Self::Finding {
                finding,
                then: Some(slice),
            } => Some(format!(
                "{}, then {}",
                finding.name(),
                slice.ignore_reason()
            )),
        }
    }
}

/// The recovery interval's tick, `T` of ADR 0109 §1.8: 10 s ± 10 %.
pub const TICK: Duration = Duration::from_secs(10);

/// The ADR 0109 §1.8 bound a cell's recovery must meet.
///
/// §1.8 bounds recovery by the recovery interval's cadence: an obligation
/// becomes the relay's at a point in store time (a claim lapses, a due time
/// passes), and the relay's next pass takes it. The bound is therefore judged
/// in passes, from the first tick at which the store had made the obligation
/// eligible ([`judge`](Self::judge)), never in store time from the crash: a
/// live world's store clock flows with wall time, so a harness that stalls
/// between two ticks (a CPU-starved or memory-stalled host) lands the next
/// tick late, and a store-time bound would charge that stall to the recovery
/// (FIG-4309).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionBound {
    /// A lost immediate attempt on SQLite across a leader failover (the
    /// killed deployment was the leader): `due_at + T + 20.5 s`.
    LostImmediateSqliteFailover,
    /// A claim the crash left lapsed: `claimed_at + claim_ttl (60 s) + T`.
    LapsedClaim,
    /// A lapsed claim whose delivery arms the next obligation of a chain,
    /// which its own bound allows one more tick: a session close whose host
    /// died before the release, then the physical delete its acknowledgement
    /// arms (ADR 0109 §4). `claimed_at + claim_ttl + 2T`.
    LapsedClaimThenArmed,
    /// Undecodable or refused: stalled in the pass that claims it, one tick.
    StalledInClaimingPass,
    /// Retryable failure: `stalled` after `attempt_ceiling` attempts at the
    /// capped backoff (≈ 1 h 47 min at the defaults) plus one tick per
    /// attempt.
    AttemptCeiling,
}

impl DetectionBound {
    /// Store time from the crash after which the obligation is the relay's:
    /// the failover lease of a lost immediate attempt, the claim TTL of a
    /// lapsed claim, the attempts' summed backoff at the ceiling. The dead
    /// deployment wrote its rows by the crash, so each row's own point is at
    /// or before this one.
    #[must_use]
    pub fn eligible_after(self) -> Duration {
        match self {
            Self::LostImmediateSqliteFailover => Duration::from_millis(20_500),
            Self::LapsedClaim | Self::LapsedClaimThenArmed => Duration::from_secs(60),
            Self::StalledInClaimingPass => Duration::ZERO,
            Self::AttemptCeiling => {
                // Attempts 1..=16 at min(2^(n-1) s, 15 min).
                let backoff: u64 = (1..16_u32)
                    .map(|attempt| (1_u64 << (attempt - 1)).min(900))
                    .sum();
                Duration::from_secs(backoff)
            }
        }
    }

    /// The recovery passes the bound allows, counting the first tick at or
    /// after [`eligible_after`](Self::eligible_after): the pass that takes
    /// the obligation, one more for the obligation a chain arms, one per
    /// attempt at the ceiling.
    #[must_use]
    pub const fn passes(self) -> usize {
        match self {
            Self::LostImmediateSqliteFailover | Self::LapsedClaim | Self::StalledInClaimingPass => {
                1
            }
            Self::LapsedClaimThenArmed => 2,
            Self::AttemptCeiling => 16,
        }
    }

    /// The bound in store time from the crash when every tick lands one `T`
    /// after the last, with the tick's +10 % jitter taken at its worst: how
    /// many ticks a recovery is given, and what a cell that must outlast a
    /// shorter bound compares against.
    #[must_use]
    pub fn limit(self) -> Duration {
        let tick = TICK + TICK / 10;
        let passes = u32::try_from(self.passes()).unwrap_or(u32::MAX);
        self.eligible_after() + tick * passes
    }

    /// Judge a recovery by passes. `ticks` holds each tick's store time after
    /// the crash, in order; `recovered_at` is the tick after which every
    /// invariant first held (0: before any tick). The recovery meets the
    /// bound when it held by the [`passes`](Self::passes)-th tick counting
    /// from the first at or after [`eligible_after`](Self::eligible_after);
    /// one that held before the obligation was eligible meets it too.
    ///
    /// # Errors
    ///
    /// The violation, naming the eligible tick and the tick that recovered.
    pub fn judge(self, ticks: &[Duration], recovered_at: usize) -> Result<(), String> {
        let eligible_after = self.eligible_after();
        let Some(eligible_tick) = ticks
            .iter()
            .position(|at| *at >= eligible_after)
            .map(|index| index + 1)
        else {
            return Ok(());
        };
        let last_allowed = eligible_tick + self.passes() - 1;
        if recovered_at <= last_allowed {
            return Ok(());
        }
        Err(format!(
            "recovered after tick {recovered_at} ({:?} of sim time), past the §1.8 bound ({}): \
             the obligation was eligible at tick {eligible_tick} ({:?}, {eligible_after:?} after \
             the crash) and the bound allows {} pass(es) from there, through tick {last_allowed}",
            ticks[recovered_at - 1],
            self.label(),
            ticks[eligible_tick - 1],
            self.passes(),
        ))
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LostImmediateSqliteFailover => "lost immediate attempt, SQLite failover",
            Self::LapsedClaim => "lapsed claim",
            Self::LapsedClaimThenArmed => "lapsed claim, then the obligation it arms",
            Self::StalledInClaimingPass => "stalled in the claiming pass",
            Self::AttemptCeiling => "attempt ceiling",
        }
    }
}

/// One registered cell of the matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaseSpec {
    pub seam: Seam,
    pub point: CrashPoint,
    pub activation: Activation,
    pub bound: DetectionBound,
    /// What the cell proves, in one line.
    pub summary: &'static str,
}

impl CaseSpec {
    /// The generated test's name: `<kind>_<point>`.
    #[must_use]
    pub fn test_name(&self) -> String {
        format!("{}_{}", self.seam.kind_label(), self.point.label())
    }
}

const fn today(
    seam: Seam,
    point: CrashPoint,
    bound: DetectionBound,
    summary: &'static str,
) -> CaseSpec {
    CaseSpec {
        seam,
        point,
        activation: Activation::Today,
        bound,
        summary,
    }
}

/// The registry: every cell the matrix runs, today's and S8's. The generated
/// tests in `tests/crash_point_matrix.rs` are checked against it.
pub const MATRIX: &[CaseSpec] = &[
    // --- Ingress (S-1..S-4) ------------------------------------------------
    // An input's ingress obligation is delivered by the drive's claim of the
    // row, not by the engine accepting the ask (ADR 0109 §3): the relay's
    // claim covers the ask and the admission after it. A host that dies at
    // the ask, or an engine that loses the drive before it admits the row,
    // leaves that claim to lapse, and the relay asks again under the next
    // attempt: those cells are bounded by the lapsed claim.
    today(
        Seam::Ingress,
        CrashPoint::AfterStateCommit,
        DetectionBound::LapsedClaim,
        "an accepted input whose drive ask never left the dead host is driven once by the recovery tick",
    ),
    today(
        Seam::Ingress,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LostImmediateSqliteFailover,
        "a drive the deployment died inside is replayed and consumes the input once",
    ),
    today(
        Seam::Ingress,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LostImmediateSqliteFailover,
        "an admission whose claim committed but whose journal result was lost claims the input once",
    ),
    today(
        Seam::Ingress,
        CrashPoint::MidJournalStep,
        DetectionBound::LostImmediateSqliteFailover,
        "a root the deployment died inside at a seeded journal step commits its answer once",
    ),
    today(
        Seam::Ingress,
        CrashPoint::InvocationLost,
        DetectionBound::LapsedClaim,
        "a drive whose invocation the engine lost before it admitted anything leaves the input to the recovery tick",
    ),
    // --- Control intents (S-6, S-7, S-9) ----------------------------------
    // A closing session's in-flight input claim is not excluded: an input
    // claimed before the close does not finish. The close ends its root
    // (`Cancelled`, `SessionDeleted`) and the only close there is, a delete,
    // retires the claim with the session's storage through the
    // `SessionDelete` obligation (ADR 0109 §4). The ingress invariant reads
    // the claim until then, so a delete that never finishes fails the cell.
    // The close's own attempt claims its `ControlIntent` obligation before
    // the engine half runs, so a host that dies at or after the release
    // leaves the claim to lapse: those cells are bounded by the lapsed claim.
    // A host that died before the release leaves the close to the retaken
    // claim, and the delete its acknowledgement arms is delivered by the
    // tick after it.
    today(
        Seam::ControlIntent,
        CrashPoint::AfterStateCommit,
        DetectionBound::LapsedClaimThenArmed,
        "a session close whose intent committed before the host died has its engine half applied by the recovery tick",
    ),
    today(
        Seam::ControlIntent,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LapsedClaim,
        "a session close whose root release the host died inside is re-applied and acknowledged",
    ),
    // The acknowledgement the host died before is re-applied once the
    // close's claim lapses, so the end state is a lapsed claim's.
    today(
        Seam::ControlIntent,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LapsedClaim,
        "a session close whose release ran but whose acknowledgement was lost is acknowledged once",
    ),
    today(
        Seam::ControlIntent,
        CrashPoint::DeliveryRetryableForever,
        DetectionBound::AttemptCeiling,
        "a session close whose engine half fails retryably forever stalls typed at the attempt ceiling instead of retrying every tick",
    ),
    // --- Scope close (S-8) -------------------------------------------------
    // The after-commit cut normally leaves a due row, but the live SDK can
    // start the local close attempt while its BeforeRun frame is being cut.
    // If that attempt claims before the host dies, the cell reads its row and
    // uses §1.8's lapsed-claim bound; a due row keeps the shorter bound. The
    // Invocation loss has the same race: the immediate attempt may claim
    // before the invocation is killed. Both cells inspect the row at restart.
    today(
        Seam::ScopeClose,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a root that committed its terminal before the deployment died closes its scope and cancels its child",
    ),
    today(
        Seam::ScopeClose,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LapsedClaim,
        "a scope close the host died inside while delivering the child's cancel is delivered once",
    ),
    today(
        Seam::ScopeClose,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LapsedClaim,
        "a scope close whose child cancel was delivered but whose plan never settled settles",
    ),
    today(
        Seam::ScopeClose,
        CrashPoint::MidJournalStep,
        DetectionBound::LostImmediateSqliteFailover,
        "a scope close that ran but whose journal result was lost closes once",
    ),
    today(
        Seam::ScopeClose,
        CrashPoint::InvocationLost,
        DetectionBound::LostImmediateSqliteFailover,
        "a root whose invocation the engine lost after its terminal commit has its scope closed by the recovery tick",
    ),
    // --- Parent-end plans (S-10, S-11) ------------------------------------
    today(
        Seam::ParentEnd,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a recorded plan the host died before applying is applied by the recovery tick",
    ),
    today(
        Seam::ParentEnd,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LostImmediateSqliteFailover,
        "a plan whose child cancel the host died inside is applied once",
    ),
    today(
        Seam::ParentEnd,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LostImmediateSqliteFailover,
        "a plan whose cancels were delivered but which never settled settles",
    ),
    today(
        Seam::ParentEnd,
        CrashPoint::DeliveryRefused,
        DetectionBound::StalledInClaimingPass,
        "a page of plans whose child cancels are refused stalls typed and never starves a later plan (head-of-line)",
    ),
    // --- Session delete (S-21) --------------------------------------------
    today(
        Seam::SessionDelete,
        CrashPoint::AfterStateCommit,
        DetectionBound::LapsedClaimThenArmed,
        "a deletion whose close committed before the host died finishes deleting the session without a caller retry",
    ),
    // The host died inside a delivery of the delete obligation, which holds
    // its claim: the relay retakes it once the claim lapses.
    today(
        Seam::SessionDelete,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LapsedClaim,
        "a deletion whose close was acknowledged before the host died finishes deleting the session without a caller retry",
    ),
    // --- Process start -----------------------------------------------------
    today(
        Seam::ProcessStart,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a process committed immediately before its host died starts through its registered obligation",
    ),
    // --- Start by definition id (ADR 0113 §3.6) ---------------------------
    // Both cuts are in the host's journaled start. On the double the host's
    // handler replays on the next deployment; on a live server the host's
    // job dies with it, and the start's own obligation starts the process.
    // A cut before the registration's result leaves that obligation due; a
    // cut after it leaves the claim the dead attempt took, which lapses.
    today(
        Seam::DefinitionStart,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LapsedClaim,
        "a start by id cut before its registration step was stored admits at most one process, and a dead start that admitted none holds nothing once the host's pin is gone",
    ),
    today(
        Seam::DefinitionStart,
        CrashPoint::MidJournalStep,
        DetectionBound::LostImmediateSqliteFailover,
        "a start by id whose registration committed before its result was journaled starts one process, which holds the definition",
    ),
    today(
        Seam::DefinitionStart,
        CrashPoint::AfterStateCommit,
        DetectionBound::LapsedClaim,
        "a start by id whose registration result was journaled never registers again, and its one process starts and holds the definition",
    ),
    today(
        Seam::DefinitionCreate,
        CrashPoint::MidJournalStep,
        DetectionBound::LostImmediateSqliteFailover,
        "a create attempt lost before its journal commit publishes one definition on replay",
    ),
    today(
        Seam::DefinitionCreate,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a committed create attempt publishes its descriptor and closure before exposing the ID",
    ),
    today(
        Seam::DefinitionCreate,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LostImmediateSqliteFailover,
        "a publication lost before frame commit replays the same definition and retains its closure",
    ),
    today(
        Seam::DefinitionCarry,
        CrashPoint::MidJournalStep,
        DetectionBound::LapsedClaim,
        "a frame prepared before its SQL activation replays the complete carry and reclaims every edge after an uncarried switch",
    ),
    today(
        Seam::DefinitionCarry,
        CrashPoint::AfterStateCommit,
        DetectionBound::LapsedClaim,
        "a committed frame retains its complete engine share across a deployment crash",
    ),
    // --- Process terminal (S-14) ------------------------------------------
    today(
        Seam::ProcessTerminal,
        CrashPoint::MidJournalStep,
        DetectionBound::LostImmediateSqliteFailover,
        "a process workflow the deployment died inside publishes its terminal to its waiters once",
    ),
    today(
        Seam::ProcessTerminal,
        CrashPoint::InvocationLost,
        DetectionBound::LostImmediateSqliteFailover,
        "a process whose workflow invocation was lost after its terminal commit still publishes the terminal to an engine waiter",
    ),
    today(
        Seam::ProcessTerminal,
        CrashPoint::CallerKilled,
        DetectionBound::LostImmediateSqliteFailover,
        "a started process whose host job an operator killed, the kill cascading into its run, ends substrate-lost and answers its engine waiter",
    ),
    // --- Child cancel (ADR 0105 §4) ---------------------------------------
    // A root is cancelled while its effect-group tool child runs an attempt
    // that ignores its token; the child's replay is immediate, so both cells
    // answer to the immediate bound.
    today(
        Seam::ChildCancel,
        CrashPoint::MidJournalStep,
        DetectionBound::LostImmediateSqliteFailover,
        "a tool child the deployment died inside after its cancel ended its attempt replays its journal, never re-runs the tool, and its root ends cancelled once",
    ),
    today(
        Seam::ChildCancel,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LostImmediateSqliteFailover,
        "a tool child the deployment died inside as its cancelled attempt's outcome reached the engine re-runs that unrecorded attempt once, which its cancel ends again, and its root ends cancelled once",
    ),
];

/// The registry cell for `seam` × `point`.
#[must_use]
pub fn case(seam: Seam, point: CrashPoint) -> Option<&'static CaseSpec> {
    MATRIX
        .iter()
        .find(|spec| spec.seam == seam && spec.point == point)
}

/// How many seeds each cell runs: `LASH_CRASH_MATRIX_SEEDS`, else 3 (a
/// quarter under `LASH_QUICK`, at least one).
#[must_use]
pub fn seed_count() -> usize {
    std::env::var("LASH_CRASH_MATRIX_SEEDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or_else(|| crate::quick_seed_sweep(3))
}

/// The seeds of `spec`'s cell: a stable function of the cell and the index,
/// so one cell's seed `i` is the same on every run and differs across cells.
#[must_use]
pub fn seeds(spec: &CaseSpec) -> Vec<u64> {
    (0..seed_count() as u64)
        .map(|index| {
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for byte in spec.test_name().bytes().chain(index.to_le_bytes()) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
            hash
        })
        .collect()
}

/// What one seed of one cell observed.
#[derive(Clone, Debug)]
pub struct CaseReport {
    pub seed: u64,
    pub test_name: String,
    /// Whether the crash point was reached; a cell whose point never fired
    /// proves nothing and is a violation.
    pub crashed: bool,
    /// Sim time from the crash to the first tick at which every invariant
    /// held, when they did.
    pub detected_after: Option<Duration>,
    pub violations: Vec<String>,
    /// What the case drew (workload shape, crash placement).
    pub notes: Vec<String>,
    /// The ticks the recovery interval ran.
    pub ticks: usize,
    /// Each recovery tick's sim time after the crash, in order: one `T`
    /// apart on the interval's cadence, further where the harness stalled
    /// between two ticks.
    pub tick_times: Vec<Duration>,
}

impl CaseReport {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Run every seed of the cell `seam` × `point` and return one report per
/// seed. `Err` when the cell is not registered.
pub async fn run_cell(seam: Seam, point: CrashPoint) -> Result<Vec<CaseReport>, String> {
    let spec =
        case(seam, point).ok_or_else(|| format!("no registered cell for {seam:?} × {point:?}"))?;
    let mut reports = Vec::new();
    for seed in seeds(spec) {
        reports.push(Box::pin(cases::run(spec, seed)).await);
    }
    Ok(reports)
}

/// Run the cell and fail with every violating seed's report.
pub async fn assert_cell(seam: Seam, point: CrashPoint) {
    let started = std::time::Instant::now();
    let reports = match Box::pin(run_cell(seam, point)).await {
        Ok(reports) => reports,
        Err(error) => panic!("{error}"),
    };
    let failed: Vec<&CaseReport> = reports.iter().filter(|report| !report.passed()).collect();
    for report in &reports {
        println!(
            "{} seed {:#x}: crashed={} detected_after={:?} ticks={} tick_times={:?} violations={} notes={:?}",
            report.test_name,
            report.seed,
            report.crashed,
            report.detected_after,
            report.ticks,
            report.tick_times,
            report.violations.len(),
            report.notes
        );
    }
    let engine = match engine::EngineKind::from_env() {
        Ok(engine::EngineKind::Double) => "double",
        Ok(engine::EngineKind::Live(_)) => "live",
        Err(_) => "unconfigured",
    };
    println!(
        "{seam:?} × {point:?} on the {engine} engine: {} seed(s) in {:?}, {} failed",
        reports.len(),
        started.elapsed(),
        failed.len()
    );
    assert!(
        failed.is_empty(),
        "crash-matrix cell {seam:?} × {point:?} failed:\n{:#?}",
        failed
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(ticks: &[u64]) -> Vec<Duration> {
        ticks.iter().copied().map(Duration::from_secs).collect()
    }

    /// FIG-4161's session-delete seed on a stalled host: three ticks 45 s
    /// apart, the claim lapsed by the second, and the delete its close armed
    /// taken by the third. The obligation's two passes ran where §1.8 puts
    /// them; the harness's stall between the ticks is not the recovery's.
    #[test]
    fn a_harness_stall_between_ticks_is_not_charged_to_the_recovery() {
        let ticks = seconds(&[45, 88, 131]);
        assert!(Duration::from_secs(131) > DetectionBound::LapsedClaimThenArmed.limit());
        assert_eq!(
            DetectionBound::LapsedClaimThenArmed.judge(&ticks, 3),
            Ok(())
        );
        let ticks = seconds(&[25, 50, 76]);
        assert!(Duration::from_secs(76) > DetectionBound::LapsedClaim.limit());
        assert_eq!(DetectionBound::LapsedClaim.judge(&ticks, 3), Ok(()));
    }

    /// A recovery that needed a pass more than its bound allows fails, on the
    /// interval's own cadence or a stalled one.
    #[test]
    fn a_recovery_a_pass_late_fails_on_any_cadence() {
        let cadence = seconds(&[10, 20, 30, 40, 50, 60, 70]);
        let late = DetectionBound::LapsedClaim.judge(&cadence, 7);
        assert!(
            late.as_ref()
                .is_err_and(|violation| violation.contains("eligible at tick 6")),
            "{late:?}"
        );
        assert_eq!(DetectionBound::LapsedClaim.judge(&cadence, 6), Ok(()));
        let stalled = seconds(&[45, 88, 131, 175]);
        assert!(
            DetectionBound::LapsedClaimThenArmed
                .judge(&stalled, 4)
                .is_err()
        );
        assert!(DetectionBound::LapsedClaim.judge(&stalled, 3).is_err());
    }

    /// A tick that lands just before the claim lapses is not the eligible
    /// one: the next is, and a recovery there meets the bound.
    #[test]
    fn the_eligible_tick_is_the_first_at_or_after_the_lapse() {
        let ticks = seconds(&[10, 20, 30, 40, 50, 59, 69]);
        assert_eq!(DetectionBound::LapsedClaim.judge(&ticks, 7), Ok(()));
        assert_eq!(DetectionBound::LapsedClaim.judge(&ticks, 0), Ok(()));
        assert_eq!(
            DetectionBound::StalledInClaimingPass.judge(&ticks, 1),
            Ok(())
        );
        assert!(
            DetectionBound::StalledInClaimingPass
                .judge(&ticks, 2)
                .is_err()
        );
    }

    /// The store-time limit is the pass bound on an unstalled cadence, so a
    /// cell's tick budget and the scope-close cells' comparisons are
    /// unchanged.
    #[test]
    fn the_limit_is_the_pass_bound_on_the_intervals_cadence() {
        let tick = TICK + TICK / 10;
        assert_eq!(
            DetectionBound::LostImmediateSqliteFailover.limit(),
            tick + Duration::from_millis(20_500)
        );
        assert_eq!(
            DetectionBound::LapsedClaim.limit(),
            Duration::from_secs(60) + tick
        );
        assert_eq!(
            DetectionBound::LapsedClaimThenArmed.limit(),
            Duration::from_secs(60) + tick * 2
        );
        assert_eq!(DetectionBound::StalledInClaimingPass.limit(), tick);
    }
}
