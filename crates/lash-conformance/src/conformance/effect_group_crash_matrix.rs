//! The durable effect-group crash matrix: ADR 0099's crash windows crossed
//! with the three ways a substrate can come back (FIG-3429).
//!
//! The turn crash matrix ([`super::turn_crash_matrix`]) derives its windows
//! from a scripted turn's seam trace, and a group child is invisible to that
//! trace: children dispatch, claim, and settle inside the group's own
//! machinery, not through the effect calls the turn seam wraps. What the seam
//! can name is the *vocabulary* — `EffectOperation` there carries `GroupChild`,
//! `GroupOpen`, `GroupSettle`, and `GroupClose` for any trace that drives
//! groups through it — but it cannot park inside a child's execution. This
//! suite therefore expresses ADR 0099's windows in the group's own terms: a
//! window is the observable residue a crashed process leaves, and a redrive is
//! who comes back for that residue.
//!
//! A window is built the way [`super::effect_group_drain`] builds crashes: a
//! host of its own on a runtime of its own, killed by dropping the runtime,
//! with the residue verified before death where the residue needs a live
//! claimant to be meaningful. Three redrives, and each is a different
//! authority fact:
//!
//! - [`GroupRedrive::SameOpener`] — the same durable scope re-presents the
//!   accepted children. The ruling is *resume*: journaled ranks replay without
//!   executing, unsettled children execute exactly once under the same
//!   opener's runners, and the queue completes.
//! - [`GroupRedrive::DifferentOpenerSameKeys`] — a different process
//!   re-presents the retained replay keys under different requests. The ruling
//!   is *refusal without loss*: an executor bound to the offered request never
//!   runs a retained child, the resolver is asked about the *retained*
//!   children (a key-only match would ask nothing), journaled ranks still
//!   replay, and an honest host still finishes the queue.
//! - [`GroupRedrive::NoOpener`] — nobody comes back for the group at all. The
//!   ruling is *the queue outlives the caller*: a host that cannot run the
//!   children still reports them truthfully (settled ranks `Settled`,
//!   unsettled `NoExecutor`, never a fabricated complete), and a later wired
//!   host settles each unsettled child exactly once.
//!
//! [`ADR_0099_ROWS`] is the reviewed table: every ADR 0099 crash-window row is
//! either covered by a window this suite drives, or excluded with the reason
//! the substrate cannot produce the state — so a future reader can see which
//! rulings remain unexercised rather than mistaking absence for coverage.
//! That is the same contract the turn matrix's outcome table gives: the
//! inventory is complete even where the machinery is not.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::effect_group_drain::{
    CRASH_LEASE_MS, DrainWorld, DrainWorldFactory, RUN, RecordingExecutors, blocking,
    child_replay_key, close, crashed_process, drain_until_no_live_lease, group_key, impostor,
    impostor_group, never, next, open, open_with, orphan_two_losers, outcome_of, pass, scope,
    settles, spec, until, until_leases_lapse, unwired_spec,
};
use super::*;
use lash_core::testing::conformance_support::ChildDrainOutcome;

/// How long a redrive waits for a settlement that the ruling says cannot
/// arrive. Long enough that a contested child could not have produced one even
/// if the refusal leaked, short enough that twelve cells stay inside the law's
/// budget.
const REFUSAL_WINDOW: Duration = Duration::from_millis(2 * CRASH_LEASE_MS);

/// The crashed residue a window leaves, in the terms a redrive can observe.
///
/// Two positions per group: position 0 settles where the window needs a
/// journaled rank, position 1 is always unsettled so every window has a child
/// the redrive's authority question is about. [`Self::settled`] and
/// [`Self::unsettled`] name that residue — the reviewed ruling each redrive
/// asserts — rather than repeating the positions at every site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupCrashWindow {
    /// The open is journaled; whether the children were ever dispatched is not
    /// observable from outside, so the ruling is on the durable fact alone:
    /// membership retained, both children unsettled (ADR 0099 W1).
    OpenJournaledBeforeDispatch,
    /// Both children hold claims and are inside their executors; nothing is
    /// journaled beyond the open (ADR 0099 W3).
    ChildrenInFlightBeforeSettlement,
    /// Position 0's terminal is journaled — replayable by any later opener —
    /// while position 1 is still in flight under a claim that dies with the
    /// process (ADR 0099 W4 and W5, which share this residue: *consumption* is
    /// a cursor on the caller's handle, not a journaled fact, so "settled" and
    /// "settled and consumed" are the same durable state).
    ChildSettledBeforeConsume,
    /// The caller closed with `RunToCompletion` while its losers were still
    /// claimed and running: the group is closed, the losers are orphaned
    /// (ADR 0099 W9).
    ClosingRecordedLosersInFlight,
}

