//! The observable semantics of the durable effect-group contract, asserted
//! against the in-memory reference host (FIG-1535, ADR 0065).
//!
//! Every test here is a statement about the *contract*, not about the inline
//! tier: a SQL or engine host that disagrees with one of these is wrong, and one
//! that agrees and additionally survives a crash is conformant. The inline tier
//! is the right place to write them because it is the only one whose whole
//! behaviour is observable without a substrate — what it cannot claim, and what
//! is therefore deliberately absent here, is cross-process durability.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Barrier, oneshot};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::testing::conformance::StagedGroupExecutors;
use crate::{EffectGroupHandle, GroupWakePolicy, LoserPolicy, RuntimeEffectGroup};

const SCOPE: &str = "fig1535-session";

fn child(key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            crate::RuntimeScope::new(SCOPE),
            "effect",
            RuntimeEffectKind::Sleep,
            format!("{key}:child:{position}"),
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 0 },
    )
}

fn group(
    key: &str,
    children: usize,
    wake: GroupWakePolicy,
    disposition: LoserPolicy,
) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            crate::RuntimeScope::new(SCOPE),
            "group",
            RuntimeEffectKind::Sleep,
            format!("{key}:group"),
        ),
        key,
        (0..children).map(|position| child(key, position)).collect(),
        wake,
        disposition,
    )
    .expect("a group with at least one child assembles")
}

/// The resolver every host in this file is registered with.
///
/// Since FIG-1578 a group carries envelopes and nothing else: what runs a child
/// is the resolver its host was registered with. A test stages the executors it
/// wants under the children's replay keys and opens the group `staged` hands
/// back. One table for the file is safe — every test namespaces its own group
/// key, and a test's *second* host must answer the same routing question as its
/// first without inheriting the first's memory.
fn executors() -> Arc<StagedGroupExecutors> {
    static EXECUTORS: std::sync::OnceLock<Arc<StagedGroupExecutors>> = std::sync::OnceLock::new();
    Arc::clone(EXECUTORS.get_or_init(|| Arc::new(StagedGroupExecutors::new())))
}

fn staged(
    group: RuntimeEffectGroup,
    executors_for_children: Vec<RuntimeEffectLocalExecutor<'static>>,
) -> RuntimeEffectGroup {
    executors().stage(group, executors_for_children)
}

/// An executor that settles as soon as it is polled.
fn immediate() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async { Ok(RuntimeEffectOutcome::Sleep) })
}

/// An executor that fails as soon as it is polled, so a settlement's `outcome`
/// can be read as well as its rank.
fn immediate_failure() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
            "this child rejects",
        ))
    })
}

/// An executor gated on a release signal, plus the handles a test needs to
/// release it and to observe whether it ever finished.
struct GatedChild {
    release: oneshot::Sender<()>,
    finished: Arc<AtomicUsize>,
}

fn gated() -> (RuntimeEffectLocalExecutor<'static>, GatedChild) {
    let (release, released) = oneshot::channel();
    let finished = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&finished);
    let executor = RuntimeEffectLocalExecutor::testing(move |_| async move {
        // A dropped receiver means the child was cancelled before its release,
        // which must not be reported as a completion.
        if released.await.is_ok() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        Ok(RuntimeEffectOutcome::Sleep)
    });
    (executor, GatedChild { release, finished })
}

/// An executor that never settles, used to keep a group incomplete so its
/// record stays observable while the test reads it.
fn never() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async {
        std::future::pending::<()>().await;
        Ok(RuntimeEffectOutcome::Sleep)
    })
}

/// Waits for a condition the host reaches on its own tasks, so a test never
/// depends on how many yields a settlement happens to take.
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the host reaches the awaited state");
}

fn controller() -> InlineRuntimeEffectController {
    let controller = InlineRuntimeEffectController::default();
    controller
        .register_group_executors(executors() as Arc<dyn crate::GroupExecutors>)
        .expect("a fresh controller has no resolver yet");
    controller
}

/// An inline host whose controller resolves grouped children through this
/// file's staging table.
fn inline_host() -> crate::InlineEffectHost {
    crate::InlineEffectHost::new(Arc::new(controller()))
}

