//! The loser drain, as laws every SQL tier answers the same way (FIG-1536).
//!
//! The drain is the reclamation layer 2.5 named and did not build: a group
//! closed under `RunToCompletion` releases its caller while children keep
//! running, and children the closing process was not running are left
//! `in_progress` in the journal with nobody driving them. These laws hold the
//! two SQL tiers to one account of what happens to those rows.
//!
//! # Why a law here needs two processes, and how it gets one
//!
//! Every interesting drain question is about a process that is *gone*. A host
//! whose children are still running renews their leases, so nothing is
//! reclaimable and nothing is proved. Killing the caller's future is not enough
//! either: a group's children run on host-owned tasks precisely so that dropping
//! the caller does not drop them.
//!
//! So the pre-crash phase of a law runs on its own Tokio runtime, on its own
//! thread, with its own host over the same substrate — and that runtime is
//! dropped. Dropping a runtime drops its tasks, which is what a process exit
//! does to them, and it takes the host's substrate handles with it. What
//! survives is exactly what a real crash leaves behind: journal rows under
//! leases nobody is renewing.
//!
//! # What the laws are about
//!
//! * A drain never touches a group that is still open — refused outright in this
//!   process, skipped on a live lease across processes.
//! * A closed group's orphaned losers settle, exactly once, journal-visible,
//!   across a restart of the driver.
//! * A `Cancel` group's children are left alone: their terminals are synthesized
//!   inside their own claims at close, and the drain has no terminal of its own
//!   to invent.
//! * A host that cannot run a child says so rather than fabricating an outcome.
//! * A pass is bounded: cancelling it stops the child in flight, reports the
//!   untouched tail, and damages nothing.
//! * A child another host claimed out from under the pass is reported, not
//!   waited out.
//! * A fully drained group has nothing unsettled, which is what makes it
//!   reclaimable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::runtime::effect::group_drain::{
    ChildDrainOutcome, EffectGroupDrain, GroupDrainExecutors, GroupDrainReport,
};
use crate::{
    CheckedEffectGroup, EffectGroupHandle, GroupSettlement, GroupWakePolicy, LoserDisposition,
    RuntimeEffectGroup,
};

/// One host over the substrate under test, plus the drain wired to it.
///
/// Both come from the same factory call because they must share a journal: a
/// drain over a different substrate would answer every law with "nothing to do".
pub struct DrainWorld {
    /// The effect host a law opens and closes groups through.
    pub host: Arc<dyn EffectHost>,
    /// The drain over the same journal, carrying the executors the law asked
    /// for.
    pub drain: Arc<dyn EffectGroupDrain>,
}

/// What a law needs the next host to be built with.
pub struct DrainWorldSpec {
    /// The effect-lease window. Laws that simulate a crash need leases that
    /// expire inside a test's patience; laws about a *live* group need the
    /// opposite and ask for a long one.
    pub lease_ttl_ms: u64,
    /// The host wiring seam under test, supplied per law so a law can count what
    /// the drain asked to run.
    pub executors: Arc<dyn GroupDrainExecutors>,
}

/// Builds a world over one substrate, as many times as a law needs.
///
/// Callable from any runtime — a crash law calls it from a runtime it is about
/// to destroy — so a backend must open its own substrate handles inside the
/// future rather than capturing handles bound to the test's runtime.
pub type DrainWorldFactory =
    Arc<dyn Fn(DrainWorldSpec) -> Pin<Box<dyn Future<Output = DrainWorld> + Send>> + Send + Sync>;

/// Run the effect-group drain suite.
pub async fn effect_group_drain_conformance(make: DrainWorldFactory) {
    let prefix = format!("drain-conformance-{}", uuid::Uuid::new_v4().simple());
    a_group_this_process_is_still_working_is_refused(&make, &prefix).await;
    a_group_the_journal_does_not_hold_is_refused(&make, &prefix).await;
    a_live_lease_is_left_to_the_executor_that_holds_it(&make, &prefix).await;
    orphaned_losers_settle_exactly_once_across_a_restart(&make, &prefix).await;
    a_cancelled_pass_stops_at_the_child_it_was_running(&make, &prefix).await;
    a_child_another_drain_holds_is_reported_contested(&make, &prefix).await;
    a_cancel_group_is_never_re_executed_by_the_drain(&make, &prefix).await;
    a_child_this_host_cannot_run_is_reported_not_invented(&make, &prefix).await;
}

// =============================================================================
// Laws
// =============================================================================

