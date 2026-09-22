//! The durable-closing laws of ADR 0099 §7 (FIG-3410, crash windows W9–W12):
//! closing is a fact recorded on the group row *before* any cancel decision is
//! issued, and finalization is a four-step sequence resumed at the first
//! incomplete step after a crash.
//!
//! These are the durable half of the group contract: where
//! [`super::effect_group_host`] asserts what a caller can observe, this suite
//! asserts what the journal holds — the `closing` row itself, the cursor it
//! carries, and the bound the drain budget puts on waiting — through the
//! [`StoreEffectGroupClosing`] seam every SQL host hands out beside its drain.
//!
//! # Tier shape
//!
//! The suite is store-tier-scoped for the same reason the drain suite is
//! (FIG-2272): the laws are typed on the closing seam, which exists once over
//! the store-backed effect-replay driver. Restate holds no group row — its
//! engine-side `EffectGroupIndex` `Closed`/`Retired` states are the twin — so
//! [`EffectHost::effect_group_closing`] answers `None` there and the suite is
//! not registered, the way `tool_child_invocation` treats a `None` drain. The
//! native tier answers the same vocabulary against its in-memory group table,
//! but a crashed-runtime law has no journal to leave `closing` in, so the
//! crash windows are asserted here rather than through it.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::effect_group_drain::{
    CRASH_LEASE_MS, DrainWorld, DrainWorldFactory, LIVE_LEASE_MS, RecordingExecutors, blocking,
    close, crashed_process, group_key, never, next, open, scope, settles, spec, spec_with_budget,
    until,
};
use super::helpers::admit;
use crate::{
    LoserPolicy, RuntimeEffectControllerError, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_core::testing::conformance_support::{
    EffectGroupLifecycle, GroupFinalizationReport, GroupOnlyFinalization, OpenerFinalizationSteps,
    StoreEffectGroupClosing,
};
use pretty_assertions::assert_eq;

const RUN: LoserPolicy = LoserPolicy::RunToCompletion;
const CANCEL: LoserPolicy = LoserPolicy::Cancel;

/// The closing seam a store-tier host hands out over the same journal it
/// serves groups through.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the suite is registered only on tiers that answer the seam"
)]
fn closing_seam(world: &DrainWorld) -> Arc<dyn StoreEffectGroupClosing> {
    world
        .host
        .effect_group_closing()
        .expect("a store-tier effect host answers the §7 closing seam")
}

/// The durable lifecycle read, unwrapped: every law below has opened the group
/// before it asks.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the group was opened by the setup above"
)]
async fn lifecycle(
    closing: &Arc<dyn StoreEffectGroupClosing>,
    group_key: &str,
) -> EffectGroupLifecycle {
    closing
        .read_group_lifecycle(group_key)
        .await
        .expect("the lifecycle read answers")
        .expect("a group the suite opened is journaled")
}

/// Wait until `rank` is durably seated in the group — the barrier a crash
/// law needs before the runtime dies: a rank still in flight at the crash is
/// legitimately drained again by the resume's step 1, so "never re-drains"
/// holds only for settlements the journal already held.
async fn until_rank_seated(scoped: &crate::ScopedEffectController<'_>, group_key: &str, rank: u64) {
    let controller = scoped.controller();
    let group_key = group_key.to_string();
    until_async(move || {
        let group_key = group_key.clone();
        async move {
            matches!(
                controller.read_group_settlement(&group_key, rank).await,
                Ok(Some(_))
            )
        }
    })
    .await;
}

/// Wait until the group's durable lifecycle reaches `settled`.
async fn until_settled(closing: &Arc<dyn StoreEffectGroupClosing>, group_key: &str) {
    let closing = Arc::clone(closing);
    let group_key = group_key.to_string();
    until_async(move || {
        let closing = Arc::clone(&closing);
        let group_key = group_key.clone();
        async move {
            matches!(
                closing.read_group_lifecycle(&group_key).await,
                Ok(Some(EffectGroupLifecycle::Settled { .. }))
            )
        }
    })
    .await;
}

