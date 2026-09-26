//! Live Restate turn runner for the turn-driving conformance laws.
//!
//! A Restate turn runs only inside a handler: its effects journal on a
//! `ctx`-bound [`RestateRuntimeEffectController`](crate::RestateRuntimeEffectController),
//! and the deployment host refuses every effect that has not entered one. The
//! live suite's endpoint serves in this test process, so a law hands its turn
//! to [`LiveTurnRunner`], which parks the law's attempt factory in a
//! process-local table and invokes [`ConformanceTurnProbe`] through ingress;
//! the handler looks the factory up and runs a fresh attempt on its own
//! controller. The tool calls of that turn open real Restate effect groups
//! whose children run in the endpoint's dispatch invocations.
//!
//! Restate runs the handler again from the top on every replay of the
//! invocation (after a suspension or a failed attempt), so the table never
//! gives an attempt away: every execution builds its turn afresh from the
//! same inputs and takes the same command path, and only the execution that
//! ends reports.
//!
//! One scope is one invocation, as on the in-process runner: a turn that
//! aborts without an outcome — it parked on a replay divergence, or met a
//! live fault — fails its attempt retryably and leaves the invocation open,
//! and the law's next run of that scope is Restate's retry of it, replaying
//! its journal. A parked attempt ends through
//! [`parked_turn_failure`](crate::parked_turn_failure), exactly as a product
//! turn handler's does: returning at the park would propose the handler's
//! output where the journal holds its next command (FIG-3697).

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use crate::RestateIngressClient;

/// One attempt a law hands a turn's invocation. It runs once per execution of
/// the handler until one of its executions ends.
struct QueuedAttempt {
    attempt: lash_conformance::ConformanceTurnAttempt,
    /// The attempt must crash; its crash is a redelivery, not a failure.
    crashing: bool,
    /// The law's trigger that kills this attempt from outside it, when the
    /// law crashes the turn at a point of its own choosing.
    crash: Option<lash_conformance::ConformanceCrash>,
    /// An aborted end keeps the attempt queued, so every retry of the open
    /// invocation runs it again, as a product handler's retry re-runs its
    /// turn.
    repeats_on_retry: bool,
}

/// How one execution of the handler ended, as the runner is told.
#[derive(Debug)]
enum AttemptEnd {
    /// The crashing attempt crashed; Restate redelivers the invocation.
    Crashed,
    /// The turn settled; the handler returns and the invocation completes.
    Settled,
    /// The turn aborted without an outcome; the invocation stays open.
    Aborted,
}

/// One scope's invocation, while the law may still run it.
struct PendingTurn {
    admitted: lash_core::AdmittedScope,
    attempts: VecDeque<QueuedAttempt>,
    ends: tokio::sync::mpsc::UnboundedSender<AttemptEnd>,
}

fn pending_turns() -> &'static Mutex<HashMap<String, PendingTurn>> {
    static TURNS: OnceLock<Mutex<HashMap<String, PendingTurn>>> = OnceLock::new();
    TURNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Wakes a handler execution that waits for the law's next attempt.
fn attempt_queued() -> &'static tokio::sync::Notify {
    static QUEUED: OnceLock<tokio::sync::Notify> = OnceLock::new();
    QUEUED.get_or_init(tokio::sync::Notify::new)
}

/// How long a retry of an open invocation waits for the law's next attempt
/// before it fails retryably, as a parked turn's retries do.
const NEXT_ATTEMPT_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// The workflow whose handler runs one parked conformance turn.
#[restate_sdk::workflow]
pub(super) trait ConformanceTurnProbe {
    async fn run(key: Json<String>) -> HandlerResult<Json<bool>>;
}

pub(super) struct ConformanceTurnProbeImpl;

/// What one handler execution runs.
enum NextAttempt {
    Run {
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
        crashing: bool,
        crash: Option<lash_conformance::ConformanceCrash>,
        repeats_on_retry: bool,
        ends: tokio::sync::mpsc::UnboundedSender<AttemptEnd>,
    },
    /// The law has queued no attempt yet.
    Idle,
    /// No turn with this key is pending in this process.
    Unknown,
}