/// The drain reclaims groups whose caller is gone, and this process can see
/// perfectly well that this one's is not.
///
/// Two halves, and the second is the one that is easy to miss. A group *open*
/// here has a caller entitled to read its settlements. A group *closed* here
/// still has losers this host's own executors are running under
/// `RunToCompletion`, and reclaiming one of those would be the host stealing
/// from itself. Both refusals are checked before anything else, so the executors
/// are never even consulted — which is what the invocation counts assert.
async fn a_group_this_process_is_still_working_is_refused(make: &DrainWorldFactory, prefix: &str) {
    let executors = RecordingExecutors::settling();
    let world = make(spec(LIVE_LEASE_MS, &executors)).await;
    let key = group_key(prefix, "open-here");
    let scoped = world
        .host
        .scoped(scope(prefix, "open-here"))
        .expect("scope");
    let handle = open(&scoped, &key, 2, RUN, vec![never(), never()]).await;

    let error = world
        .drain
        .drain_group(&key, &CancellationToken::new())
        .await
        .expect_err("a group open to a caller in this process is not drainable");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupDrainDeferred,
        "the refusal is 'come back', not 'never': {error}"
    );
    assert!(
        error.code.is_retryable(),
        "a host must be able to tell a wait-and-retry refusal from a permanent \
         one by its code alone, without reading the message: {error}"
    );
    assert_eq!(
        executors.invocations(),
        0,
        "the open-group guard runs before the queue is read, so no child is even \
         considered for execution"
    );

    // Closing is not enough to make a group drainable *here*. The caller is
    // released, but this host's own executors are still running both losers
    // under `RunToCompletion`, and a host that drained now would reclaim a child
    // from itself the moment one of those executors stalled past its lease —
    // the one race the claim fence cannot arbitrate, because both sides are this
    // process.
    close(&scoped, handle, RUN)
        .await
        .expect("the caller closes");
    let error = world
        .drain
        .drain_group(&key, &CancellationToken::new())
        .await
        .expect_err("a closed group whose children this host is still running is not drainable");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupDrainDeferred
    );
    assert!(error.code.is_retryable(), "{error}");
    assert_eq!(
        executors.invocations(),
        0,
        "still no child is considered: the guard is about this host's own work, \
         not about what the journal says"
    );
}

