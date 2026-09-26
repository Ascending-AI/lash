//! The harness proves itself: a deterministic drive passes every replay, and
//! each class of nondeterminism the check exists for fails it at the run and
//! entry that exposes it.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use futures_util::future::{Either, join, select};

use super::*;
use crate::{
    AdmittedScope, EffectAddress, ExecutionScope, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, SleepSpec,
};

const SEED: u64 = 0x5EED_3672;

fn check() -> DeterminismCheck {
    DeterminismCheck::new(SEED).perturbed_replays(8)
}

fn failure_of<E: DeterminismEngine>(check: DeterminismCheck, engine: &E) -> DeterminismFailure {
    match check.run(engine) {
        Ok(report) => panic!("the check passed: {:?}", report.transcript),
        Err(failure) => failure,
    }
}

/// The canonical deterministic shape: sequential operations, two concurrent
/// ones joined in declaration order, and a commit built from their outcomes.
fn deterministic_drive<'c>(
    _: &'c (),
    cx: &'c LocalTestCx,
) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
    Box::pin(async move {
        let first: u64 = cx.op("turn/1", "llm", &"prompt", async { 7 }).await;
        let (left, right): (u64, u64) = join(
            cx.op("turn/2a", "tool", &("add", first), async move { first + 1 }),
            cx.op(
                "turn/2b",
                "tool",
                &("double", first),
                async move { first * 2 },
            ),
        )
        .await;
        let last: String = cx
            .op("turn/3", "llm", &(left, right), async move {
                format!("{left}+{right}")
            })
            .await;
        cx.record_commit(&("committed", first, left, right, last));
    })
}

#[test]
fn a_deterministic_drive_passes_cold_separate_worker_and_perturbed_replays() {
    let engine = LocalEngine::new(|| (), deterministic_drive);
    let report = check()
        .run(&engine)
        .unwrap_or_else(|failure| panic!("{failure}"));

    assert_eq!(
        report.replays.len(),
        10,
        "cold, separate worker, 8 perturbed"
    );
    assert_eq!(report.transcript.commands().count(), 4);
    assert_eq!(
        report.transcript.commits().collect::<Vec<_>>(),
        vec![r#"["committed",7,8,14,"8+14"]"#]
    );
}

#[test]
fn the_check_is_itself_deterministic_under_its_seed() {
    let engine = LocalEngine::new(|| (), deterministic_drive);
    let first = check().run(&engine).map(|report| report.transcript);
    let second = check().run(&engine).map(|report| report.transcript);
    assert_eq!(first, second);
}

/// A value the drive reads that nothing records: a clock, an RNG, a global
/// counter. It changes between runs of one worker.
static AMBIENT: AtomicU64 = AtomicU64::new(0);

#[test]
fn an_unrecorded_ambient_read_in_a_command_diverges_on_the_cold_replay() {
    let engine = LocalEngine::new(
        || (),
        |_: &(), cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                let now = AMBIENT.fetch_add(1, Ordering::SeqCst);
                let _: () = cx.op("turn/1", "sleep", &("until", now), async {}).await;
            })
        },
    );
    let failure = failure_of(check(), &engine);

    assert_eq!(failure.mode, RunMode::Replay(ReplayMode::Cold));
    assert!(
        matches!(
            &failure.cause,
            FailureCause::Run(RunFailure::Divergence(ReplayDivergence::CommandMismatch { key, .. }))
                if key == "turn/1"
        ),
        "{failure}"
    );
}

/// A worker's process-local state: stable within one worker, different on
/// another — a registry, a cache, a worker-minted reference.
struct Worker {
    local_ref: u64,
}

static WORKERS: AtomicU64 = AtomicU64::new(0);

#[test]
fn worker_local_state_in_a_commit_diverges_only_on_a_separate_worker() {
    let engine = LocalEngine::new(
        || Worker {
            local_ref: WORKERS.fetch_add(1, Ordering::SeqCst),
        },
        |worker: &Worker, cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                let answer: u64 = cx.op("turn/1", "llm", &"prompt", async { 1 }).await;
                cx.record_commit(&(answer, worker.local_ref));
            })
        },
    );
    let failure = failure_of(check(), &engine);

    assert_eq!(failure.mode, RunMode::Replay(ReplayMode::SeparateWorker));
    let FailureCause::Diverged(divergence) = &failure.cause else {
        panic!("{failure}");
    };
    assert_eq!(
        divergence.index, 1,
        "the command matched; the commit did not"
    );
    assert!(matches!(
        divergence.expected,
        Some(TranscriptEntry::Commit { .. })
    ));
}

