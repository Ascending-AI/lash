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
        let (admitted, attempt, crashing, ends) = loop {
            let queued = attempt_queued().notified();
            tokio::pin!(queued);
            queued.as_mut().enable();
            match next_attempt(&key) {
                NextAttempt::Run {
                    admitted,
                    attempt,
                    crashing,
                    ends,
                } => break (admitted, attempt, crashing, ends),
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
        let (end, result) = match (CatchUnwind {
            inner: attempt(scoped),
        })
        .await
        {
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
        finish_attempt(&key);
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

/// Runs each conformance turn inside a [`ConformanceTurnProbe`] handler on the
/// live endpoint.
pub(super) struct LiveTurnRunner {
    connection: crate::RestateConnection,
    process_runner: std::sync::Arc<super::effect_group_conformance::LawProcessRunner>,
    /// The invocations a law left open, by scope: each one's probe key and
    /// its ingress call, which returns once the invocation completes.
    open: tokio::sync::Mutex<HashMap<String, OpenInvocation>>,
}

struct OpenInvocation {
    key: String,
    call: tokio::task::JoinHandle<Result<bool, crate::RestateHttpError>>,
}

impl LiveTurnRunner {
    pub(super) fn shared(
        connection: crate::RestateConnection,
        process_runner: std::sync::Arc<super::effect_group_conformance::LawProcessRunner>,
    ) -> std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner> {
        std::sync::Arc::new(Self {
            connection,
            process_runner,
            open: tokio::sync::Mutex::default(),
        })
    }

    /// Runs `attempts` as the next attempts of `admitted`'s invocation: a new
    /// invocation, or the retry of the one a parked or aborted run left open.
    /// Returns once the turn settled (the invocation completed) or aborted
    /// (the invocation stays open for the law's next run of the scope).
    async fn run_attempts(&self, admitted: lash_core::AdmittedScope, attempts: Vec<QueuedAttempt>) {
        let scope = format!("{:?}", admitted.scope());
        let crash_expected = attempts.iter().any(|attempt| attempt.crashing);
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
        loop {
            tokio::select! {
                // The handler reports its attempt's end before it returns, so
                // when both are ready the report is read first; an unbiased
                // pick could take the finished call and miss the report.
                biased;
                end = ended.recv() => match end {
                    Some(AttemptEnd::Crashed) => crashed = true,
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
                    Some(AttemptEnd::Aborted) => {
                        open.insert(scope, OpenInvocation { key: key.clone(), call });
                        break;
                    }
                    None => panic!("the live conformance turn `{key}` lost its attempt channel"),
                },
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
                },
                QueuedAttempt {
                    attempt: redrive,
                    crashing: false,
                },
            ],
        )
        .await;
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