/// The drain applies the disposition the group declared, so a group the journal
/// does not hold has no disposition for it to apply — and it refuses rather than
/// picking one.
async fn a_group_the_journal_does_not_hold_is_refused(make: &DrainWorldFactory, prefix: &str) {
    let executors = RecordingExecutors::settling();
    let world = make(spec(LIVE_LEASE_MS, &executors)).await;
    let error = world
        .drain
        .drain_group(
            &group_key(prefix, "never-opened"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("an unrecorded group is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "and this one is 'never', which is why it does not share a code with the \
         deferrals: {error}"
    );
    assert!(
        !error.code.is_retryable(),
        "retrying will not conjure a group the journal does not hold: {error}"
    );
    assert_eq!(executors.invocations(), 0);
}

/// Across processes, the lease is what says "someone owns this now".
///
/// The draining host has never seen this group, so its local guard says nothing;
/// what protects the live group's children is the lease each of them is running
/// under. The drain reports them and moves on rather than queueing behind them.
async fn a_live_lease_is_left_to_the_executor_that_holds_it(
    make: &DrainWorldFactory,
    prefix: &str,
) {
    let running = RecordingExecutors::settling();
    let owner = make(spec(LIVE_LEASE_MS, &running)).await;
    let key = group_key(prefix, "live-lease");
    let scoped = owner
        .host
        .scoped(scope(prefix, "live-lease"))
        .expect("scope");
    let entered = Arc::new(AtomicUsize::new(0));
    let handle = open(
        &scoped,
        &key,
        2,
        RUN,
        vec![blocking(&entered), blocking(&entered)],
    )
    .await;
    // Both children have entered their executors, so both rows are claimed and
    // both leases are live. Before this point the journal holds no row at all
    // and the law would pass for the wrong reason.
    until(|| entered.load(Ordering::SeqCst) == 2).await;

    let drainer_executors = RecordingExecutors::settling();
    let drainer = make(spec(LIVE_LEASE_MS, &drainer_executors)).await;
    let report = drainer
        .drain
        .drain_group(&key, &CancellationToken::new())
        .await
        .expect("a drain of a group this process never opened is allowed to run");
    assert_eq!(report.children.len(), 2);
    for child in &report.children {
        assert!(
            matches!(child.outcome, ChildDrainOutcome::LeaseLive { .. }),
            "a child under a live lease is left to its own executor, got {:?}",
            child.outcome
        );
    }
    assert_eq!(
        drainer_executors.invocations(),
        0,
        "a skipped child is never handed to the draining host's executors"
    );
    assert!(
        !report.is_complete(),
        "a group with unsettled children is not reclaimable"
    );

    close(&scoped, handle, RUN)
        .await
        .expect("the caller closes");
}

/// The headline law: a crash between close and drain completion recovers
/// exactly once.
///
/// The pre-crash process opens a three-child group, consumes the settlement its
/// one fast child produces, closes under `RunToCompletion`, and dies with two
/// children still `in_progress`. A second driver — new owner id, new lease
/// counter, none of the first's memory — drains them. Each loser runs exactly
/// once, and a second drain pass finds nothing to do: the count is the proof,
/// and the ranks read back through the group surface are the journal-visible
/// half of it.
async fn orphaned_losers_settle_exactly_once_across_a_restart(
    make: &DrainWorldFactory,
    prefix: &str,
) {
    let key = group_key(prefix, "crash-drain");
    let scope = scope(prefix, "crash-drain");
    crashed_process(make, {
        let key = key.clone();
        let scope = scope.clone();
        move |world| {
            Box::pin(async move {
                let scoped = world.host.scoped(scope).expect("scope");
                let entered = Arc::new(AtomicUsize::new(0));
                let mut handle = open(
                    &scoped,
                    &key,
                    3,
                    RUN,
                    vec![settles(0), blocking(&entered), blocking(&entered)],
                )
                .await;
                let first = next(&scoped, &mut handle)
                    .await
                    .expect("the fast child settles first");
                assert_eq!(first.position, 0);
                // Both losers are claimed and journaled before the process dies,
                // which is what makes them the drain's queue rather than rows
                // that never existed.
                until(|| entered.load(Ordering::SeqCst) == 2).await;
                close(&scoped, handle, RUN)
                    .await
                    .expect("the caller closes and releases its losers");
            })
        }
    })
    .await;

    let executors = RecordingExecutors::settling();
    let world = make(spec(CRASH_LEASE_MS, &executors)).await;
    let report = drain_until_no_live_lease(&world, &key).await;
    assert_eq!(
        report.disposition,
        LoserDisposition::RunToCompletion,
        "the disposition comes off the group row, which the drain did not write"
    );
    assert_eq!(
        executors.executions(),
        vec![child_replay_key(&key, 1), child_replay_key(&key, 2)],
        "each orphaned loser runs exactly once, and the child that had already \
         settled does not run at all"
    );

    let second = world
        .drain
        .drain_group(&key, &CancellationToken::new())
        .await
        .expect("a second pass over a drained group is allowed");
    assert!(
        second.is_complete(),
        "a fully drained closed group has nothing unsettled left, which is what \
         makes it reclaimable: {second:?}"
    );
    assert_eq!(
        executors.invocations(),
        2,
        "the second pass runs nothing: exactly-once survives the drain being \
         called again, not just the process being restarted"
    );

    // Journal-visible: a reader holding none of the drain's memory reads all
    // three ranks back, in rank order, one per child.
    let reader = make(spec(CRASH_LEASE_MS, &RecordingExecutors::settling())).await;
    let scoped = reader.host.scoped(scope).expect("scope");
    let mut handle = reopen(&scoped, &key, 3, RUN).await;
    let mut positions = Vec::new();
    let mut sequences = Vec::new();
    for rank in 1..=3 {
        let settlement = next(&scoped, &mut handle)
            .await
            .unwrap_or_else(|err| panic!("rank {rank} is served from the journal: {err}"));
        positions.push(settlement.position);
        sequences.push(settlement.sequence);
    }
    positions.sort_unstable();
    assert_eq!(
        positions,
        vec![0, 1, 2],
        "every child settled once, so every position appears once"
    );
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "ranks are served in ascending settlement order: {sequences:?}"
    );
    close(&scoped, handle, RUN)
        .await
        .expect("the reader closes");
}

/// A pass is bounded: cancelling it stops the child in flight and says so.
///
/// Without this a `drain_group` call is unbounded — a child runs for as long as
/// its effect does, and a pass runs its children one at a time — so an operator,
/// a shutdown, or a supervising timer would have no way to take a pass back. The
/// law holds the pass inside a child's execution, cancels, and requires the call
/// to return; then it requires that nothing was damaged by stopping, by draining
/// the same group to completion afterwards with each child run exactly once.
async fn a_cancelled_pass_stops_at_the_child_it_was_running(
    make: &DrainWorldFactory,
    prefix: &str,
) {
    let key = group_key(prefix, "cancelled-pass");
    let scope = scope(prefix, "cancelled-pass");
    orphan_two_losers(make, &key, &scope).await;
    until_leases_lapse(make, &key).await;

    let entered = Arc::new(AtomicUsize::new(0));
    let release = CancellationToken::new();
    let held = RecordingExecutors::uniform(ExecutorAnswer::Hold {
        entered: Arc::clone(&entered),
        release: release.clone(),
    });
    let world = make(spec(CRASH_LEASE_MS, &held)).await;
    let cancel = CancellationToken::new();
    let running = {
        let drain = Arc::clone(&world.drain);
        let key = key.clone();
        let cancel = cancel.clone();
        crate::task::spawn(async move { drain.drain_group(&key, &cancel).await })
    };
    // The pass is now *inside* a child's execution, holding its claim. Cancelling
    // before this point would prove only that a token can be checked at the top
    // of a loop.
    until(|| entered.load(Ordering::SeqCst) >= 1).await;
    cancel.cancel();

    let report = tokio::time::timeout(AWAIT_BUDGET, running)
        .await
        .expect("a cancelled pass returns instead of finishing the child it holds")
        .expect("the pass task is not itself cancelled")
        .expect("a cancelled pass reports what it did rather than failing");
    assert_eq!(report.children.len(), 2);
    for child in &report.children {
        assert_eq!(
            child.outcome,
            ChildDrainOutcome::Interrupted,
            "the child in flight and the child behind it are both reported \
             untouched, so the report cannot be read as an examined empty queue"
        );
    }
    assert_eq!(report.settled(), 0);
    assert!(!report.is_complete());
    release.cancel();

    // Nothing was damaged by stopping: the abandoned claim lapses like a crashed
    // executor's, and a later pass settles both children exactly once.
    let capable = RecordingExecutors::settling();
    let second = make(spec(CRASH_LEASE_MS, &capable)).await;
    // Accumulated across passes, not read off the last one: the abandoned claim
    // outlives the child beside it, so the pass that reclaims it is a later pass
    // whose queue no longer mentions the child settled first.
    drain_until_no_live_lease(&second, &key).await;
    assert_eq!(
        capable.executions(),
        vec![child_replay_key(&key, 0), child_replay_key(&key, 1)],
        "an interrupted child is reclaimed, not run twice and not lost"
    );
    let settled = pass(&second, &key).await.expect("a final pass runs");
    assert!(
        settled.is_complete(),
        "the group a cancelled pass left behind still drains to completion: \
         {settled:?}"
    );
}

/// A child another drain is running right now is reported, not queued behind.
///
/// This is the busy-claim half of "someone else owns it": the pass read the
/// child's lease as expired and, by the time it reached for the claim, another
/// host held it. Queueing there is the failure mode — a pass that waits out a
/// live renewer is unbounded, and the rest of its queue is the work it was
/// called for.
///
/// The race is built rather than hoped for. Host B's pass reads both children
/// while both leases are lapsed, then parks inside child 0's execution; host A
/// then finds child 0 live-leased and takes child 1; releasing B sends it on to
/// child 1, whose claim is now busy. Both hosts' passes are real passes over a
/// real substrate, so this is also the only integration exercise of the
/// end-of-pass confirm read against a genuine concurrent writer.
async fn a_child_another_drain_holds_is_reported_contested(make: &DrainWorldFactory, prefix: &str) {
    let key = group_key(prefix, "contested");
    let scope = scope(prefix, "contested");
    orphan_two_losers(make, &key, &scope).await;
    until_leases_lapse(make, &key).await;

    let b_entered = Arc::new(AtomicUsize::new(0));
    let b_release = CancellationToken::new();
    let b_executors = RecordingExecutors::by_position(
        vec![ExecutorAnswer::Hold {
            entered: Arc::clone(&b_entered),
            release: b_release.clone(),
        }],
        ExecutorAnswer::Settle,
    );
    let b = make(spec(LIVE_LEASE_MS, &b_executors)).await;
    let b_pass = {
        let drain = Arc::clone(&b.drain);
        let key = key.clone();
        crate::task::spawn(async move { drain.drain_group(&key, &CancellationToken::new()).await })
    };
    until(|| b_entered.load(Ordering::SeqCst) >= 1).await;

    let a_entered = Arc::new(AtomicUsize::new(0));
    let a_release = CancellationToken::new();
    let a_executors = RecordingExecutors::by_position(
        vec![ExecutorAnswer::Refuse],
        ExecutorAnswer::Hold {
            entered: Arc::clone(&a_entered),
            release: a_release.clone(),
        },
    );
    let a = make(spec(LIVE_LEASE_MS, &a_executors)).await;
    let a_pass = {
        let drain = Arc::clone(&a.drain);
        let key = key.clone();
        crate::task::spawn(async move { drain.drain_group(&key, &CancellationToken::new()).await })
    };
    // Host A now holds child 1's claim, and will hold it until released.
    until(|| a_entered.load(Ordering::SeqCst) >= 1).await;

    b_release.cancel();
    let report = tokio::time::timeout(AWAIT_BUDGET, b_pass)
        .await
        .expect("the pass that lost a claim returns rather than waiting the winner out")
        .expect("the pass task is not cancelled")
        .expect("losing a claim is a reported outcome, not a failed pass");
    assert_eq!(
        outcome_of(&report, &child_replay_key(&key, 0)),
        ChildDrainOutcome::Settled,
        "the child this pass did win is settled: {report:?}"
    );
    assert_eq!(
        outcome_of(&report, &child_replay_key(&key, 1)),
        ChildDrainOutcome::Contested,
        "the child the other host took is reported as contested, not as settled \
         and not waited out: {report:?}"
    );
    assert_eq!(report.settled(), 1);
    assert!(
        !report.is_complete(),
        "a contested child keeps the group unreclaimable"
    );

    a_release.cancel();
    let a_report = tokio::time::timeout(AWAIT_BUDGET, a_pass)
        .await
        .expect("the winning pass finishes")
        .expect("the pass task is not cancelled")
        .expect("the winning pass reports");
    assert_eq!(
        outcome_of(&a_report, &child_replay_key(&key, 1)),
        ChildDrainOutcome::Settled,
        "the contested child did settle — on the host that owned it: {a_report:?}"
    );

    // Exact multisets, asks and executions kept apart. A deduplicated count
    // would have passed a host that asked for the same child twice, which is the
    // defect a contest is most likely to produce.
    assert_eq!(
        b_executors.asked_about(),
        vec![child_replay_key(&key, 0), child_replay_key(&key, 1)],
        "the losing host asked about both children, once each: it won the first \
         and reached for the second"
    );
    assert_eq!(
        b_executors.executions(),
        vec![child_replay_key(&key, 0)],
        "and ran only the one it won — the contested child was asked for and \
         never executed here, which is the difference an ask count cannot see"
    );
    assert_eq!(
        a_executors.asked_about(),
        vec![child_replay_key(&key, 1)],
        "the winning host never asked about the child it found live-leased"
    );
    assert_eq!(
        a_executors.executions(),
        vec![child_replay_key(&key, 1)],
        "and ran the contested child exactly once"
    );
    let mut across_hosts = b_executors.executions();
    across_hosts.extend(a_executors.executions());
    across_hosts.sort();
    assert_eq!(
        across_hosts,
        vec![child_replay_key(&key, 0), child_replay_key(&key, 1)],
        "two children, two executions, however the contest resolved"
    );
}

/// The drain never runs a `Cancel` group's child, whatever state that child is
/// in.
///
/// A `Cancel` group's terminals belong to the process running each child, which
/// synthesizes them inside the claim it already holds. The drain has no terminal
/// of its own to invent here — and it says so, child by child, rather than
/// reporting an empty pass that could not be told apart from a group with
/// nothing left.
///
/// The fixture is the harshest case rather than the tidiest: a process that died
/// *between open and close*, so the close-time terminals were never synthesized
/// at all and the children are simply orphaned. Even then the drain declines,
/// which is the point — the refusal is read off the group's declared
/// disposition, not off what happened to the children.
async fn a_cancel_group_is_never_re_executed_by_the_drain(make: &DrainWorldFactory, prefix: &str) {
    let key = group_key(prefix, "cancel-declared");
    let scope = scope(prefix, "cancel-declared");
    crashed_process(make, {
        let key = key.clone();
        let scope = scope.clone();
        move |world| {
            Box::pin(async move {
                let scoped = world.host.scoped(scope).expect("scope");
                let entered = Arc::new(AtomicUsize::new(0));
                let handle = open(
                    &scoped,
                    &key,
                    2,
                    CANCEL,
                    vec![blocking(&entered), blocking(&entered)],
                )
                .await;
                until(|| entered.load(Ordering::SeqCst) == 2).await;
                // Deliberately no close: this is the process dying between open
                // and close, which is the only way a `Cancel` group reaches the
                // drain with children still unsettled.
                drop(handle);
            })
        }
    })
    .await;

    let executors = RecordingExecutors::settling();
    let world = make(spec(CRASH_LEASE_MS, &executors)).await;
    // No wait for the dead leases: the disposition is read before the lease is,
    // so a `Cancel` group answers the same whether or not its children still
    // look claimed. That ordering is the law.
    let report = pass(&world, &key).await.expect("the pass runs");
    assert_eq!(report.disposition, LoserDisposition::Cancel);
    assert_eq!(report.children.len(), 2);
    for child in &report.children {
        assert_eq!(
            child.outcome,
            ChildDrainOutcome::CancelDeclared,
            "a cancel-declared child is named, not silently dropped from the pass"
        );
    }
    assert_eq!(
        executors.invocations(),
        0,
        "the drain never runs a cancel-declared child"
    );
    assert_eq!(
        report.settled(),
        0,
        "and never reports one as settled: these rows are still unsettled, which \
         is the bounded residual a crash before close leaves"
    );
}

/// A host that cannot run a journaled command answers `None`, and the pass
/// reports it.
///
/// The alternative — an executor that fabricates an outcome — would journal a
/// terminal no effect ever produced, at the rank a real one would have taken.
async fn a_child_this_host_cannot_run_is_reported_not_invented(
    make: &DrainWorldFactory,
    prefix: &str,
) {
    let key = group_key(prefix, "no-executor");
    let scope = scope(prefix, "no-executor");
    crashed_process(make, {
        let key = key.clone();
        let scope = scope.clone();
        move |world| {
            Box::pin(async move {
                let scoped = world.host.scoped(scope).expect("scope");
                let entered = Arc::new(AtomicUsize::new(0));
                let handle = open(&scoped, &key, 1, RUN, vec![blocking(&entered)]).await;
                until(|| entered.load(Ordering::SeqCst) == 1).await;
                close(&scoped, handle, RUN)
                    .await
                    .expect("the caller closes");
            })
        }
    })
    .await;

    let refusing = RecordingExecutors::refusing();
    let world = make(spec(CRASH_LEASE_MS, &refusing)).await;
    let report = loop {
        let report = pass(&world, &key).await.expect("the pass runs");
        if report
            .children
            .iter()
            .all(|child| !matches!(child.outcome, ChildDrainOutcome::LeaseLive { .. }))
        {
            break report;
        }
        tokio::time::sleep(POLL).await;
    };
    assert_eq!(report.children.len(), 1);
    assert_eq!(report.children[0].outcome, ChildDrainOutcome::NoExecutor);
    assert_eq!(
        refusing.invocations(),
        1,
        "the executors were asked and answered `None`; the child was not skipped \
         before the host got a say"
    );
    assert!(
        !report.is_complete(),
        "a child no host will run keeps the group unreclaimable, which is the \
         state an operator has to be able to see"
    );

    // And the queue survives: a host that *can* run it still finds it.
    let capable = RecordingExecutors::settling();
    let second = make(spec(CRASH_LEASE_MS, &capable)).await;
    let report = drain_until_no_live_lease(&second, &key).await;
    assert_eq!(report.settled(), 1);
    assert_eq!(capable.executions(), vec![child_replay_key(&key, 0)]);
}

// =============================================================================
// Fixtures
// =============================================================================

const RUN: LoserDisposition = LoserDisposition::RunToCompletion;
const CANCEL: LoserDisposition = LoserDisposition::Cancel;

/// The lease window a crash law uses: long enough that a claim is not lost while
/// the pre-crash process is still working, short enough that a dead process's
/// rows become reclaimable inside a test's patience.
const CRASH_LEASE_MS: u64 = 900;

/// The lease window a live-group law uses: longer than the law, so a lease
/// expiring mid-test can never be mistaken for the drain honoring it.
const LIVE_LEASE_MS: u64 = 60_000;

const POLL: Duration = Duration::from_millis(25);

/// Every await is bounded: a host that never serves a rank must fail this suite
/// as a timeout at the law that waited, not as a hung test binary.
const AWAIT_BUDGET: Duration = Duration::from_secs(30);

fn spec(lease_ttl_ms: u64, executors: &Arc<RecordingExecutors>) -> DrainWorldSpec {
    DrainWorldSpec {
        lease_ttl_ms,
        executors: Arc::clone(executors) as Arc<dyn GroupDrainExecutors>,
    }
}

/// Runs `phase` on a host of its own, on a runtime of its own, and then destroys
/// that runtime.
///
/// This is the suite's crash. Dropping a Tokio runtime drops every task it owns,
/// including the host-owned tasks a group's children run on, and it drops the
/// host's substrate handles with them. What is left behind is what a killed
/// process leaves: journal rows under leases nobody renews.
async fn crashed_process<P>(make: &DrainWorldFactory, phase: P)
where
    P: FnOnce(DrainWorld) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
{
    let make = Arc::clone(make);
    let executors = RecordingExecutors::settling();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the crashing process gets a runtime of its own");
        runtime.block_on(async move {
            let world = make(spec(CRASH_LEASE_MS, &executors)).await;
            phase(world).await;
        });
        drop(runtime);
    })
    .join()
    .expect("the crashing process runs its phase before dying");
}