fn next_attempt(key: &str) -> NextAttempt {
    let mut turns = pending_turns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(turn) = turns.get_mut(key) else {
        return NextAttempt::Unknown;
    };
    let Some(front) = turn.attempts.front() else {
        return NextAttempt::Idle;
    };
    NextAttempt::Run {
        admitted: turn.admitted.clone(),
        attempt: Arc::clone(&front.attempt),
        crashing: front.crashing,
        crash: front.crash.clone(),
        repeats_on_retry: front.repeats_on_retry,
        ends: turn.ends.clone(),
    }
}

/// The current attempt ended: the next execution runs the next one.
fn finish_attempt(key: &str) {
    if let Some(turn) = pending_turns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(key)
    {
        turn.attempts.pop_front();
    }
}

impl ConformanceTurnProbe for ConformanceTurnProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(key): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let (admitted, attempt, crashing, crash, repeats_on_retry, ends) = loop {
            let queued = attempt_queued().notified();
            tokio::pin!(queued);
            queued.as_mut().enable();
            match next_attempt(&key) {
                NextAttempt::Run {
                    admitted,
                    attempt,
                    crashing,
                    crash,
                    repeats_on_retry,
                    ends,
                } => break (admitted, attempt, crashing, crash, repeats_on_retry, ends),
                // An invocation that finds nothing to run fails terminally
                // rather than silently succeeding without the turn it was
                // asked to run.
                NextAttempt::Unknown => {
                    return Err(TerminalError::new(format!(
                        "conformance turn `{key}` is not pending in this process"
                    ))
                    .into());
                }
                // A retry of an open invocation waits for the law's next
                // attempt; one that never comes fails the retry retryably.
                NextAttempt::Idle => {
                    if tokio::time::timeout(NEXT_ATTEMPT_WAIT, queued)
                        .await
                        .is_err()
                    {
                        return Err(HandlerError::from(std::io::Error::other(format!(
                            "conformance turn `{key}` has no attempt queued"
                        ))));
                    }
                }
            }
        };
        let controller = crate::RestateRuntimeEffectController::new_for_test(ctx);
        let scoped = controller
            .scoped_effect_controller(admitted)
            .map_err(TerminalError::from_error)?;
        let run = CatchUnwind {
            inner: attempt(scoped),
        };
        // A law's crash trigger kills this execution where it stands: the
        // attempt's future is dropped mid-poll, as a dying deployment drops
        // its handler, and the attempt fails retryably so Restate redelivers
        // the invocation to the law's next run of the scope.
        let ran = match crash {
            Some(crash) => tokio::select! {
                biased;
                () = crash.fired() => Err(()),
                ran = run => match ran {
                    Ok(end) => panic!(
                        "conformance turn `{key}` ended ({end:?}) before its crash fired"
                    ),
                    Err(()) => Err(()),
                },
            },
            None => run.await,
        };
        let (end, result) = match ran {
            Ok(lash_conformance::ConformanceTurnEnd::Settled) => {
                (AttemptEnd::Settled, Ok(Json(true)))
            }
            // The turn parked: its attempt ends the way every parked turn
            // handler's does, retryably, so the invocation keeps its journal.
            Ok(lash_conformance::ConformanceTurnEnd::Aborted(
                lash_core::TurnFailureCause::Parked,
            )) => (
                AttemptEnd::Aborted,
                Err(crate::parked_turn_failure(format!(
                    "conformance turn `{key}`"
                ))),
            ),
            // Any other abort is a live fault: retryable, the invocation
            // stays open for its retry.
            Ok(lash_conformance::ConformanceTurnEnd::Aborted(cause)) => (
                AttemptEnd::Aborted,
                Err(HandlerError::from(std::io::Error::other(format!(
                    "conformance turn `{key}` aborted: {cause:?}"
                )))),
            ),
            // The crashing attempt died as the law asked: fail retryably, so
            // Restate redelivers the invocation and the redrive replays this
            // attempt's journal — the way a deployment recovers a turn whose
            // handler died.
            Err(()) if crashing => (
                AttemptEnd::Crashed,
                Err(HandlerError::from(std::io::Error::other(format!(
                    "conformance turn `{key}` crashed; Restate redelivers it to the redrive"
                )))),
            ),
            // Any other panic in the law must fail the invocation terminally:
            // an unwinding handler reads as retryable, and the retry would
            // panic again.
            Err(()) => {
                return Err(TerminalError::new(format!(
                    "conformance turn `{key}` panicked inside the probe handler"
                ))
                .into());
            }
        };
        if !(repeats_on_retry && matches!(end, AttemptEnd::Aborted)) {
            finish_attempt(&key);
        }
        let _ = ends.send(end);
        result
    }
}