/// `until` over an async predicate — the durable lifecycle lives behind a read,
/// so a law cannot ask the question synchronously.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the host reaches the awaited state"
)]
async fn until_async<Fut: Future<Output = bool> + Send>(mut condition: impl FnMut() -> Fut + Send) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the host reaches the awaited state");
}

/// Whether an opener step fails when it is called.
enum Injection {
    /// Never fails.
    Never,
    /// Fails every call — the probe a crash law polls with, where `Err` is the
    /// signal that the step before it has recorded.
    Always,
}

impl Injection {
    /// Runs the failure, if any, for one call of `step`.
    fn run(&self, step: &'static str) -> Result<(), RuntimeEffectControllerError> {
        let fail = match self {
            Self::Never => false,
            Self::Always => true,
        };
        if fail {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!("injected crash in {step}"),
            ));
        }
        Ok(())
    }
}

/// The opener's steps 2 and 3, instrumented.
///
/// Every call is counted, and a step fails as its `Injection` says — the
/// injected crash that lets a law park the cursor exactly between two steps.
/// The counts are what "a recorded step is never re-run" is asserted on: a
/// resume may not call a step the cursor already records.
pub struct RecordingFinalization {
    outcome_calls: AtomicUsize,
    parent_calls: AtomicUsize,
    outcome: Injection,
    parent: Injection,
}

impl RecordingFinalization {
    fn new(outcome: Injection, parent: Injection) -> Self {
        Self {
            outcome_calls: AtomicUsize::new(0),
            parent_calls: AtomicUsize::new(0),
            outcome,
            parent,
        }
    }

    fn outcome_calls(&self) -> usize {
        self.outcome_calls.load(Ordering::SeqCst)
    }

    fn parent_calls(&self) -> usize {
        self.parent_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl OpenerFinalizationSteps for RecordingFinalization {
    async fn commit_outcome_and_accounting(
        &self,
        _group_key: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.outcome_calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.run("commit_outcome_and_accounting")
    }

    async fn record_parent_end(
        &self,
        _group_key: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.parent_calls.fetch_add(1, Ordering::SeqCst);
        self.parent.run("record_parent_end")
    }
}

/// An executor that parks on an explicit release and ignores everything else —
/// the law-facing answer to "a child whose attempt body does not return".
///
/// `started` proves the body ran; `finished` proves it returned. A law reads
/// `finished == 0` at a `settled` lifecycle to show that what seated the rank
/// was the durable decision, never the body's return.
struct ParkedChild {
    release: CancellationToken,
    started: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

impl ParkedChild {
    fn release(&self) {
        self.release.cancel();
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
}

fn parked(position: usize) -> (RuntimeEffectLocalExecutor<'static>, ParkedChild) {
    let release = CancellationToken::new();
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let child = ParkedChild {
        release: release.clone(),
        started: Arc::clone(&started),
        finished: Arc::clone(&finished),
    };
    let executor = RuntimeEffectLocalExecutor::testing(move |_| {
        let release = release.clone();
        let started = Arc::clone(&started);
        let finished = Arc::clone(&finished);
        async move {
            started.fetch_add(1, Ordering::SeqCst);
            release.cancelled().await;
            finished.fetch_add(1, Ordering::SeqCst);
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "position": position }),
            })
        }
    });
    (executor, child)
}

/// The `closing` phase's cursor, or `None` for any other phase.
fn completed_steps(lifecycle: &EffectGroupLifecycle) -> Option<u8> {
    match lifecycle {
        EffectGroupLifecycle::Closing { finalized, .. } => Some(finalized.completed()),
        _ => None,
    }
}