/// Drains until a pass finds nothing left under a live lease, and returns that
/// pass.
///
/// A law cannot know when a dead process's leases expire — that is the
/// substrate's clock, not the test's — so it retries rather than sleeping a
/// guessed interval. Two children claimed together need not expire together
/// either, so the loop's condition is per-child: keep going while any child was
/// skipped as live, and the executors accumulate across passes.
/// Leaves a closed two-child `RunToCompletion` group whose losers are claimed,
/// journaled, and owned by nobody.
///
/// The starting state every drain law that is not about a *live* group needs,
/// and the one a crash actually produces: the caller closed and was released,
/// the children never settled, and the process that claimed them is gone.
async fn orphan_two_losers(make: &DrainWorldFactory, key: &str, scope: &ExecutionScope) {
    crashed_process(make, {
        let key = key.to_string();
        let scope = scope.clone();
        move |world| {
            Box::pin(async move {
                let scoped = world.host.scoped(scope).expect("scope");
                let entered = Arc::new(AtomicUsize::new(0));
                let handle = open(
                    &scoped,
                    &key,
                    2,
                    RUN,
                    vec![blocking(&entered), blocking(&entered)],
                )
                .await;
                until(|| entered.load(Ordering::SeqCst) == 2).await;
                close(&scoped, handle, RUN)
                    .await
                    .expect("the caller closes and releases its losers");
            })
        }
    })
    .await;
}