/// The capability flag is the admission gate, and the scoped view a host
/// actually reaches groups through must answer it the same way.
///
/// A scoped wrapper that forwarded the flag but left the methods on their
/// fail-closed defaults would advertise support and then refuse every group, so
/// the flag is asserted through the same object that runs the group.
#[tokio::test]
async fn the_inline_tier_supports_groups_through_the_scoped_host_view() {
    use crate::EffectHost;

    let host = inline_host();
    let scoped = host
        .scoped(crate::ExecutionScope::runtime_operation("fig1535-scoped"))
        .expect("scoped controller");
    assert!(
        scoped.controller().supports_effect_groups(),
        "the scoped view must report the group support its own methods provide"
    );

    let key = "fig1535:scoped";
    let mut handle = scoped
        .controller()
        .open_effect_group(staged(
            group(key, 1, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            vec![immediate()],
        ))
        .await
        .expect("a group opens through the scoped view");
    let settlement = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the scoped view serves the settlement rather than refusing it");
    assert_eq!(settlement.position, 0);
    scoped
        .controller()
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the scoped view closes the group");
}

/// The in-memory reference host answers the shared host suite, which is what
/// makes "the SQL tiers agree with the reference" a checkable claim rather than
/// two files of similar-looking tests (FIG-1564).
///
/// The laws in this file stay where they are: they reach this tier's own
/// settlement record, which is how the *allocator* is asserted, and the shared
/// suite deliberately cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_inline_host_satisfies_the_shared_effect_group_suite() {
    // One host, handed out repeatedly: the suite's factory stands for "another
    // view of the same substrate", and on a tier whose substrate *is* the
    // process, that is this object. A fresh `InlineEffectHost` per call would be
    // a different substrate, which is the one thing the factory may not be.
    //
    // The suite hands its own resolver to every factory call, and registering it
    // is what makes this host support groups at all.
    //
    // `None` asks for the unregistered host two laws are about, and on this tier
    // that has to be a *fresh* controller: registration is a property of the
    // substrate here rather than of a connection to it, so "the same substrate,
    // unwired" does not exist. The cost is only to the second of those laws — "a
    // refused open journals nothing" — and it is no cost at all, because this
    // tier journals nothing to leave behind.
    let controller = Arc::new(InlineRuntimeEffectController::default());
    let host: std::sync::Arc<dyn crate::EffectHost> = std::sync::Arc::new(
        crate::InlineEffectHost::new(Arc::clone(&controller) as Arc<dyn RuntimeEffectController>),
    );
    crate::testing::conformance::effect_group_host_conformance(move |suite_executors| {
        let Some(suite_executors) = suite_executors else {
            return std::sync::Arc::new(crate::InlineEffectHost::new(Arc::new(
                InlineRuntimeEffectController::default(),
            )
                as Arc<dyn RuntimeEffectController>))
                as std::sync::Arc<dyn crate::EffectHost>;
        };
        controller
            .register_group_executors(suite_executors)
            .expect("the suite registers one resolver on this host");
        std::sync::Arc::clone(&host)
    })
    .await;
}

/// First-settlement wake: the caller resumes on the winner while the loser is
/// still running.
///
/// Asserted on observed resume ordering rather than on wall time — the loser is
/// held on an explicit gate, so "the caller did not wait for it" is a fact about
/// the host and not about the scheduler's speed.
#[tokio::test]
async fn the_first_settlement_wakes_the_caller_while_the_loser_still_runs() {
    let controller = controller();
    let key = "fig1535:race";
    let (slow, loser) = gated();
    let mut handle = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![slow, immediate()],
        ))
        .await
        .expect("the group opens");

    let settlement = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");
    assert_eq!(
        settlement.position, 1,
        "the child that settled first must be the one delivered, whatever its \
         input position"
    );
    assert_eq!(
        settlement.sequence, 1,
        "the winner holds the first sequence"
    );
    assert_eq!(handle.consumed(), 1);
    assert_eq!(
        loser.finished.load(Ordering::SeqCst),
        0,
        "the caller resumed on the winner, so the loser cannot have completed"
    );
}