/// Polls a future under `catch_unwind`, turning a panic into `Err(())`. The
/// payload is already on stderr through the panic hook.
pub(super) struct CatchUnwind<'a, T> {
    pub(super) inner: Pin<Box<dyn Future<Output = T> + Send + 'a>>,
}

impl<T> Future for CatchUnwind<'_, T> {
    type Output = Result<T, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

/// How one execution of a served process segment ended.
#[derive(Debug)]
pub(super) enum SegmentEnd {
    /// The law's crash killed it; its invocation fails retryably.
    Crashed,
    /// Its body settled; the process completes.
    Settled,
    /// Its body aborted without an outcome; the invocation retries.
    Aborted,
    /// Its body panicked; the process fails.
    Panicked,
}

/// The body a law serves one process's segments with, and the crash that
/// kills its execution.
struct ServedSegment {
    body: Option<lash_conformance::ConformanceTurnAttempt>,
    crash: Option<lash_conformance::ConformanceCrash>,
    ends: Option<tokio::sync::mpsc::UnboundedSender<SegmentEnd>>,
}

/// The process segments laws serve on the endpoint's `LashProcessWorkflow`:
/// the endpoint's process runner consults this table first, so a served
/// process's every segment execution — past the workflow's own admission —
/// runs the law's body on the process-scoped controller the workflow lends.
///
/// As on the turn probe, Restate runs a segment again from the top on every
/// replay, so a body is a factory, and an execution that finds no body served
/// waits for the law's next one.
#[derive(Default)]
pub(super) struct ServedSegments {
    segments: Mutex<HashMap<lash_core::ProcessId, ServedSegment>>,
    /// Bodies served for the process a start keyed so registers: the start
    /// mints the id, so the law can only name that process by its key, and
    /// the first execution of it adopts the body under its minted id.
    by_start_key: Mutex<HashMap<lash_core::StartKey, ServedSegment>>,
    served: tokio::sync::Notify,
}

impl ServedSegments {
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<lash_core::ProcessId, ServedSegment>> {
        self.segments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serves `process_id`'s segments with `body` from now on (or with none,
    /// so an execution waits), racing `crash` when one is given.
    fn serve(
        &self,
        process_id: &lash_core::ProcessId,
        body: Option<lash_conformance::ConformanceTurnAttempt>,
        crash: Option<lash_conformance::ConformanceCrash>,
        ends: Option<tokio::sync::mpsc::UnboundedSender<SegmentEnd>>,
    ) {
        self.table()
            .insert(process_id.clone(), ServedSegment { body, crash, ends });
        self.served.notify_waiters();
    }

    /// Serves the segments of the process a start keyed `start_key`
    /// registers with `body` from now on.
    fn serve_start(
        &self,
        start_key: &lash_core::StartKey,
        body: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.by_start_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                start_key.clone(),
                ServedSegment {
                    body: Some(body),
                    crash: None,
                    ends: None,
                },
            );
        self.served.notify_waiters();
    }