fn outcome_of(report: &GroupDrainReport, replay_key: &str) -> ChildDrainOutcome {
    report
        .children
        .iter()
        .find(|child| child.replay_key == replay_key)
        .unwrap_or_else(|| panic!("the pass reported on `{replay_key}`: {report:?}"))
        .outcome
        .clone()
}

/// One pass nobody is cancelling.
async fn pass(
    world: &DrainWorld,
    group_key: &str,
) -> Result<GroupDrainReport, RuntimeEffectControllerError> {
    world
        .drain
        .drain_group(group_key, &CancellationToken::new())
        .await
}

/// Waits until a dead process's claims have lapsed, without running anything.
///
/// A law that needs an *expired* starting state cannot sleep a guessed interval
/// — expiry is the substrate's clock — and cannot poll with a settling host,
/// which would drain the very children the law is about. A refusing host asks
/// the same question and answers `NoExecutor`, which writes nothing.
async fn until_leases_lapse(make: &DrainWorldFactory, group_key: &str) {
    let probe = make(spec(CRASH_LEASE_MS, &RecordingExecutors::refusing())).await;
    tokio::time::timeout(AWAIT_BUDGET, async {
        loop {
            let report = pass(&probe, group_key)
                .await
                .expect("a probe pass over a closed group runs");
            if report
                .children
                .iter()
                .all(|child| !matches!(child.outcome, ChildDrainOutcome::LeaseLive { .. }))
            {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("a dead process's claims lapse");
}

async fn drain_until_no_live_lease(world: &DrainWorld, group_key: &str) -> GroupDrainReport {
    tokio::time::timeout(AWAIT_BUDGET, async {
        loop {
            let report = world
                .drain
                .drain_group(group_key, &CancellationToken::new())
                .await
                .expect("a drain pass over a closed group runs");
            if report
                .children
                .iter()
                .all(|child| !matches!(child.outcome, ChildDrainOutcome::LeaseLive { .. }))
            {
                return report;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the orphaned losers become reclaimable once their leases expire")
}

fn scope(prefix: &str, label: &str) -> ExecutionScope {
    ExecutionScope::runtime_operation(format!("{prefix}-{label}"))
}

fn group_key(prefix: &str, label: &str) -> String {
    format!("{prefix}:group:{label}:0")
}

fn child_replay_key(group_key: &str, position: usize) -> String {
    format!("{group_key}:child:{position}")
}

fn child(group_key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new(group_key),
            "effect",
            RuntimeEffectKind::LanguageRuntimeValue,
            child_replay_key(group_key, position),
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: format!("group-child-{position}"),
        },
    )
}

fn group(key: &str, children: usize, disposition: LoserDisposition) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            RuntimeScope::new(key),
            "group",
            RuntimeEffectKind::LanguageRuntimeValue,
            format!("{key}:group"),
        ),
        key,
        (0..children).map(|position| child(key, position)).collect(),
        GroupWakePolicy::All,
        disposition,
    )
    .expect("a group with at least one child assembles")
}

async fn open(
    scoped: &ScopedEffectController<'_>,
    key: &str,
    children: usize,
    disposition: LoserDisposition,
    executors: Vec<RuntimeEffectLocalExecutor<'static>>,
) -> EffectGroupHandle {
    scoped
        .controller()
        .open_effect_group(
            CheckedEffectGroup::try_new(group(key, children, disposition), executors)
                .expect("one executor per child aligns"),
        )
        .await
        .expect("the group opens")
}

/// Reopens a fully settled group with executors that would block if they ran.
///
/// They never do: every child holds a terminal, so each dispatch replays the
/// record. An executor that could settle would hide a reopen that re-ran a child.
async fn reopen(
    scoped: &ScopedEffectController<'_>,
    key: &str,
    children: usize,
    disposition: LoserDisposition,
) -> EffectGroupHandle {
    let executors = (0..children).map(|_| never()).collect();
    open(scoped, key, children, disposition, executors).await
}

async fn next(
    scoped: &ScopedEffectController<'_>,
    handle: &mut EffectGroupHandle,
) -> Result<GroupSettlement, RuntimeEffectControllerError> {
    tokio::time::timeout(
        AWAIT_BUDGET,
        scoped
            .controller()
            .await_next_settlement(handle, CancellationToken::new()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "a settlement at rank {} of durable effect group {} never arrived",
            handle.consumed() + 1,
            handle.group_key()
        )
    })
}

async fn close(
    scoped: &ScopedEffectController<'_>,
    handle: EffectGroupHandle,
    disposition: LoserDisposition,
) -> Result<(), RuntimeEffectControllerError> {
    scoped
        .controller()
        .close_effect_group(handle, disposition)
        .await
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(AWAIT_BUDGET, async {
        while !condition() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the host reaches the awaited state");
}

fn settles(position: usize) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| async move {
        Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
            value: serde_json::json!({ "position": position }),
        })
    })
}