/// The settlement at rank `n` is a record, not a race: reading it again — as a
/// replayed frame does — yields the same child.
///
/// The re-read goes through `EffectGroupHandle::restored`, which is the path a
/// durable continuation actually arrives on, so this pins the reopen rule too:
/// the caller's cursor wins, and the host keeps no consumption state of its own.
#[tokio::test]
async fn settlement_n_is_stable_across_re_reads() {
    let controller = controller();
    let key = "fig1535:stable";
    let (slow, loser) = gated();
    let mut handle = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![slow, immediate()],
        ))
        .await
        .expect("the group opens");

    let first = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");

    // Let the loser settle, so a host that served "the next unread settlement"
    // rather than rank `consumed + 1` has a different answer available.
    loser.release.send(()).expect("release the loser");
    until(|| controller.recorded_group_settlements(key).len() == 2).await;

    let mut replayed = EffectGroupHandle::restored(key, 2, 0).expect("a cursor at 0 restores");
    let re_read = controller
        .await_next_settlement(&mut replayed, CancellationToken::new())
        .await
        .expect("the replayed frame re-reads rank 1");
    assert_eq!(
        (re_read.position, re_read.sequence),
        (first.position, first.sequence),
        "rank 1 is a decided fact; a replay must observe the same child at it"
    );

    let second = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the second settlement arrives");
    assert_eq!(
        second.sequence, 2,
        "sequences are monotonic in rank order, so rank 2 follows rank 1"
    );
    assert_ne!(second.position, first.position);
}

/// Concurrent settlement allocates one sequence per child: no two children ever
/// share a rank, and the record's order is the sequence order.
///
/// This is the in-memory half of the allocator ADR 0065 makes normative. A
/// read-then-max allocator passes every other test in this file and fails this
/// one, which is the whole reason it is written at a width where siblings settle
/// together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn siblings_settling_together_get_distinct_sequences() {
    let controller = controller();
    let key = "fig1535:concurrent";
    let width = 16;
    // A barrier rather than a notification: every child must have arrived before
    // any of them is released, so the settlements really do land together. A
    // broadcast wake can be missed by a child that has not parked yet, which
    // would leave the group short a settlement instead of racing it.
    let start = Arc::new(Barrier::new(width));
    let executors = (0..width)
        .map(|_| {
            let start = Arc::clone(&start);
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                start.wait().await;
                Ok(RuntimeEffectOutcome::Sleep)
            })
        })
        .collect::<Vec<_>>();
    let mut handle = controller
        .open_effect_group(staged(
            group(
                key,
                width,
                GroupWakePolicy::All,
                LoserPolicy::RunToCompletion,
            ),
            executors,
        ))
        .await
        .expect("the group opens");

    let mut sequences = Vec::new();
    let mut positions = Vec::new();
    while !handle.is_exhausted() {
        let settlement = controller
            .await_next_settlement(&mut handle, CancellationToken::new())
            .await
            .expect("every child settles");
        sequences.push(settlement.sequence);
        positions.push(settlement.position);
    }
    assert_eq!(sequences.len(), width);
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "delivering by rank must yield strictly increasing sequences, not {sequences:?}"
    );
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(
        positions.len(),
        width,
        "each child settles exactly once, so no position may be delivered twice"
    );
    assert!(
        handle.is_exhausted(),
        "a caller that consumed every child is exhausted by its own arithmetic"
    );
}