/// Which of two concurrent operations lands first is scheduling, not a
/// recorded fact: a drive that branches on it is the unrecorded `select!` race.
fn racing_drive<'c>(_: &'c (), cx: &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
    Box::pin(async move {
        let left = cx.op("turn/left", "tool", &"left", async { 1_u64 });
        let right = cx.op("turn/right", "tool", &"right", async { 2_u64 });
        let winner = match select(left, right).await {
            Either::Left((value, _)) | Either::Right((value, _)) => value,
        };
        let _: () = cx
            .op(format!("turn/after-{winner}"), "llm", &winner, async {})
            .await;
    })
}

#[test]
fn branching_on_completion_order_diverges_under_perturbed_scheduling() {
    let engine = LocalEngine::new(|| (), racing_drive);
    let failure = failure_of(check().perturbed_replays(32), &engine);

    assert!(
        matches!(
            failure.mode,
            RunMode::Replay(ReplayMode::Cold | ReplayMode::Perturbed { .. })
        ),
        "{failure}"
    );
    assert!(
        matches!(
            &failure.cause,
            FailureCause::Run(RunFailure::Divergence(ReplayDivergence::UnrecordedCommand { key, .. }))
                if key.starts_with("turn/after-")
        ),
        "{failure}"
    );
}

/// Wakes its own task once, as `yield_now` or a channel does.
struct WakeSelfOnce(bool);

impl Future for WakeSelfOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

#[test]
fn a_wake_no_operation_caused_fails_the_run() {
    let engine = LocalEngine::new(
        || (),
        |_: &(), cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                let _: u64 = cx.op("turn/1", "llm", &"prompt", async { 1 }).await;
                WakeSelfOnce(false).await;
            })
        },
    );
    let failure = failure_of(check(), &engine);

    assert_eq!(failure.mode, RunMode::Fresh);
    assert!(
        matches!(
            failure.cause,
            FailureCause::Run(RunFailure::NonOpWake { .. })
        ),
        "{failure}"
    );
}

#[test]
fn awaiting_a_future_that_is_not_an_operation_fails_the_run() {
    let engine = LocalEngine::new(
        || (),
        |_: &(), _cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                let (_keep, never) = tokio::sync::oneshot::channel::<()>();
                let _ = never.await;
            })
        },
    );
    let failure = failure_of(check(), &engine);

    assert_eq!(failure.mode, RunMode::Fresh);
    assert!(
        matches!(
            failure.cause,
            FailureCause::Run(RunFailure::NonOpAwait { .. })
        ),
        "{failure}"
    );
}

#[test]
fn drive_code_runs_outside_any_tokio_runtime() {
    let engine = LocalEngine::new(
        || (),
        |_: &(), _cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            })
        },
    );
    let failure = failure_of(check(), &engine);

    assert_eq!(failure.mode, RunMode::Fresh);
    assert!(
        matches!(
            failure.cause,
            FailureCause::Run(RunFailure::Panicked { .. })
        ),
        "{failure}"
    );
}

#[test]
fn an_operation_body_may_use_tokio_and_wake_from_another_thread() {
    let engine = LocalEngine::new(
        || (),
        |_: &(), cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                let slept: u64 = cx
                    .op("turn/1", "sleep", &1_u64, async {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        let task = crate::task::spawn(async { 3_u64 });
                        task.await.unwrap_or_default()
                    })
                    .await;
                cx.record_commit(&slept);
            })
        },
    );
    let report = check()
        .run(&engine)
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_eq!(report.transcript.commits().collect::<Vec<_>>(), vec!["3"]);
}

fn sleep_envelope(key: &str, duration_ms: u64) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(ExecutionScope::turn("determinism-session", "turn-1"), key)
                .unwrap_or_else(|error| panic!("{error}")),
            RuntimeAttribution::none(),
            key,
        ),
        RuntimeEffectCommand::Sleep {
            spec: SleepSpec::For { duration_ms },
        },
    )
}

