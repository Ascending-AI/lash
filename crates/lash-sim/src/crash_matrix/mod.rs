//! The crash-point matrix (FIG-3849): the 1.0 durability gate on the
//! in-process Restate server double.
//!
//! Every case is one cell of {seam or obligation kind} × {crash point} ×
//! {seed}. A case builds a [`world::CrashWorld`] — lash-restate's engine on
//! the server double over a SQLite memory store set, with one deployment (a
//! [`lash::LashCore`], its session driver and its recovery interval) — drives
//! the seam's workload, and kills the deployment at the named crash point:
//! the host process dies where it stands, every attempt the engine was
//! running on it is dropped and replayed, and a fresh deployment comes up
//! after a seeded outage. The recovery interval then ticks on the server's
//! virtual clock until the [`invariants`] hold, and the case fails when they
//! never do, or hold only after the ADR 0109 §1.8 detection bound.
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
pub mod deployment;
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
    /// S-14: a process's terminal and its publication to engine waiters.
    ProcessTerminal,
}

impl Seam {
    /// Every seam, in registry order.
    pub const ALL: [Self; 6] = [
        Self::Ingress,
        Self::ControlIntent,
        Self::ScopeClose,
        Self::ParentEnd,
        Self::SessionDelete,
        Self::ProcessTerminal,
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
            Self::ProcessTerminal => "process_terminal",
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
            Self::ProcessTerminal => &["S-14"],
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
            Self::DeliveryRefused => "delivery_refused",
            Self::DeliveryRetryableForever => "delivery_retryable_forever",
        }
    }
}

/// The S8 slices of ADR 0109 §8 that own a cell today's `main` fails.
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

/// The ADR 0109 §1.8 bound a cell's recovery must meet, in sim time from the
/// crash to the first tick at which every invariant holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionBound {
    /// A lost immediate attempt on SQLite across a leader failover (the
    /// killed deployment was the leader): `due_at + T + 20.5 s`.
    LostImmediateSqliteFailover,
    /// A claim the crash left lapsed: `claimed_at + claim_ttl (60 s) + T`.
    LapsedClaim,
    /// Undecodable or refused: stalled in the pass that claims it, one tick.
    StalledInClaimingPass,
    /// Retryable failure: `stalled` after `attempt_ceiling` attempts at the
    /// capped backoff (≈ 1 h 47 min at the defaults) plus one tick per
    /// attempt.
    AttemptCeiling,
}

impl DetectionBound {
    /// The bound, with the tick's +10 % jitter taken at its worst.
    #[must_use]
    pub fn limit(self) -> Duration {
        let tick = TICK + TICK / 10;
        match self {
            Self::LostImmediateSqliteFailover => tick + Duration::from_millis(20_500),
            Self::LapsedClaim => Duration::from_secs(60) + tick,
            Self::StalledInClaimingPass => tick,
            Self::AttemptCeiling => {
                // Attempts 1..=16 at min(2^(n-1) s, 15 min), each plus a tick.
                let backoff: u64 = (1..16_u32)
                    .map(|attempt| (1_u64 << (attempt - 1)).min(900))
                    .sum();
                Duration::from_secs(backoff) + tick * 16
            }
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LostImmediateSqliteFailover => "lost immediate attempt, SQLite failover",
            Self::LapsedClaim => "lapsed claim",
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

const fn blocked(
    finding: Finding,
    then: Option<S8Slice>,
    seam: Seam,
    point: CrashPoint,
    bound: DetectionBound,
    summary: &'static str,
) -> CaseSpec {
    CaseSpec {
        seam,
        point,
        activation: Activation::Finding { finding, then },
        bound,
        summary,
    }
}

const fn s8(
    slice: S8Slice,
    seam: Seam,
    point: CrashPoint,
    bound: DetectionBound,
    summary: &'static str,
) -> CaseSpec {
    CaseSpec {
        seam,
        point,
        activation: Activation::S8(slice),
        bound,
        summary,
    }
}

/// The registry: every cell the matrix runs, today's and S8's. The generated
/// tests in `tests/crash_point_matrix.rs` are checked against it.
pub const MATRIX: &[CaseSpec] = &[
    // --- Ingress (S-1..S-4) ------------------------------------------------
    today(
        Seam::Ingress,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
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
        DetectionBound::LostImmediateSqliteFailover,
        "a drive whose invocation the engine lost before it admitted anything leaves the input to the recovery tick",
    ),
    // --- Control intents (S-6, S-7, S-9) ----------------------------------
    blocked(
        Finding::F1,
        Some(S8Slice::D),
        Seam::ControlIntent,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a session close whose intent committed before the host died has its engine half applied by the recovery tick",
    ),
    blocked(
        Finding::F1,
        Some(S8Slice::D),
        Seam::ControlIntent,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LostImmediateSqliteFailover,
        "a session close whose root release the host died inside is re-applied and acknowledged",
    ),
    blocked(
        Finding::F1,
        Some(S8Slice::D),
        Seam::ControlIntent,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LostImmediateSqliteFailover,
        "a session close whose release ran but whose acknowledgement was lost is acknowledged once",
    ),
    blocked(
        Finding::F1,
        Some(S8Slice::C),
        Seam::ControlIntent,
        CrashPoint::DeliveryRetryableForever,
        DetectionBound::AttemptCeiling,
        "a session close whose engine half fails retryably forever stalls typed at the attempt ceiling instead of retrying every tick",
    ),
    // --- Scope close (S-8) -------------------------------------------------
    today(
        Seam::ScopeClose,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a root that committed its terminal before the deployment died closes its scope and cancels its child",
    ),
    today(
        Seam::ScopeClose,
        CrashPoint::DuringEngineDelivery,
        DetectionBound::LostImmediateSqliteFailover,
        "a scope close the host died inside while delivering the child's cancel is delivered once",
    ),
    today(
        Seam::ScopeClose,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LostImmediateSqliteFailover,
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
    s8(
        S8Slice::P,
        Seam::ParentEnd,
        CrashPoint::DeliveryRefused,
        DetectionBound::StalledInClaimingPass,
        "a page of plans whose child cancels are refused stalls typed and never starves a later plan (head-of-line)",
    ),
    // --- Session delete (S-21) --------------------------------------------
    blocked(
        Finding::F1,
        Some(S8Slice::D),
        Seam::SessionDelete,
        CrashPoint::AfterStateCommit,
        DetectionBound::LostImmediateSqliteFailover,
        "a deletion whose close committed before the host died finishes deleting the session without a caller retry",
    ),
    blocked(
        Finding::F1,
        Some(S8Slice::D),
        Seam::SessionDelete,
        CrashPoint::AfterDeliveryBeforeSettle,
        DetectionBound::LostImmediateSqliteFailover,
        "a deletion whose close was acknowledged before the host died finishes deleting the session without a caller retry",
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
            "{} seed {:#x}: crashed={} detected_after={:?} ticks={} violations={} notes={:?}",
            report.test_name,
            report.seed,
            report.crashed,
            report.detected_after,
            report.ticks,
            report.violations.len(),
            report.notes
        );
    }
    println!(
        "{seam:?} × {point:?}: {} seed(s) in {:?}, {} failed",
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