impl GroupCrashWindow {
    const ALL: [Self; 4] = [
        Self::OpenJournaledBeforeDispatch,
        Self::ChildrenInFlightBeforeSettlement,
        Self::ChildSettledBeforeConsume,
        Self::ClosingRecordedLosersInFlight,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::OpenJournaledBeforeDispatch => "open-journaled",
            Self::ChildrenInFlightBeforeSettlement => "children-in-flight",
            Self::ChildSettledBeforeConsume => "child-settled",
            Self::ClosingRecordedLosersInFlight => "closing-recorded",
        }
    }

    /// Positions whose terminals are journaled when the process dies.
    fn settled(self) -> &'static [usize] {
        match self {
            Self::ChildSettledBeforeConsume => &[0],
            _ => &[],
        }
    }

    /// Positions a redrive may still have to run.
    fn unsettled(self) -> &'static [usize] {
        match self {
            Self::ChildSettledBeforeConsume => &[1],
            _ => &[0, 1],
        }
    }
}

/// Who comes back for the crashed residue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupRedrive {
    /// The same durable scope reopens with the accepted children.
    SameOpener,
    /// A different process reopens with the retained replay keys under
    /// different requests — the authority substitution FIG-3429 was written to
    /// name, replayed against each crashed state rather than only the healthy
    /// one.
    DifferentOpenerSameKeys,
    /// No opener returns; the drain is the only way the group can finish.
    NoOpener,
}

impl GroupRedrive {
    const ALL: [Self; 3] = [
        Self::SameOpener,
        Self::DifferentOpenerSameKeys,
        Self::NoOpener,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::SameOpener => "same-opener",
            Self::DifferentOpenerSameKeys => "different-opener",
            Self::NoOpener => "no-opener",
        }
    }
}