fn admitted() -> AdmittedScope {
    AdmittedScope::new(ExecutionScope::turn("determinism-session", "turn-1"))
}

static CONTROLLER_BODIES: AtomicUsize = AtomicUsize::new(0);

/// Drive code as it is today: effects issued through a scoped controller.
fn controller_drive<'c>(_: &'c (), cx: &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
    Box::pin(async move {
        let controller = cx
            .controller(admitted())
            .unwrap_or_else(|error| panic!("{error}"));
        let mut outcomes = Vec::new();
        for (key, duration_ms) in [("sleep-a", 5), ("sleep-b", 9)] {
            let outcome = controller
                .controller()
                .execute_effect(
                    sleep_envelope(key, duration_ms),
                    RuntimeEffectLocalExecutor::testing(|_| async {
                        CONTROLLER_BODIES.fetch_add(1, Ordering::SeqCst);
                        Ok(RuntimeEffectOutcome::Sleep)
                    }),
                )
                .await;
            outcomes.push(matches!(outcome, Ok(RuntimeEffectOutcome::Sleep)));
        }
        cx.record_commit(&outcomes);
    })
}

#[test]
fn drive_code_issuing_effects_through_a_controller_runs_under_the_harness() {
    let engine = LocalEngine::new(|| (), controller_drive);
    let report = check()
        .run(&engine)
        .unwrap_or_else(|failure| panic!("{failure}"));

    assert_eq!(
        CONTROLLER_BODIES.load(Ordering::SeqCst),
        2,
        "the fresh run executes each effect once; every replay serves the journal"
    );
    let kinds = report
        .transcript
        .commands()
        .map(|entry| match entry {
            TranscriptEntry::Command { key, kind, .. } => format!("{kind}@{key}"),
            TranscriptEntry::Commit { .. } => String::new(),
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["sleep@sleep-a", "sleep@sleep-b"]);
    assert_eq!(
        report.transcript.commits().collect::<Vec<_>>(),
        vec!["[true,true]"]
    );
}

static ENVELOPE_NONCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn an_envelope_that_is_not_derived_from_recorded_state_diverges_through_the_controller() {
    let engine = LocalEngine::new(
        || (),
        |_: &(), cx: &LocalTestCx| -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                let controller = cx
                    .controller(admitted())
                    .unwrap_or_else(|error| panic!("{error}"));
                let duration_ms = ENVELOPE_NONCE.fetch_add(1, Ordering::SeqCst);
                let _ = controller
                    .controller()
                    .execute_effect(
                        sleep_envelope("sleep-a", duration_ms),
                        RuntimeEffectLocalExecutor::testing(|_| async {
                            Ok(RuntimeEffectOutcome::Sleep)
                        }),
                    )
                    .await;
            })
        },
    );
    let failure = failure_of(check(), &engine);

    assert_eq!(failure.mode, RunMode::Replay(ReplayMode::Cold));
    assert!(
        matches!(
            &failure.cause,
            FailureCause::Run(RunFailure::Divergence(ReplayDivergence::CommandMismatch {
                key,
                issued_kind,
                ..
            })) if key == "sleep-a" && issued_kind == "sleep"
        ),
        "{failure}"
    );
}

#[test]
fn the_comparator_reports_the_first_differing_entry() {
    let command = |bytes: &str| TranscriptEntry::Command {
        key: "k".to_string(),
        kind: "llm".to_string(),
        bytes: bytes.to_string(),
    };
    let reference = DriveTranscript {
        entries: vec![command("same"), command(r#"{"deadline":100}"#)],
    };
    let actual = DriveTranscript {
        entries: vec![command("same"), command(r#"{"deadline":101}"#)],
    };
    let divergence = reference
        .compare(&actual)
        .expect_err("the second command differs");
    assert_eq!(divergence.index, 1);
    assert!(
        divergence.to_string().contains("at byte 14"),
        "{divergence}"
    );

    let shorter = DriveTranscript {
        entries: vec![command("same")],
    };
    let divergence = reference
        .compare(&shorter)
        .expect_err("the run ended early");
    assert_eq!(divergence.actual, None);
    assert!(reference.compare(&reference.clone()).is_ok());
}