/// An executor that records that it started and then never settles.
///
/// Entry is the signal a law waits on: the executor runs only after the claim
/// commits, so "entered" means the child's row exists and holds a lease. Without
/// it a crash law would race the claim and sometimes leave nothing behind to
/// drain.
fn blocking(entered: &Arc<AtomicUsize>) -> RuntimeEffectLocalExecutor<'static> {
    let entered = Arc::clone(entered);
    RuntimeEffectLocalExecutor::testing(move |_| async move {
        entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("a blocking child is never polled to completion")
    })
}

fn never() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async {
        std::future::pending::<()>().await;
        unreachable!("a never-settling child is never polled to completion")
    })
}

/// What a recording host answers for one child.
#[derive(Clone)]
enum ExecutorAnswer {
    /// Runs and produces an outcome immediately.
    Settle,
    /// This host cannot run the command.
    Refuse,
    /// Runs, reports that it started, and stays in the effect until the law
    /// releases it — which is how a law holds a *live claim* open for a known
    /// stretch, since a claim lives exactly as long as the execution under it.
    Hold {
        entered: Arc<AtomicUsize>,
        release: CancellationToken,
    },
}

/// A log of the replay keys whose effects actually ran, as distinct from the
/// replay keys a host was *asked* about.
type ExecutionLog = Arc<std::sync::Mutex<Vec<String>>>;