/// W9 — `closing` is durable before any cancel decision is issued (ADR 0099
/// §7's first sentence), and a resuming host finalizes the recorded row
/// without re-deciding anything.
///
/// The ordering claim is observable rather than internal: a `close` under
/// `Cancel` returns only after the `closing` row holds the effective
/// disposition, so the read right after `close` can never see `live`, and the
/// loser's rank is already seated by a decision — a second host reads that
/// terminal back and finalizes the group without re-running or re-deciding a
/// child.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn closing_is_recorded_before_any_cancel_is_issued(make: DrainWorldFactory) {
    let prefix = format!("closing-w9-{}", uuid::Uuid::new_v4().simple());
    let executors = RecordingExecutors::settling();
    let world = make(spec(LIVE_LEASE_MS, &executors)).await;
    let closing = closing_seam(&world);
    let scoped = world
        .host
        .scoped(admit(scope(&prefix, "w9")))
        .expect("a scope binds");
    let key = group_key(&prefix, "w9");
    let (loser_executor, loser) = parked(1);
    let mut handle = open(&scoped, &key, 2, RUN, vec![settles(0), loser_executor]).await;
    until(|| loser.started() == 1).await;
    let winner = next(&scoped, &mut handle)
        .await
        .expect("the settling child takes rank 1");

    // The close narrows a `RunToCompletion` open to `Cancel` — and the durable
    // write is the whole law: by the time it returns the row must hold
    // `closing` with the effective disposition, ahead of every decision.
    close(&scoped, handle, CANCEL)
        .await
        .expect("the close records `closing` and returns");
    match lifecycle(&closing, &key).await {
        EffectGroupLifecycle::Closing { disposition, .. } => {
            assert_eq!(disposition, CANCEL);
        }
        EffectGroupLifecycle::Settled { disposition } => {
            assert_eq!(disposition, CANCEL);
        }
        EffectGroupLifecycle::Live => {
            panic!(
                "close returned while the row still read `live` — the \
                    cancel decisions were issued without the closing fact"
            )
        }
    }

    // The loser's cancel decision is durable: a second host over the same
    // journal reopens the group and reads rank 2 as the cancellation terminal
    // — no decision is owed, issued, or re-run by it.
    let second = make(spec(LIVE_LEASE_MS, &executors)).await;
    let closing_b = closing_seam(&second);
    let scoped_b = second
        .host
        .scoped(admit(scope(&prefix, "w9")))
        .expect("a scope binds on the resuming host");
    // The reopen restates the *declared* disposition — the fence refuses a
    // reopen that narrows it — while the row's effective `closing`
    // disposition is what the second host serves.
    let mut reopened = open(&scoped_b, &key, 2, RUN, vec![never(), never()]).await;
    let reread = next(&scoped_b, &mut reopened)
        .await
        .expect("the second host reads rank 1");
    assert_eq!(
        (reread.position, reread.sequence),
        (winner.position, winner.sequence),
    );
    let loser_settlement = next(&scoped_b, &mut reopened)
        .await
        .expect("the second host reads rank 2");
    assert!(
        loser_settlement.outcome.is_err(),
        "rank 2 is the seated cancel decision, not a body that ran: \
         {loser_settlement:?}"
    );

    // The resuming host's finalize — the `resume` entry a redriven turn calls —
    // completes the recorded closing: every rank was already seated, so it
    // re-decides nothing and re-executes nothing. The first host's own
    // finalizer may already have settled the row — a cancel-decided body is
    // dropped with its group — in which case the resume correctly finds no
    // closing group under the scope — and it may settle between the read and
    // the resume, so the assertion asks only that one of the two held.
    let reports = closing_b
        .resume_closing_groups(scoped_b.execution_scope(), &GroupOnlyFinalization)
        .await
        .expect("resume runs the recorded closing groups");
    if !reports
        .iter()
        .any(|report| matches!(report, GroupFinalizationReport::Settled { group_key } if *group_key == key))
    {
        assert!(
            matches!(
                lifecycle(&closing_b, &key).await,
                EffectGroupLifecycle::Settled { .. }
            ),
            "the recorded closing group finalizes to settled, by the resume \
             or the opener's own finalizer: {reports:?}"
        );
    }
    assert!(
        executors.executions().is_empty(),
        "a resume re-executes nothing: {:?}",
        executors.executions()
    );
    until_settled(&closing, &key).await;
    assert_eq!(
        completed_steps(&lifecycle(&closing_b, &key).await),
        None,
        "settled is terminal — it carries no cursor"
    );
    loser.release();
}