/// Awaiting past the last child is refused rather than hanging or clamping.
#[tokio::test]
async fn awaiting_past_the_last_child_is_refused() {
    let controller = controller();
    let key = "fig1535:exhausted";
    let mut handle = controller
        .open_effect_group(staged(
            group(key, 1, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            vec![immediate()],
        ))
        .await
        .expect("the group opens");
    controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the only child settles");

    let error = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect_err("there is no rank 2 on a one-child group");
    assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
    assert_eq!(
        handle.consumed(),
        1,
        "a refused await leaves the cursor of record where it was"
    );
}

/// A cancelled await leaves both cursors alone — the caller's and the group's —
/// so the settlement it did not deliver is still the next one served.
#[tokio::test]
async fn a_cancelled_await_leaves_the_rank_to_be_read_again() {
    let controller = controller();
    let key = "fig1535:cancelled-await";
    let (gate, child) = gated();
    let mut handle = controller
        .open_effect_group(staged(
            group(key, 1, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![gate],
        ))
        .await
        .expect("the group opens");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = controller
        .await_next_settlement(&mut handle, cancel)
        .await
        .expect_err("a cancelled await does not deliver");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
        "cancellation is its own named outcome, not a group-shape failure"
    );
    assert_eq!(handle.consumed(), 0);

    child.release.send(()).expect("release the child");
    let settlement = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the settlement the cancelled await did not take is still owed");
    assert_eq!((settlement.position, settlement.sequence), (0, 1));
}

/// `RunToCompletion`: the losers keep running under host ownership after the
/// caller has closed, and their settlements land behind the winner's.
///
/// The third child never settles, which is what keeps the group's record
/// readable while the test reads it — the assertion is about the losers, not
/// about retention.
#[tokio::test]
async fn run_to_completion_losers_settle_after_the_caller_is_gone() {
    let controller = controller();
    let key = "fig1535:run-to-completion";
    let (slow, loser) = gated();
    let mut handle = controller
        .open_effect_group(staged(
            group(key, 3, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![immediate(), slow, never()],
        ))
        .await
        .expect("the group opens");
    let winner = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the winner settles");
    assert_eq!(winner.position, 0);

    controller
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the caller closes and moves on");

    loser.release.send(()).expect("release the loser");
    until(|| loser.finished.load(Ordering::SeqCst) == 1).await;
    until(|| controller.recorded_group_settlements(key).len() == 2).await;

    let recorded = controller.recorded_group_settlements(key);
    assert_eq!(
        recorded,
        vec![(0, 1, true), (1, 2, true)],
        "the loser ran to completion under host ownership and journaled behind \
         the winner"
    );
}

/// `Cancel`: every child that had not settled holds a cancellation as its
/// terminal, and the cancelled child never completes.
#[tokio::test]
async fn cancel_gives_every_unsettled_child_a_cancellation_terminal() {
    let controller = controller();
    let key = "fig1535:cancel";
    let (slow, loser) = gated();
    let mut handle = controller
        .open_effect_group(staged(
            group(key, 3, GroupWakePolicy::First, LoserPolicy::Cancel),
            vec![immediate(), slow, never()],
        ))
        .await
        .expect("the group opens");
    controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the winner settles");

    controller
        .close_effect_group(handle, LoserPolicy::Cancel)
        .await
        .expect("the caller closes under the declared disposition");

    let recorded = controller.recorded_group_settlements(key);
    assert_eq!(
        recorded,
        vec![(0, 1, true), (1, 2, false), (2, 3, false)],
        "a cancelled group's every child holds a terminal, the winner's its own \
         outcome and each loser's its cancellation"
    );
    assert_eq!(
        loser.finished.load(Ordering::SeqCst),
        0,
        "a cancelled child must not go on to complete"
    );
    // The released signal arrives after the cancellation, and must not seat a
    // second settlement for a child that already holds a terminal.
    let _ = loser.release.send(());
    tokio::task::yield_now().await;
    assert_eq!(controller.recorded_group_settlements(key).len(), 3);
}

/// Close may narrow the declared disposition and may never widen it, in either
/// direction of arrival: declared, then cumulatively.
#[tokio::test]
async fn close_may_narrow_but_never_widen() {
    let controller = controller();

    let declared_cancel = "fig1535:declared-cancel";
    let handle = controller
        .open_effect_group(staged(
            group(
                declared_cancel,
                1,
                GroupWakePolicy::First,
                LoserPolicy::Cancel,
            ),
            vec![never()],
        ))
        .await
        .expect("the group opens");
    let widened = controller
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect_err("a declared Cancel may not be closed as RunToCompletion");
    assert_eq!(
        widened.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "widening is a shape refusal: a crash-drain would apply the declared \
         disposition, so the losers' fate cannot depend on reaching the close"
    );
    assert!(
        controller
            .recorded_group_settlements(declared_cancel)
            .is_empty(),
        "a refused close applies nothing, so the group is untouched"
    );

    // Narrowing runs the other way, and it is resolved against the disposition
    // in force rather than the declared one: a group closed once as declared is
    // still narrowable, and the narrowing settles the children the first close
    // had left running.
    let declared_run = "fig1535:declared-run";
    let handle = controller
        .open_effect_group(staged(
            group(
                declared_run,
                1,
                GroupWakePolicy::First,
                LoserPolicy::RunToCompletion,
            ),
            vec![never()],
        ))
        .await
        .expect("the group opens");
    controller
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("closing as declared releases the caller's interest");
    assert!(
        controller
            .recorded_group_settlements(declared_run)
            .is_empty(),
        "a RunToCompletion close settles nothing by itself; the child is still \
         running under host ownership"
    );

    let replayed = EffectGroupHandle::restored(declared_run, 1, 0).expect("a handle restores");
    controller
        .close_effect_group(replayed, LoserPolicy::Cancel)
        .await
        .expect("a caller that has learned it no longer wants the losers may narrow");
    assert_eq!(
        controller.recorded_group_settlements(declared_run),
        vec![(0, 1, false)],
        "narrowing to Cancel gives the still-running child its cancellation as \
         its terminal, which is what a later close has to have applied"
    );

    // The reverse ordering is not reachable on this tier and is deliberately not
    // asserted: a Cancel close settles every remaining child, so the group is
    // complete and reaped, and a replayed close of a group with no losers left
    // to decide succeeds rather than raising on a healthy replay path.
}

/// Close is idempotent under the same disposition, because a crash between a
/// successful close and the continuation commit replays it.
#[tokio::test]
async fn closing_twice_under_one_disposition_succeeds() {
    let controller = controller();
    let key = "fig1535:idempotent-close";
    let handle = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::Cancel),
            vec![never(), never()],
        ))
        .await
        .expect("the group opens");
    controller
        .close_effect_group(handle, LoserPolicy::Cancel)
        .await
        .expect("the first close applies the disposition");
    let replayed = EffectGroupHandle::restored(key, 2, 0).expect("a handle restores");
    controller
        .close_effect_group(replayed, LoserPolicy::Cancel)
        .await
        .expect("a replayed frame closing the same group again must not raise");
}

