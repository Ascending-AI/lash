//! The observable semantics of the durable effect-group contract, asserted
//! against the in-memory reference host (FIG-1535, ADR 0065).
//!
//! Every test here is a statement about the *contract*, not about the native
//! tier: a SQL or engine host that disagrees with one of these is wrong, and one
//! that agrees and additionally survives a crash is conformant. The native substrate
//! is the right place to write them because it is the only one whose whole
//! behaviour is observable without a substrate — what it cannot claim, and what
//! is therefore deliberately absent here, is cross-process durability.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Barrier, oneshot};
use tokio_util::sync::CancellationToken;

use crate::conformance::StagedGroupExecutors;
use crate::*;
use crate::{EffectGroupHandle, GroupWakePolicy, LoserPolicy, RuntimeEffectGroup};
use pretty_assertions::assert_eq;

const SCOPE: &str = "fig1535-session";

fn child(key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(
                ExecutionScope::runtime_operation(SCOPE),
                format!("{key}:child:{position}"),
            )
            .expect("valid child effect address"),
            RuntimeAttribution::none(),
            "effect",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 0 },
        },
    )
}

fn group(
    key: &str,
    children: usize,
    wake: GroupWakePolicy,
    disposition: LoserPolicy,
) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(
                ExecutionScope::runtime_operation(SCOPE),
                format!("{key}:group"),
            )
            .expect("valid group effect address"),
            RuntimeAttribution::none(),
            "group",
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

fn controller() -> NativeRuntimeEffectController {
    let controller = NativeRuntimeEffectController::default();
    controller
        .register_group_executors(executors() as Arc<dyn crate::GroupExecutors>)
        .expect("a fresh controller has no resolver yet");
    controller
}

/// An native host whose controller resolves grouped children through this
/// file's staging table.
fn native_host() -> crate::NativeEffectHost {
    crate::NativeEffectHost::with_native_controller(Arc::new(controller()))
}

/// The capability flag is the admission gate, and the scoped view a host
/// actually reaches groups through must answer it the same way.
///
/// A scoped wrapper that forwarded the flag but left the methods on their
/// fail-closed defaults would advertise support and then refuse every group, so
/// the flag is asserted through the same object that runs the group.
#[tokio::test]
async fn the_native_substrate_supports_groups_through_the_scoped_host_view() {
    use crate::EffectHost;

    let host = native_host();
    let scoped = host
        .scoped(admit(crate::ExecutionScope::runtime_operation(SCOPE)))
        .expect("scoped controller");
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

/// ADR 0099 §14's "unclaimed tasks are counted" on this tier: a group closed
/// under `RunToCompletion` keeps its draining loser counted against the scope
/// until the loser settles, which is what a quiescent retirement reads.
#[tokio::test]
async fn a_draining_loser_is_counted_until_it_settles() {
    let controller = Arc::new(controller());
    let host = crate::NativeEffectHost::with_native_controller(Arc::clone(&controller));
    let scope = crate::ExecutionScope::runtime_operation(SCOPE);
    let scoped = host.scoped(admit(scope.clone())).expect("a scope binds");
    let key = "fig2266:draining-counted";
    let (slow, loser) = gated();
    let mut handle = scoped
        .controller()
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![immediate(), slow],
        ))
        .await
        .expect("the group opens");

    let settlement = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");
    assert_eq!(settlement.position, 0);
    scoped
        .controller()
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes under RunToCompletion");

    assert_eq!(
        controller.unsettled_children_under(&scope),
        1,
        "the draining loser is still counted after the caller closed"
    );
    loser.release.send(()).expect("release the loser");
    until(|| controller.unsettled_children_under(&scope) == 0).await;
}

/// The supervisor owns its children's tasks for exactly the group's life:
/// both live in the group's task set while the loser drains, and the set is
/// gone once the last settlement reaps the group — never aborted mid-drain.
#[tokio::test]
async fn the_supervisor_owns_its_children_for_the_groups_life() {
    let controller = Arc::new(controller());
    let host = crate::NativeEffectHost::with_native_controller(Arc::clone(&controller));
    let scoped = host
        .scoped(admit(crate::ExecutionScope::runtime_operation(SCOPE)))
        .expect("a scope binds");
    let key = "fig2266:supervisor-life";
    let (slow, loser) = gated();
    let mut handle = scoped
        .controller()
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![immediate(), slow],
        ))
        .await
        .expect("the group opens");

    let settlement = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");
    assert_eq!(settlement.position, 0);
    assert_eq!(
        controller.open_group_task_count(key),
        Some(2),
        "the group still owns both child tasks while the loser drains"
    );
    scoped
        .controller()
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");

    loser.release.send(()).expect("release the loser");
    until(|| controller.open_group_task_count(key).is_none()).await;
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