/// W10 — a crash after step 1 records leaves the cursor at `finalized: 1`, and
/// the resume runs the opener's steps 2 and 3 exactly once each — never
/// re-draining what step 1 already ranked.
///
/// The crash is a real one: the opener's runtime is dropped with a
/// `RunToCompletion` loser still parked and the close-time finalizer parked
/// behind it, so what the journal holds is `closing{finalized: 0}` plus a
/// claimed child under a lease nobody renews. The resume's step 1 finishes
/// that child once, then the injected failure parks the cursor at 1; the
/// healthy resume must pick up at step 2 — not at the drain.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_crash_after_drain_resumes_at_outcome_commit(make: DrainWorldFactory) {
    let prefix = format!("closing-w10-{}", uuid::Uuid::new_v4().simple());
    let group_scope = scope(&prefix, "w10");
    let key = group_key(&prefix, "w10");
    crashed_process(&make, {
        let group_scope = group_scope.clone();
        let key = key.clone();
        move |world| {
            Box::pin(async move {
                let scoped = world
                    .host
                    .scoped(admit(group_scope))
                    .expect("a scope binds");
                let entered = Arc::new(AtomicUsize::new(0));
                let handle =
                    open(&scoped, &key, 2, RUN, vec![settles(0), blocking(&entered)]).await;
                until(|| entered.load(Ordering::SeqCst) == 1).await;
                close(&scoped, handle, RUN)
                    .await
                    .expect("the caller closes and releases its loser");
                // The winner's rank must be durable before the process dies —
                // see `until_rank_seated`.
                until_rank_seated(&scoped, &key, 1).await;
                // The runtime dies here: the parked loser task and the
                // close-time finalizer waiting on it die with it.
            })
        }
    })
    .await;

    // The surviving host: poll step 1 with an always-failing step 2 until the
    // dead lease lapses and the loser is finished exactly once — `Err` is the
    // signal that the cursor reached 1, since `Pending` is the only earlier
    // answer.
    let executors = RecordingExecutors::settling();
    let world = make(spec(CRASH_LEASE_MS, &executors)).await;
    let closing = closing_seam(&world);
    let probe = RecordingFinalization::new(Injection::Always, Injection::Never);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match closing.finalize_group(&key, &probe).await {
                Err(_) => return,
                Ok(GroupFinalizationReport::Pending { .. }) => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Ok(GroupFinalizationReport::Settled { .. }) => {
                    panic!("an always-failing step 2 cannot settle the group")
                }
            }
        }
    })
    .await
    .expect("the dead lease lapses and step 1 records");
    assert_eq!(
        completed_steps(&lifecycle(&closing, &key).await),
        Some(1),
        "the injected crash parks the cursor between drain and outcome commit"
    );
    assert_eq!(probe.parent_calls(), 0, "step 3 never ran");

    // The healthy resume: steps 2 and 3 run exactly once each, the loser was
    // finished exactly once (by the first resume's drain pass), and the group
    // settles.
    let healthy = RecordingFinalization::new(Injection::Never, Injection::Never);
    let reports = closing
        .resume_closing_groups(&group_scope, &healthy)
        .await
        .expect("the resume finishes the recorded closing");
    assert!(
        reports
            .iter()
            .any(|report| matches!(report, GroupFinalizationReport::Settled { group_key } if *group_key == key)),
        "the resumed group settles: {reports:?}"
    );
    assert_eq!(healthy.outcome_calls(), 1, "step 2 ran once on the resume");
    assert_eq!(healthy.parent_calls(), 1, "step 3 ran once on the resume");
    assert_eq!(
        executors.executions(),
        vec![format!("{key}:child:1")],
        "the loser was finished exactly once — the resume never re-drains"
    );
    until_settled(&closing, &key).await;
}