impl ExecutorAnswer {
    fn executor(
        &self,
        position: usize,
        replay_key: &str,
        executed: &ExecutionLog,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let executed = Arc::clone(executed);
        let replay_key = replay_key.to_string();
        let entry = move || executed.lock_recover().push(replay_key.clone());
        match self {
            Self::Settle => Some(RuntimeEffectLocalExecutor::testing(move |_| {
                entry();
                async move {
                    Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: serde_json::json!({ "position": position }),
                    })
                }
            })),
            Self::Refuse => None,
            Self::Hold { entered, release } => {
                let entered = Arc::clone(entered);
                let release = release.clone();
                Some(RuntimeEffectLocalExecutor::testing(move |_| {
                    entry();
                    let entered = Arc::clone(&entered);
                    let release = release.clone();
                    async move {
                        entered.fetch_add(1, Ordering::SeqCst);
                        release.cancelled().await;
                        Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: serde_json::json!({ "position": position }),
                        })
                    }
                }))
            }
        }
    }
}

/// The host wiring seam, instrumented.
///
/// Records the replay key of every child the drain asked about, in order, which
/// is how the laws state "exactly once": the sequence itself, not a count that
/// two runs of one child and none of another would also satisfy.
///
/// Answers are per group position, because the laws that put two drains in a
/// race need one host to hold one child while another host reaches for a
/// different one — a uniform answer cannot express that.
struct RecordingExecutors {
    asked: std::sync::Mutex<Vec<String>>,
    executed: ExecutionLog,
    by_position: Vec<ExecutorAnswer>,
    otherwise: ExecutorAnswer,
}