/// A closed group serves the caller nothing further, whichever disposition it
/// closed under.
#[tokio::test]
async fn a_closed_group_serves_its_caller_no_further_settlements() {
    let controller = controller();
    let key = "fig1535:closed";
    let handle = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            vec![immediate(), never()],
        ))
        .await
        .expect("the group opens");
    controller
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the caller closes");

    let mut replayed = EffectGroupHandle::restored(key, 2, 0).expect("a handle restores");
    let error = controller
        .await_next_settlement(&mut replayed, CancellationToken::new())
        .await
        .expect_err("a closed group is closed to its caller");
    assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
    assert_eq!(replayed.consumed(), 0);
}

/// A reopen is fenced on group shape and dispatches nothing.
///
/// Both halves matter: a shrunk child vec under one key renumbers every rank
/// above the change, and a reopen that re-dispatched would double every side
/// effect the first dispatch is still producing.
#[tokio::test]
async fn a_reopen_is_fenced_on_shape_and_runs_no_child_twice() {
    let controller = controller();
    let key = "fig1535:reopen";
    let runs = Arc::new(AtomicUsize::new(0));
    let executors = (0..2)
        .map(|_| {
            let runs = Arc::clone(&runs);
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(RuntimeEffectOutcome::Sleep)
            })
        })
        .collect::<Vec<_>>();
    let handle = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            executors,
        ))
        .await
        .expect("the group opens");
    until(|| runs.load(Ordering::SeqCst) == 2).await;
    assert_eq!(handle.children(), 2);
    assert_eq!(handle.group_key(), key);

    let narrowed = controller
        .open_effect_group(staged(
            group(key, 1, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            vec![immediate()],
        ))
        .await
        .expect_err("a reopen with fewer children is refused");
    assert_eq!(
        narrowed.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape
    );

    let rewaked = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![immediate(), immediate()],
        ))
        .await
        .expect_err("a reopen under a different wake rule is refused");
    assert_eq!(
        rewaked.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape
    );

    let redeclared = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::All, LoserPolicy::Cancel),
            vec![immediate(), immediate()],
        ))
        .await
        .expect_err("a reopen declaring a different disposition is refused");
    assert_eq!(
        redeclared.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape
    );

    let reopened = controller
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            vec![immediate(), immediate()],
        ))
        .await
        .expect("a reopen of the same shape is accepted");
    assert_eq!(
        reopened.consumed(),
        0,
        "open always returns a fresh cursor; only the caller knows how far it \
         consumed, and it restores that itself"
    );
    tokio::task::yield_now().await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "a reopen must not dispatch a second copy of children already running \
         under host ownership"
    );
}