    /// Whether a law serves the segments of `process_id`, registered by
    /// `registration`: by its id, or by the key of the start that minted it,
    /// whose served body the id then adopts.
    pub(super) fn serves(
        &self,
        process_id: &lash_core::ProcessId,
        registration: &lash_core::ProcessRegistration,
    ) -> bool {
        if let Some(start_key) = &registration.start_key
            && let Some(segment) = self
                .by_start_key
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(start_key)
        {
            self.table().insert(process_id.clone(), segment);
        }
        self.table().contains_key(process_id)
    }

    /// Runs one execution of `process_id`'s served segment.
    pub(super) async fn run(
        &self,
        process_id: &lash_core::ProcessId,
        scoped: lash_core::ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        let retryable = |message: String| {
            lash_core::PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                message,
            ))
        };
        let (body, crash, ends) = loop {
            let served = self.served.notified();
            tokio::pin!(served);
            served.as_mut().enable();
            if let Some(segment) = self.table().get(process_id)
                && let Some(body) = &segment.body
            {
                break (
                    Arc::clone(body),
                    segment.crash.clone(),
                    segment.ends.clone(),
                );
            }
            // A retry of a crashed segment waits for the law's recovery body;
            // one that never comes fails the retry retryably.
            if tokio::time::timeout(NEXT_ATTEMPT_WAIT, served)
                .await
                .is_err()
            {
                return Err(retryable(format!(
                    "process `{process_id}` has no segment body served"
                )));
            }
        };
        let run = CatchUnwind {
            inner: body(scoped),
        };
        let ran = match &crash {
            Some(crash) => tokio::select! {
                biased;
                () = crash.fired() => None,
                ran = run => Some(ran),
            },
            None => Some(run.await),
        };
        let (end, outcome) = match ran {
            // The law's crash killed this execution where it stands: the body
            // is dropped mid-poll, as a dying deployment drops its handler,
            // and the invocation fails retryably. Its retry waits for the
            // law's recovery body.
            None => {
                if let Some(segment) = self.table().get_mut(process_id) {
                    segment.body = None;
                    segment.crash = None;
                }
                (
                    SegmentEnd::Crashed,
                    Err(retryable(format!(
                        "process `{process_id}` crashed; its engine recovers it"
                    ))),
                )
            }
            Some(Ok(lash_conformance::ConformanceTurnEnd::Settled)) => (
                SegmentEnd::Settled,
                Ok(lash_core::ProcessRunOutcome::Terminal {
                    output: Box::new(lash_core::ProcessAwaitOutput::from_tool_output(
                        lash_core::ToolCallOutput::success(serde_json::json!({
                            "served_segment": "settled"
                        })),
                    )),
                    prelude: Vec::new(),
                }),
            ),
            Some(Ok(lash_conformance::ConformanceTurnEnd::Aborted(cause))) => (
                SegmentEnd::Aborted,
                Err(retryable(format!(
                    "process `{process_id}` aborted: {cause:?}"
                ))),
            ),
            Some(Err(())) => (
                SegmentEnd::Panicked,
                Err(lash_core::PluginError::Session(format!(
                    "process `{process_id}` panicked in its served segment"
                ))),
            ),
        };
        if let Some(ends) = ends {
            let _ = ends.send(end);
        }
        outcome
    }
}

/// Runs each conformance turn inside a [`ConformanceTurnProbe`] handler on the
/// live endpoint.
pub(super) struct LiveTurnRunner {
    connection: crate::RestateConnection,
    /// Where the runner kills and purges a crashed process segment's
    /// invocation for [`SegmentRecovery::SubstrateLost`](lash_conformance::SegmentRecovery::SubstrateLost).
    admin: super::effect_group_conformance::HarnessAdmin,
    process_runner: std::sync::Arc<super::effect_group_conformance::LawProcessRunner>,
    /// The invocations a law left open, by scope: each one's probe key and
    /// its ingress call, which returns once the invocation completes.
    open: tokio::sync::Mutex<HashMap<String, OpenInvocation>>,
    /// The process segments a law crashed and has not recovered yet.
    crashed_segments: tokio::sync::Mutex<HashMap<lash_core::ProcessId, CrashedSegment>>,
}