/// W11 — a crash after step 2 records leaves the cursor at `finalized: 2`, and
/// the resume runs step 3 alone: the committed outcome and accounting step is
/// never re-run.
///
/// Same crash shape as W10; the injection point moves one step later.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_crash_after_accounting_resumes_at_parent_end(make: DrainWorldFactory) {
    let prefix = format!("closing-w11-{}", uuid::Uuid::new_v4().simple());
    let group_scope = scope(&prefix, "w11");
    let key = group_key(&prefix, "w11");
    crashed_process(&make, {
        let group_scope = group_scope.clone();
        let key = key.clone();
        move |world| {
            Box::pin(async move {
                let scoped = world
                    .host
                    .scoped(admit(group_scope))
                    .expect("a scope binds");
                let entered = Arc::new(AtomicUsize::new(0));
                let handle =
                    open(&scoped, &key, 2, RUN, vec![settles(0), blocking(&entered)]).await;
                until(|| entered.load(Ordering::SeqCst) == 1).await;
                close(&scoped, handle, RUN)
                    .await
                    .expect("the caller closes and releases its loser");
                // Same barrier as W10: the winner's rank must be durable
                // before the process dies, or the resume legitimately drains
                // it again.
                until_rank_seated(&scoped, &key, 1).await;
            })
        }
    })
    .await;

    let executors = RecordingExecutors::settling();
    let world = make(spec(CRASH_LEASE_MS, &executors)).await;
    let closing = closing_seam(&world);
    let probe = RecordingFinalization::new(Injection::Never, Injection::Always);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match closing.finalize_group(&key, &probe).await {
                Err(_) => return,
                Ok(GroupFinalizationReport::Pending { .. }) => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Ok(GroupFinalizationReport::Settled { .. }) => {
                    panic!("an always-failing step 3 cannot settle the group")
                }
            }
        }
    })
    .await
    .expect("the dead lease lapses and steps 1 and 2 record");
    assert_eq!(
        completed_steps(&lifecycle(&closing, &key).await),
        Some(2),
        "the injected crash parks the cursor between accounting and parent end"
    );
    assert_eq!(probe.outcome_calls(), 1, "step 2 ran exactly once");

    let healthy = RecordingFinalization::new(Injection::Never, Injection::Never);
    let reports = closing
        .resume_closing_groups(&group_scope, &healthy)
        .await
        .expect("the resume finishes the recorded closing");
    assert!(
        reports
            .iter()
            .any(|report| matches!(report, GroupFinalizationReport::Settled { group_key } if *group_key == key)),
        "the resumed group settles: {reports:?}"
    );
    assert_eq!(
        healthy.outcome_calls(),
        0,
        "a recorded step is never re-run: step 2 was committed before the crash"
    );
    assert_eq!(healthy.parent_calls(), 1, "step 3 ran once on the resume");
    assert_eq!(
        executors.executions(),
        vec![format!("{key}:child:1")],
        "the loser was finished exactly once"
    );
    until_settled(&closing, &key).await;
}