/// The reviewed ruling for one ADR 0099 crash-window row.
enum Adr0099Ruling {
    /// The matrix drives this window's crash state; the value is which
    /// [`GroupCrashWindow`] realizes it.
    Covered(GroupCrashWindow),
    /// The ruling exists on paper but the substrate has no seam that can
    /// produce the crash state — the string says which machinery is missing,
    /// so the exclusion is a fact a reader can check rather than a gap.
    NoSeam(&'static str),
    /// ADR 0099 promises nothing at this window, so there is no oracle to
    /// write.
    NothingPromised(&'static str),
}

/// One row of the reviewed table: the ADR's window number and summary, and
/// what this suite does with it.
struct Adr0099Row {
    row: &'static str,
    summary: &'static str,
    ruling: Adr0099Ruling,
}

/// Every ADR 0099 §"crash windows" row, classified.
///
/// Rows whose crash state is a gap between two writes inside the driver — or
/// between a write and machinery that does not exist on this substrate — are
/// `NoSeam`: there is no externally observable difference between "before" and
/// "after" a step the substrate performs atomically or never performs.
const ADR_0099_ROWS: &[Adr0099Row] = &[
    Adr0099Row {
        row: "W1",
        summary: "group open journaled before child dispatch",
        ruling: Adr0099Ruling::Covered(GroupCrashWindow::OpenJournaledBeforeDispatch),
    },
    Adr0099Row {
        row: "W2",
        summary: "child dispatched before its invocation id is published",
        ruling: Adr0099Ruling::NoSeam(
            "a child's claim and its published identity are one fused write in \
             this substrate — dispatch spawns a task whose first durable act is \
             the claim, so there is no gap to park a crash inside",
        ),
    },
    Adr0099Row {
        row: "W3",
        summary: "child in flight before settlement, opener live",
        ruling: Adr0099Ruling::Covered(GroupCrashWindow::ChildrenInFlightBeforeSettlement),
    },
    Adr0099Row {
        row: "W4",
        summary: "child settled and ranked before the opener consumes it",
        ruling: Adr0099Ruling::Covered(GroupCrashWindow::ChildSettledBeforeConsume),
    },
    Adr0099Row {
        row: "W5",
        summary: "winner consumed while losers remain in flight",
        ruling: Adr0099Ruling::Covered(GroupCrashWindow::ChildSettledBeforeConsume),
    },
    Adr0099Row {
        row: "W6",
        summary: "final record written, completion notification lost",
        ruling: Adr0099Ruling::NoSeam(
            "protected final-attempt records do not exist on this substrate",
        ),
    },
    Adr0099Row {
        row: "W7",
        summary: "notification sent, intent drain not run",
        ruling: Adr0099Ruling::NoSeam(
            "the intent-drain machinery does not exist on this substrate",
        ),
    },
    Adr0099Row {
        row: "W8",
        summary: "intent drain run, incorporation not realized",
        ruling: Adr0099Ruling::NoSeam(
            "the intent-realization protocol does not exist on this substrate",
        ),
    },
    Adr0099Row {
        row: "W9",
        summary: "close recorded before cancel is issued to losers",
        ruling: Adr0099Ruling::Covered(GroupCrashWindow::ClosingRecordedLosersInFlight),
    },
    Adr0099Row {
        row: "W10",
        summary: "all drains complete before terminal accounting commits",
        ruling: Adr0099Ruling::NoSeam(
            "the finalization-commit steps do not exist on this substrate",
        ),
    },
    Adr0099Row {
        row: "W11",
        summary: "terminal accounting committed before parent retirement",
        ruling: Adr0099Ruling::NoSeam(
            "the finalization-commit steps do not exist on this substrate",
        ),
    },
    Adr0099Row {
        row: "W12",
        summary: "close deadline expires while the group is draining",
        ruling: Adr0099Ruling::NoSeam("close-deadline expiry does not exist on this substrate"),
    },
    Adr0099Row {
        row: "W13",
        summary: "segment handover mid-group",
        ruling: Adr0099Ruling::NoSeam("segment handover does not exist on this substrate"),
    },
    Adr0099Row {
        row: "W14",
        summary: "child handler dies with the opener alive",
        ruling: Adr0099Ruling::NoSeam(
            "a child handler runs inside the driver's own task; the seam can \
             kill the process, not one task, and an executor failure is a \
             journaled terminal rather than a death",
        ),
    },
    Adr0099Row {
        row: "W15",
        summary: "attach retention expires mid-group",
        ruling: Adr0099Ruling::NoSeam("attach retention does not exist on this substrate"),
    },
    Adr0099Row {
        row: "W16",
        summary: "session deleted while the group is accepted or closing",
        ruling: Adr0099Ruling::NoSeam(
            "the session-delete exclusion is FIG-3396's and does not exist on \
             this substrate",
        ),
    },
    Adr0099Row {
        row: "W17",
        summary: "late completion arrives after a cancel decision",
        ruling: Adr0099Ruling::NoSeam(
            "turn-cancel closure for group children does not exist on this \
             substrate",
        ),
    },
    Adr0099Row {
        row: "W18",
        summary: "native process death mid-group",
        ruling: Adr0099Ruling::NothingPromised(
            "the native tier promises nothing on process death; the row is a \
             documented non-goal, not an oracle",
        ),
    },
    Adr0099Row {
        row: "W19",
        summary: "final record committed and ordered, crash before drain",
        ruling: Adr0099Ruling::NoSeam(
            "commit-order drain positions do not exist on this substrate",
        ),
    },
    Adr0099Row {
        row: "W20",
        summary: "two final commits race for one group",
        ruling: Adr0099Ruling::NoSeam(
            "the concurrent-final linearization does not exist on this \
             substrate",
        ),
    },
];

/// The reviewed-table invariant, run inside the law and as a unit test: every
/// ADR 0099 row is classified, every covered window is driven by the matrix,
/// and every window in the matrix is owed by at least one row — so the table
/// and the code cannot drift apart silently.
fn assert_adr_0099_table_is_reviewed() {
    assert_eq!(
        ADR_0099_ROWS.len(),
        20,
        "ADR 0099 names twenty crash windows; the table must account for all \
         of them"
    );
    for (index, row) in ADR_0099_ROWS.iter().enumerate() {
        assert_eq!(
            row.row,
            format!("W{}", index + 1),
            "rows are numbered in ADR order so a gap is visible"
        );
        assert!(
            !row.summary.trim().is_empty(),
            "row {} must carry the ADR's summary so the table reads against \
             the document it reviews",
            row.row
        );
        match &row.ruling {
            Adr0099Ruling::Covered(window) => assert!(
                GroupCrashWindow::ALL.contains(window),
                "row {} covers a window the matrix does not drive",
                row.row
            ),
            Adr0099Ruling::NoSeam(reason) | Adr0099Ruling::NothingPromised(reason) => assert!(
                !reason.trim().is_empty(),
                "an excluded row must say which machinery is missing or why no \
                 oracle applies — a bare exclusion is a gap, not a ruling \
                 (row {})",
                row.row
            ),
        }
    }
    for window in GroupCrashWindow::ALL {
        assert!(
            ADR_0099_ROWS.iter().any(
                |row| matches!(&row.ruling, Adr0099Ruling::Covered(covered) if *covered == window)
            ),
            "window {window:?} is driven by the matrix but no ADR 0099 row \
             claims it — either the mapping is wrong or the window is \
             unreviewed"
        );
    }
}

/// The durable effect-group crash matrix (FIG-3429): every realizable ADR 0099
/// crash window crossed with every redrive the substrate admits.
///
/// Run against each SQL tier by `store_effect_group_drain_tests!`, which owns
/// the world factory this suite drives: the crash, the lease, and the drain
/// are the drain fixture's, the windows and rulings are this file's.
pub async fn store_effect_group_crash_matrix_conformance(make: DrainWorldFactory) {
    assert_adr_0099_table_is_reviewed();
    let prefix = format!("group-crash-{}", uuid::Uuid::new_v4().simple());
    for window in GroupCrashWindow::ALL {
        for redrive in GroupRedrive::ALL {
            run_cell(&make, &prefix, window, redrive).await;
        }
    }
}

/// One cell: crash at the window, let the dead process's leases lapse, then
/// apply the redrive's ruling.
async fn run_cell(
    make: &DrainWorldFactory,
    prefix: &str,
    window: GroupCrashWindow,
    redrive: GroupRedrive,
) {
    let label = format!("{}-{}", window.label(), redrive.label());
    let key = group_key(prefix, &label);
    let crashed_scope = scope(prefix, &label);
    crash_at(make, window, &key, &crashed_scope).await;
    until_leases_lapse(make, &key).await;
    match redrive {
        GroupRedrive::SameOpener => {
            redrive_same_opener(make, window, &key, &crashed_scope).await;
        }
        GroupRedrive::DifferentOpenerSameKeys => {
            redrive_different_opener(make, window, &key, &crashed_scope).await;
        }
        GroupRedrive::NoOpener => {
            redrive_no_opener(make, window, &key).await;
        }
    }
}

/// Leaves the residue the window names, verified before the process dies where
/// verification needs a live claimant.
async fn crash_at(
    make: &DrainWorldFactory,
    window: GroupCrashWindow,
    key: &str,
    crashed_scope: &ExecutionScope,
) {
    match window {
        GroupCrashWindow::OpenJournaledBeforeDispatch => {
            crashed_process_with_probe(make, {
                let key = key.to_string();
                let scope = crashed_scope.clone();
                move |world, probe| {
                    Box::pin(async move {
                        let scoped = world.host.scoped(scope.clone()).expect("scope");
                        // `never` rather than `settles`: the residue must be
                        // *unsettled* children, and an executor that could
                        // finish would let a fast child journal a terminal
                        // before the process died — a different window's
                        // residue. Whether dispatch ran is not externally
                        // observable; unsettled membership is.
                        let _handle = open(&scoped, &key, 2, RUN, vec![never(), never()]).await;
                        // The retained membership is the window's durable
                        // fact, and a drain pass cannot see it: the drain's
                        // work list is unsettled *rows*, and a child nobody
                        // claimed has no row yet. A second opener's fence is
                        // what proves the journal holds the group — the
                        // reopen's header is judged against the retained
                        // record, so it succeeding *is* the assertion — and
                        // its settle window expiring proves no rank was
                        // journaled before the process died.
                        let probe_scoped = probe.host.scoped(scope).expect("scope");
                        let mut probe_handle =
                            open(&probe_scoped, &key, 2, RUN, vec![never(), never()]).await;
                        assert!(
                            tokio::time::timeout(
                                REFUSAL_WINDOW,
                                probe_scoped.controller().await_next_settlement(
                                    &mut probe_handle,
                                    CancellationToken::new(),
                                ),
                            )
                            .await
                            .is_err(),
                            "no rank was journaled before the crash"
                        );
                    })
                }
            })
            .await;
        }
        GroupCrashWindow::ChildrenInFlightBeforeSettlement => {
            crashed_process(make, {
                let key = key.to_string();
                let scope = crashed_scope.clone();
                move |world| {
                    Box::pin(async move {
                        let scoped = world.host.scoped(scope).expect("scope");
                        let entered = Arc::new(AtomicUsize::new(0));
                        let _handle = open(
                            &scoped,
                            &key,
                            2,
                            RUN,
                            vec![blocking(&entered), blocking(&entered)],
                        )
                        .await;
                        // Entry means the claim committed: the residue is two
                        // children in flight under leases nobody will renew.
                        until(|| entered.load(Ordering::SeqCst) == 2).await;
                    })
                }
            })
            .await;
        }
        GroupCrashWindow::ChildSettledBeforeConsume => {
            crashed_process_with_probe(make, {
                let key = key.to_string();
                let scope = crashed_scope.clone();
                move |world, probe| {
                    Box::pin(async move {
                        let scoped = world.host.scoped(scope).expect("scope");
                        let entered = Arc::new(AtomicUsize::new(0));
                        let _handle =
                            open(&scoped, &key, 2, RUN, vec![settles(0), blocking(&entered)]).await;
                        until(|| entered.load(Ordering::SeqCst) == 1).await;
                        // The residue must be a *journaled* rank beside a live
                        // claim, not a child that merely returned. The drain's
                        // work list is children holding no rank, so position 0
                        // being journaled reads as *absence* from the report
                        // while position 1 reads as a live lease — both checked
                        // while the crashed world's claim is still live.
                        let report = pass(&probe, &key)
                            .await
                            .expect("the probe reads the journal");
                        assert_eq!(
                            report.children.len(),
                            1,
                            "position 0's rank is journaled, so it has left \
                             the drain's queue: {report:?}"
                        );
                        assert!(
                            matches!(
                                outcome_of(&report, &child_replay_key(&key, 1)),
                                ChildDrainOutcome::LeaseLive { .. }
                            ),
                            "position 1 is in flight under a live claim when \
                             the process dies: {report:?}"
                        );
                    })
                }
            })
            .await;
        }
        GroupCrashWindow::ClosingRecordedLosersInFlight => {
            orphan_two_losers(make, key, crashed_scope).await;
        }
    }
}

/// The same durable scope reopens with the accepted children.
///
/// The ruling is *resume*: journaled ranks replay to the new handle without
/// executing anything — which is also the durable answer to W4 and W5 sharing
/// one window, since a new handle's cursor starts at zero and is re-served
/// every journaled rank — while unsettled children execute exactly once each,
/// under the executors this opener staged. A resolver that ran a journaled
/// child again, or a retained child under the wrong request, would show up in
/// `ran` as an extra or a wrong position.
async fn redrive_same_opener(
    make: &DrainWorldFactory,
    window: GroupCrashWindow,
    key: &str,
    crashed_scope: &ExecutionScope,
) {
    let capable = RecordingExecutors::settling();
    let world = make(spec(CRASH_LEASE_MS, &capable)).await;
    let scoped = world
        .host
        .scoped(crashed_scope.clone())
        .expect("the same scope binds");
    let ran: Arc<Mutex<Vec<usize>>> = Arc::default();
    let mut handle = open(
        &scoped,
        key,
        2,
        RUN,
        vec![counting(&ran, 0), counting(&ran, 1)],
    )
    .await;

    let mut positions = Vec::new();
    for _ in 0..2 {
        let settlement = next(&scoped, &mut handle)
            .await
            .expect("every rank settles under the returning opener");
        positions.push(settlement.position);
    }
    if let &[journaled] = window.settled() {
        assert_eq!(
            positions.first(),
            Some(&journaled),
            "the journaled rank is served first — it carries the lowest \
             sequence the group ever committed"
        );
    }
    positions.sort_unstable();
    assert_eq!(
        positions,
        vec![0, 1],
        "the returning opener is served every rank exactly once"
    );

    let mut executed = ran.lock_recover().clone();
    executed.sort_unstable();
    let expected: Vec<usize> = window.unsettled().to_vec();
    assert_eq!(
        executed, expected,
        "only the unsettled children execute: a journaled rank replays from \
         its record and no child runs twice"
    );

    // The caller releases before the completeness check: a group open to a
    // caller in this process is not drainable here, by the same guard the
    // drain suite's own laws rely on.
    close(&scoped, handle, RUN)
        .await
        .expect("the returning caller closes once every rank is served");
    drain_until_no_live_lease(&world, key).await;
    let report = pass(&world, key).await.expect("a final pass runs");
    assert!(
        report.is_complete(),
        "the resumed group drains to completion: {report:?}"
    );
}

/// A different process reopens with the retained replay keys under different
/// requests — the authority substitution, replayed at every crashed state.
///
/// The ruling is *refusal without loss*. Under a key-only match the staged
/// impostor executors would be handed the retained children and run them; the
/// observed request (`saw`) names exactly what ran them, which is how the law
/// distinguishes "nothing ran" from "the right thing ran". Under the binding
/// the only runners left are the ones the resolver produced for the retained
/// children themselves — this host's resolver refuses every command, so the
/// ruling predicts nothing runs, every retained child is asked about once,
/// journaled ranks still replay to the impostor's handle, and an honest host
/// still finishes the queue.
async fn redrive_different_opener(
    make: &DrainWorldFactory,
    window: GroupCrashWindow,
    key: &str,
    crashed_scope: &ExecutionScope,
) {
    let impostor_runs = Arc::new(AtomicUsize::new(0));
    let impostor_saw: Arc<Mutex<Vec<String>>> = Arc::default();
    let refusing = RecordingExecutors::refusing();
    let world = make(spec(CRASH_LEASE_MS, &refusing)).await;
    let scoped = world
        .host
        .scoped(crashed_scope.clone())
        .expect("the same scope binds for a different process");
    let mut handle = open_with(
        &scoped,
        impostor_group(scoped.execution_scope(), key, 2, RUN),
        vec![
            impostor(&impostor_runs, &impostor_saw),
            impostor(&impostor_runs, &impostor_saw),
        ],
    )
    .await;

    for &position in window.settled() {
        let settlement = next(&scoped, &mut handle).await.expect(
            "a journaled rank replays to any opener — refusal governs \
                     execution, not the record",
        );
        assert_eq!(settlement.position, position);
    }
    assert!(
        tokio::time::timeout(
            REFUSAL_WINDOW,
            scoped
                .controller()
                .await_next_settlement(&mut handle, CancellationToken::new()),
        )
        .await
        .is_err(),
        "no settlement arrives for an unsettled retained child: nothing this \
         opener resolved may run it"
    );
    assert_eq!(
        impostor_runs.load(Ordering::SeqCst),
        0,
        "an executor bound to the offered request may not run a retained child \
         under a different request, however equal its replay key"
    );
    assert!(
        impostor_saw.lock_recover().is_empty(),
        "the impostor was never handed a retained request: {:?}",
        impostor_saw.lock_recover()
    );
    let expected_asks: Vec<String> = (0..2).map(|p| child_replay_key(key, p)).collect();
    assert_eq!(
        refusing.asked_about(),
        expected_asks,
        "the routing question the reopen asks is about the retained children, \
         once each — matching on the key alone would ask the resolver nothing"
    );

    let capable = RecordingExecutors::settling();
    let honest = make(spec(CRASH_LEASE_MS, &capable)).await;
    drain_until_no_live_lease(&honest, key).await;
    let report = pass(&honest, key).await.expect("a final pass runs");
    assert!(
        report.is_complete(),
        "refusing the impostor strands nothing: {report:?}"
    );
    let expected_executions: Vec<String> = window
        .unsettled()
        .iter()
        .map(|position| child_replay_key(key, *position))
        .collect();
    assert_eq!(
        capable.executions(),
        expected_executions,
        "the honest host settles each unsettled child exactly once"
    );
}

/// Nobody comes back for the group; the drain is the only redrive.
///
/// The ruling is *the queue outlives the caller*. A host with no resolver at
/// all still reports the queue truthfully — `Settled` where the journal holds
/// a rank, `NoExecutor` where a child is waiting, never a fabricated
/// `is_complete` that would retire the journal of work nobody ran — and a
/// later wired host settles each unsettled child exactly once.
async fn redrive_no_opener(make: &DrainWorldFactory, window: GroupCrashWindow, key: &str) {
    let unwired = make(unwired_spec(CRASH_LEASE_MS)).await;
    let report = pass(&unwired, key)
        .await
        .expect("an unwired host's drain reports the queue rather than refusing it");
    // The queue is exactly the unsettled children: a journaled rank has left
    // the drain's work list, so `settled` positions must be *absent* — their
    // being reported again would mean a journaled rank re-entered the queue.
    let mut reported: Vec<String> = report
        .children
        .iter()
        .map(|child| child.replay_key.clone())
        .collect();
    reported.sort();
    let expected_queue: Vec<String> = window
        .unsettled()
        .iter()
        .map(|position| child_replay_key(key, *position))
        .collect();
    assert_eq!(
        reported, expected_queue,
        "the pass reports exactly the unsettled children: {report:?}"
    );
    for child in &report.children {
        assert_eq!(
            child.outcome,
            ChildDrainOutcome::NoExecutor,
            "a child no opener can run is reported waiting, not invented or \
             hidden: {report:?}"
        );
    }
    assert!(
        !report.is_complete(),
        "an unwired host must not report a group with unsettled children as \
         complete: that answer retires the journal of work nobody ran"
    );

    let capable = RecordingExecutors::settling();
    let honest = make(spec(CRASH_LEASE_MS, &capable)).await;
    drain_until_no_live_lease(&honest, key).await;
    let report = pass(&honest, key).await.expect("a final pass runs");
    assert!(
        report.is_complete(),
        "the abandoned group still drains to completion: {report:?}"
    );
    let expected_executions: Vec<String> = window
        .unsettled()
        .iter()
        .map(|position| child_replay_key(key, *position))
        .collect();
    assert_eq!(
        capable.executions(),
        expected_executions,
        "with no opener the drain still settles each unsettled child exactly once"
    );
}

/// A staged executor that records which positions actually ran under this
/// opener, so a redrive can name *what executed* rather than only that the
/// group settled.
fn counting(ran: &Arc<Mutex<Vec<usize>>>, position: usize) -> RuntimeEffectLocalExecutor<'static> {
    let ran = Arc::clone(ran);
    RuntimeEffectLocalExecutor::testing(move |_| {
        let ran = Arc::clone(&ran);
        async move {
            ran.lock_recover().push(position);
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "position": position }),
            })
        }
    })
}

/// [`crashed_process`]'s two-world variant: the crashed world plus a probe
/// world on the same doomed runtime, for windows whose residue is only
/// meaningful while the crashed world's claims are still live.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn crashed_process_with_probe<P>(make: &DrainWorldFactory, phase: P)
where
    P: FnOnce(DrainWorld, DrainWorld) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
{
    let make = Arc::clone(make);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the crashing process gets a runtime of its own");
        runtime.block_on(async move {
            let world = make(spec(CRASH_LEASE_MS, &RecordingExecutors::settling())).await;
            // A refusing resolver asks the drain's question and answers
            // `NoExecutor`: the probe reads the queue without writing anything
            // into it.
            let probe = make(spec(CRASH_LEASE_MS, &RecordingExecutors::refusing())).await;
            phase(world, probe).await;
        });
        drop(runtime);
    })
    .join()
    .expect("the crashing process runs its phase before dying");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_adr_0099_crash_window_row_is_classified() {
        assert_adr_0099_table_is_reviewed();
    }
}