struct OpenInvocation {
    key: String,
    call: tokio::task::JoinHandle<Result<bool, crate::RestateHttpError>>,
}

/// A process segment whose execution a law's crash killed: its registration,
/// and the ingress call of its invocation, which returns once the invocation
/// completes.
struct CrashedSegment {
    process_id: lash_core::ProcessId,
    registration: lash_core::ProcessRegistration,
    call: SegmentCall,
}

type SegmentCall =
    tokio::task::JoinHandle<Result<crate::RestateProcessWorkflowOutput, crate::RestateHttpError>>;

/// How long a recovered segment may take to reach its end.
const SEGMENT_RECOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl LiveTurnRunner {
    pub(super) fn shared(
        connection: crate::RestateConnection,
        admin: super::effect_group_conformance::HarnessAdmin,
        process_runner: std::sync::Arc<super::effect_group_conformance::LawProcessRunner>,
    ) -> std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner> {
        std::sync::Arc::new(Self {
            connection,
            admin,
            process_runner,
            open: tokio::sync::Mutex::default(),
            crashed_segments: tokio::sync::Mutex::default(),
        })
    }

    /// Submits segment 0 of `registration` to the endpoint's
    /// `LashProcessWorkflow`, the way a process start schedules it, and
    /// returns the ingress call that completes with the invocation.
    fn submit_segment(
        &self,
        process_id: &lash_core::ProcessId,
        registration: &lash_core::ProcessRegistration,
    ) -> SegmentCall {
        let ingress = RestateIngressClient::new(self.connection.clone());
        let key = crate::process::process_segment_workflow_key(process_id, 0);
        let input = crate::RestateProcessWorkflowInput {
            process_id: process_id.clone(),
            registration: registration.clone(),
            execution_context: lash_core::ProcessExecutionContext::default(),
            segment_ordinal: 0,
            journal_version: crate::RESTATE_PROCESS_JOURNAL_VERSION,
        };
        tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, crate::RestateProcessWorkflowOutput>(
                    crate::LashService::ProcessWorkflow.name(),
                    &key,
                    "run",
                    &input,
                )
                .await
        })
    }

    /// Runs `attempts` as the next attempts of `admitted`'s invocation: a new
    /// invocation, or the retry of the one a parked or aborted run left open.
    /// Returns once the turn settled (the invocation completed) or aborted
    /// (the invocation stays open for the law's next run of the scope). An
    /// attempt that repeats on retry instead runs on every retry until the
    /// invocation pauses, and the runner returns how many runs aborted.
    async fn run_attempts(
        &self,
        admitted: lash_core::AdmittedScope,
        attempts: Vec<QueuedAttempt>,
    ) -> usize {
        let until_paused = attempts.iter().any(|attempt| attempt.repeats_on_retry);
        let scope = format!("{:?}", admitted.scope());
        let crash_expected = attempts.iter().any(|attempt| attempt.crashing);
        let leave_open_on_crash = attempts
            .last()
            .is_some_and(|attempt| attempt.crash.is_some());
        let (ends, mut ended) = tokio::sync::mpsc::unbounded_channel();
        let mut open = self.open.lock().await;
        let reopened = open.remove(&scope);
        let key = reopened.as_ref().map_or_else(
            || {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                format!(
                    "turn-probe-{}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("clock after the epoch")
                        .as_nanos(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                )
            },
            |open| open.key.clone(),
        );
        {
            let mut turns = pending_turns()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn = turns.entry(key.clone()).or_insert_with(|| PendingTurn {
                admitted: admitted.clone(),
                attempts: VecDeque::new(),
                ends: ends.clone(),
            });
            turn.admitted = admitted;
            turn.attempts.extend(attempts);
            turn.ends = ends;
        }
        attempt_queued().notify_waiters();
        let mut call = match reopened {
            Some(open) => open.call,
            None => {
                let ingress = RestateIngressClient::new(self.connection.clone());
                let key = key.clone();
                tokio::spawn(async move {
                    ingress
                        .call_workflow_json::<_, bool>("ConformanceTurnProbe", &key, "run", &key)
                        .await
                })
            }
        };
        let mut crashed = false;
        let mut aborted = 0_usize;
        let mut poll = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                // The handler reports its attempt's end before it returns, so
                // when both are ready the report is read first; an unbiased
                // pick could take the finished call and miss the report.
                biased;
                end = ended.recv() => match end {
                    // The law crashed this turn from outside: the invocation
                    // stays open, and the law's next run of the scope is
                    // Restate's redelivery of it.
                    Some(AttemptEnd::Crashed) if leave_open_on_crash => {
                        open.insert(scope, OpenInvocation { key: key.clone(), call });
                        crashed = true;
                        break;
                    }
                    Some(AttemptEnd::Crashed) => crashed = true,
                    Some(AttemptEnd::Settled) if until_paused => panic!(
                        "the live conformance turn `{key}` settled where every run must park"
                    ),
                    Some(AttemptEnd::Settled) => {
                        let ran = (&mut call).await.expect("the probe's ingress call task");
                        pending_turns()
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&key);
                        assert!(
                            matches!(ran, Ok(true)),
                            "the live conformance turn `{key}` did not complete in its handler: \
                             {ran:?}"
                        );
                        break;
                    }
                    Some(AttemptEnd::Aborted) if until_paused => aborted += 1,
                    Some(AttemptEnd::Aborted) => {
                        aborted += 1;
                        open.insert(scope, OpenInvocation { key: key.clone(), call });
                        break;
                    }
                    None => panic!("the live conformance turn `{key}` lost its attempt channel"),
                },
                // A paused invocation runs nothing more until an operator
                // resumes it; the law is done with it.
                _ = poll.tick(), if until_paused && aborted > 0 => {
                    if self.admin.workflow_paused("ConformanceTurnProbe", &key).await {
                        pending_turns()
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&key);
                        call.abort();
                        break;
                    }
                }
                ran = &mut call => {
                    pending_turns()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&key);
                    panic!(
                        "the live conformance turn `{key}` ended before its attempt reported: \
                         {ran:?}"
                    );
                }
            }
        }
        assert!(
            crashed || !crash_expected,
            "the live conformance turn `{key}` ended without its crashing attempt crashing"
        );
        aborted
    }
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for LiveTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.run_attempts(
            admitted,
            vec![QueuedAttempt {
                attempt,
                crashing: false,
                crash: None,
                repeats_on_retry: false,
            }],
        )
        .await;
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.run_attempts(
            admitted,
            vec![
                QueuedAttempt {
                    attempt: crashing,
                    crashing: true,
                    crash: None,
                    repeats_on_retry: false,
                },
                QueuedAttempt {
                    attempt: redrive,
                    crashing: false,
                    crash: None,
                    repeats_on_retry: false,
                },
            ],
        )
        .await;
    }

    async fn run_parking_turn_until_rested(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) -> usize {
        self.run_attempts(
            admitted,
            vec![QueuedAttempt {
                attempt,
                crashing: false,
                crash: None,
                repeats_on_retry: true,
            }],
        )
        .await
    }

    async fn run_turn_until_crash(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
        crash: lash_conformance::ConformanceCrash,
    ) {
        self.run_attempts(
            admitted,
            vec![QueuedAttempt {
                attempt,
                crashing: true,
                crash: Some(crash),
                repeats_on_retry: false,
            }],
        )
        .await;
    }

    async fn serve_segments(
        &self,
        start_key: &lash_core::StartKey,
        body: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.process_runner.segments().serve_start(start_key, body);
    }

    /// The segment runs in the endpoint's real `LashProcessWorkflow`, past its
    /// admission; the crash fails the execution retryably, so the invocation
    /// stays open for Restate to deliver again.
    async fn run_segment_until_crash(
        &self,
        process_id: &lash_core::ProcessId,
        registration: lash_core::ProcessRegistration,
        body: lash_conformance::ConformanceTurnAttempt,
        crash: lash_conformance::ConformanceCrash,
    ) {
        let process_id = process_id.clone();
        let (ends, mut ended) = tokio::sync::mpsc::unbounded_channel();
        self.process_runner
            .segments()
            .serve(&process_id, Some(body), Some(crash), Some(ends));
        let mut call = self.submit_segment(&process_id, &registration);
        tokio::select! {
            biased;
            end = ended.recv() => match end {
                Some(SegmentEnd::Crashed) => {}
                end => panic!(
                    "the segment of process `{process_id}` ended ({end:?}) before its crash fired"
                ),
            },
            ran = &mut call => panic!(
                "the invocation of process `{process_id}` completed before its crash fired: {ran:?}"
            ),
        }
        self.crashed_segments.lock().await.insert(
            process_id.clone(),
            CrashedSegment {
                process_id,
                registration,
                call,
            },
        );
    }

    /// `Replay` lets Restate deliver the crashed invocation again, replaying
    /// its journal. `SubstrateLost` kills that invocation and purges it, as
    /// retention does, and submits the segment afresh under the same key.
    async fn recover_segment(
        &self,
        process_id: &lash_core::ProcessId,
        recovery: lash_conformance::SegmentRecovery,
        body: lash_conformance::ConformanceTurnAttempt,
    ) {
        let crashed = self
            .crashed_segments
            .lock()
            .await
            .remove(process_id)
            .unwrap_or_else(|| panic!("process `{process_id}` has a crashed segment to recover"));
        let segments = self.process_runner.segments();
        let ran = match recovery {
            lash_conformance::SegmentRecovery::Replay => {
                segments.serve(process_id, Some(body), None, None);
                tokio::time::timeout(SEGMENT_RECOVERY_TIMEOUT, crashed.call)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("the replayed segment of process `{process_id}` ended")
                    })
                    .expect("the segment's ingress call task")
            }
            lash_conformance::SegmentRecovery::SubstrateLost => {
                let key = crate::process::process_segment_workflow_key(process_id, 0);
                // The kill must have stopped the crashed execution for
                // real — its attempt's task ended, not just been aborted —
                // before the body is served: an abort lets a poll in flight
                // run to its next yield, and that poll would find the body.
                let killed = self
                    .admin
                    .kill_workflow_run(crate::LashService::ProcessWorkflow.name(), &key)
                    .await;
                let _ = crashed.call.await;
                self.admin.purge_invocation(&killed).await;
                // Only now may an execution find the body: the killed
                // invocation's retry must never run it.
                segments.serve(process_id, Some(body), None, None);
                tokio::time::timeout(
                    SEGMENT_RECOVERY_TIMEOUT,
                    self.submit_segment(&crashed.process_id, &crashed.registration),
                )
                .await
                .unwrap_or_else(|_| panic!("the fresh segment of process `{process_id}` ended"))
                .expect("the segment's ingress call task")
            }
        };
        assert!(
            ran.is_ok(),
            "the recovered segment of process `{process_id}` ended its invocation: {ran:?}"
        );
    }

    /// Process segments run in the endpoint's `LashProcessWorkflow`: the
    /// worker is installed there, and the runtime's own port only observes
    /// the registry that workflow writes terminals into.
    fn process_work(
        &self,
        watched: lash_core::WatchedRegistry,
        worker: lash_core_worker::DurableProcessWorker,
    ) -> lash_core::ProcessWorkWiring {
        self.process_runner.install(worker);
        let port = std::sync::Arc::new(lash_core::NativeProcessWork::for_registry(
            std::sync::Arc::clone(watched.registry()),
        ));
        lash_core::ProcessWorkWiring::new(watched, port)
    }
}