/// The wake rule is journaled identity, not host behaviour: the host serves
/// settlements by rank under every rule, and which one ends the caller's loop is
/// the caller's decision.
///
/// Written against `FirstSuccess`, the rule most likely to tempt a host into
/// filtering: a host that skipped the failed child would make its outcome
/// unreachable through the only method that reports it, and `any`'s accumulated
/// failures are program-visible.
#[tokio::test]
async fn the_wake_rule_is_identity_and_the_host_filters_nothing() {
    let controller = controller();
    let key = "fig1535:first-success";
    let (slow, success) = gated();
    let mut handle = controller
        .open_effect_group(staged(
            group(
                key,
                2,
                GroupWakePolicy::FirstSuccess,
                LoserPolicy::RunToCompletion,
            ),
            vec![immediate_failure(), slow],
        ))
        .await
        .expect("the group opens");

    let first = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");
    assert_eq!(first.position, 0);
    let error = first
        .outcome
        .expect_err("the first child rejected, and the host reports that verbatim");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
        "a failed settlement carries its own error, so `any` can accumulate it"
    );

    success
        .release
        .send(())
        .expect("release the successful arm");
    let second = controller
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the successful arm settles at rank 2");
    assert_eq!(second.position, 1);
    assert!(second.outcome.is_ok());
}

/// A close racing its own children's settlements seats exactly one terminal per
/// child.
///
/// This is the window the position-already-settled guard in `record` exists for:
/// `close(Cancel)` writes a cancellation terminal for every position that has not
/// settled *yet*, and a child that was already inside its executor can come back
/// with a real outcome a moment later. Without the guard that child holds two
/// ranks, and a group of `n` children reports more than `n` settlements — which
/// is the same defect a durable host would commit by upserting a settlement row
/// twice. The window is narrow, so this is written as a stress: several
/// iterations, several children, and the close aimed at the moment the children
/// are settling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_racing_its_children_seats_one_terminal_per_child() {
    let width = 8;
    for iteration in 0..24 {
        let controller = controller();
        let key = format!("fig1535:race-close:{iteration}");
        // Every child arrives at the barrier and is released together, so the
        // settlements land in the same instant the caller closes.
        let start = Arc::new(Barrier::new(width));
        let executors = (0..width)
            .map(|_| {
                let start = Arc::clone(&start);
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    start.wait().await;
                    Ok(RuntimeEffectOutcome::Sleep)
                })
            })
            .collect::<Vec<_>>();
        let handle = controller
            .open_effect_group(staged(
                group(&key, width, GroupWakePolicy::All, LoserPolicy::Cancel),
                executors,
            ))
            .await
            .expect("the group opens");

        // Yield a varying number of times before closing, so the close lands at
        // different points of the children's settlement across iterations.
        for _ in 0..(iteration % 6) {
            tokio::task::yield_now().await;
        }
        controller
            .close_effect_group(handle, LoserPolicy::Cancel)
            .await
            .expect("closing under the declared disposition");

        let recorded = controller.recorded_group_settlements(&key);
        assert_eq!(
            recorded.len(),
            width,
            "a group of {width} children holds {width} terminals, not {}: a child \
             that settled inside the cancellation window must not also take a \
             cancellation rank (iteration {iteration}, record {recorded:?})",
            recorded.len()
        );
        let mut positions = recorded
            .iter()
            .map(|(position, _, _)| *position)
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        assert_eq!(
            positions.len(),
            width,
            "each child holds exactly one terminal (iteration {iteration}, \
             record {recorded:?})"
        );
        // Recorded order *is* sequence order: the sequence is allocated under the
        // same lock that pushes the settlement, so a reader that walks the record
        // by rank walks it by sequence too. Positions are arbitrary here — which
        // child won is a race — but the sequences they were allocated cannot be.
        let sequences_in_record = recorded
            .iter()
            .map(|(_, sequence, _)| *sequence)
            .collect::<Vec<_>>();
        assert!(
            sequences_in_record.windows(2).all(|pair| pair[0] < pair[1]),
            "recorded order must be sequence order (iteration {iteration}, \
             sequences {sequences_in_record:?})"
        );
        let mut sequences = sequences_in_record;
        sequences.sort_unstable();
        sequences.dedup();
        assert_eq!(
            sequences.len(),
            width,
            "no two terminals share a sequence (iteration {iteration}, \
             record {recorded:?})"
        );
    }
}