impl RecordingExecutors {
    fn uniform(answer: ExecutorAnswer) -> Arc<Self> {
        Arc::new(Self {
            asked: std::sync::Mutex::new(Vec::new()),
            executed: ExecutionLog::default(),
            by_position: Vec::new(),
            otherwise: answer,
        })
    }

    fn settling() -> Arc<Self> {
        Self::uniform(ExecutorAnswer::Settle)
    }

    /// A host that cannot run these commands at all.
    fn refusing() -> Arc<Self> {
        Self::uniform(ExecutorAnswer::Refuse)
    }

    fn by_position(by_position: Vec<ExecutorAnswer>, otherwise: ExecutorAnswer) -> Arc<Self> {
        Arc::new(Self {
            asked: std::sync::Mutex::new(Vec::new()),
            executed: ExecutionLog::default(),
            by_position,
            otherwise,
        })
    }

    fn invocations(&self) -> usize {
        self.asked.lock_recover().len()
    }

    /// The replay keys this host was *asked* about, sorted, with duplicates
    /// kept — an ask twice is exactly the defect these laws are looking for, so
    /// the multiset is the assertion and a deduplicated set would hide it.
    fn asked_about(&self) -> Vec<String> {
        let mut keys = self.asked.lock_recover().clone();
        keys.sort();
        keys
    }

    /// The replay keys whose effects this host actually *ran*, sorted, with
    /// duplicates kept.
    ///
    /// Distinct from [`asked_about`](Self::asked_about) because they can differ
    /// honestly: a pass asks for an executor and then loses the claim, so the ask
    /// happened and the execution did not. Exactly-once is a statement about
    /// this list.
    fn executions(&self) -> Vec<String> {
        let mut keys = self.executed.lock_recover().clone();
        keys.sort();
        keys
    }
}

impl GroupDrainExecutors for RecordingExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let replay_key = envelope
            .invocation
            .replay_key()
            .expect("a journaled child carries its replay key")
            .to_string();
        let position = envelope
            .group
            .as_ref()
            .map(|membership| membership.position)
            .expect("a drained child is a group member");
        self.asked.lock_recover().push(replay_key.clone());
        self.by_position
            .get(position)
            .unwrap_or(&self.otherwise)
            .executor(position, &replay_key, &self.executed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::effect::group_drain::DrainedChild;

    fn drained(replay_key: &str, outcome: ChildDrainOutcome) -> DrainedChild {
        DrainedChild {
            replay_key: replay_key.to_string(),
            outcome,
        }
    }

    /// Completeness is about the *queue*, not about what this pass managed to
    /// do: a child left under a live lease is still unsettled, and a group
    /// holding one is not reclaimable however successful the pass was.
    #[test]
    fn a_pass_is_complete_only_when_the_queue_was_empty() {
        let with_a_skip = GroupDrainReport {
            group_key: "g".to_string(),
            disposition: RUN,
            children: vec![
                drained("a", ChildDrainOutcome::Settled),
                drained("b", ChildDrainOutcome::LeaseLive { expires_at_ms: 1 }),
            ],
        };
        assert_eq!(with_a_skip.settled(), 1);
        assert!(!with_a_skip.is_complete());

        let empty = GroupDrainReport {
            group_key: "g".to_string(),
            disposition: RUN,
            children: Vec::new(),
        };
        assert_eq!(empty.settled(), 0);
        assert!(empty.is_complete());
    }

    /// Contested is not settled. A pass that could not win the claim must not
    /// count the child, or "how much is left" becomes unanswerable from the
    /// report.
    #[test]
    fn a_contested_child_is_not_counted_as_settled() {
        let report = GroupDrainReport {
            group_key: "g".to_string(),
            disposition: RUN,
            children: vec![
                drained("a", ChildDrainOutcome::Contested),
                drained("b", ChildDrainOutcome::NoExecutor),
                drained("c", ChildDrainOutcome::CancelDeclared),
            ],
        };
        assert_eq!(report.settled(), 0);
        assert!(!report.is_complete());
    }
}