/// W12 — the drain budget bounds how long finalization waits on a
/// cancel-decided body, and `closing` stays durably readable while an
/// obligation is owed elsewhere.
///
/// Two halves.
///
/// **Pending.** A `RunToCompletion` loser still running on host A is a
/// protected obligation A owes — A's own close-time finalizer waits on it —
/// while a *second* host's `finalize_group` must report `Pending` rather than
/// steal a child under A's live lease. `closing` is readable on both hosts for
/// the whole window, and once the loser is released the group settles and both
/// hosts read it.
///
/// **Budget.** A `Cancel` close over a body that does not return reaches
/// `settled` while the body still has not finished — the rank its decision
/// seated is the durable fact, and the budget is what lets finalization stop
/// waiting on the body.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_drain_budget_expiry_leaves_closing_recorded(make: DrainWorldFactory) {
    let prefix = format!("closing-w12-{}", uuid::Uuid::new_v4().simple());

    // --- Pending: the protected obligation this host owes is not another
    // host's to finish.
    let executors = RecordingExecutors::settling();
    let world_a = make(spec(LIVE_LEASE_MS, &executors)).await;
    let closing_a = closing_seam(&world_a);
    let scoped_a = world_a
        .host
        .scoped(admit(scope(&prefix, "w12-pending")))
        .expect("a scope binds");
    let key = group_key(&prefix, "w12-pending");
    let (loser_executor, loser) = parked(1);
    let mut handle = open(&scoped_a, &key, 2, RUN, vec![settles(0), loser_executor]).await;
    until(|| loser.started() == 1).await;
    next(&scoped_a, &mut handle)
        .await
        .expect("the settling child takes rank 1");
    close(&scoped_a, handle, RUN)
        .await
        .expect("the close records `closing` and returns");

    // Host B, same journal: A's loser is under A's live lease, so B owes it
    // nothing and must say so — `Pending` — rather than dispatch it again.
    let world_b = make(spec(LIVE_LEASE_MS, &executors)).await;
    let closing_b = closing_seam(&world_b);
    let report = closing_b
        .finalize_group(&key, &GroupOnlyFinalization)
        .await
        .expect("a second host's finalize answers");
    assert_eq!(
        report,
        GroupFinalizationReport::Pending {
            group_key: key.clone(),
            unsettled: 1,
        },
        "a child under A's live lease is owed elsewhere: B reports Pending"
    );
    assert!(
        matches!(
            lifecycle(&closing_a, &key).await,
            EffectGroupLifecycle::Closing {
                disposition: RUN,
                ..
            }
        ),
        "`closing` is durably readable while the obligation is owed"
    );

    // Release: A's own finalizer converges the group to `settled`, and both
    // hosts read it.
    loser.release();
    until_settled(&closing_a, &key).await;
    until_settled(&closing_b, &key).await;
    assert_eq!(
        closing_b
            .finalize_group(&key, &GroupOnlyFinalization)
            .await
            .expect("a settled group reports settled"),
        GroupFinalizationReport::Settled {
            group_key: key.clone()
        },
    );

    // --- Budget: the wait on a cancel-decided body is bounded by the
    // controller's drain budget, measured from the decision.
    let executors = RecordingExecutors::settling();
    let world = make(spec_with_budget(
        LIVE_LEASE_MS,
        &executors,
        Some(Duration::from_millis(200)),
    ))
    .await;
    let closing = closing_seam(&world);
    let scoped = world
        .host
        .scoped(admit(scope(&prefix, "w12-budget")))
        .expect("a scope binds");
    let key = group_key(&prefix, "w12-budget");
    let (executor, body) = parked(1);
    let handle = open(&scoped, &key, 2, RUN, vec![settles(0), executor]).await;
    until(|| body.started() == 1).await;

    close(&scoped, handle, CANCEL)
        .await
        .expect("the close records `closing` and returns");
    // Whatever phase the read lands on, it must carry the cancelled
    // disposition — `closing` was recorded before the decision was issued.
    match lifecycle(&closing, &key).await {
        EffectGroupLifecycle::Closing { disposition, .. }
        | EffectGroupLifecycle::Settled { disposition } => {
            assert_eq!(disposition, CANCEL);
        }
        EffectGroupLifecycle::Live => {
            panic!("close returned while the row still read `live`")
        }
    }
    until_settled(&closing, &key).await;
    assert_eq!(
        body.finished(),
        0,
        "the group settled while the cancel-decided body had not returned: \
         the seated rank, not the body, is the durable fact"
    );
    // The settled group's task is dropped with it — releasing the body is
    // hygiene for a body that survived, not a guarantee it returns.
    body.release();
}
