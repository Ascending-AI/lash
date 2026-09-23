//! Live Restate turn runner for the turn-driving conformance laws.
//!
//! A Restate turn runs only inside a handler: its effects journal on a
//! `ctx`-bound [`RestateRuntimeEffectController`](crate::RestateRuntimeEffectController),
//! and the deployment host refuses every effect that has not entered one. The
//! live suite's endpoint serves in this test process, so a law hands its turn
//! to [`LiveTurnRunner`], which parks the job in a process-local table and
//! invokes [`ConformanceTurnProbe`] through ingress; the handler takes the job
//! back and runs it on its own controller. The tool calls of that turn open
//! real Restate effect groups whose children run in the endpoint's dispatch
//! invocations.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use crate::RestateIngressClient;

/// What a parked turn runs. A plain law's job runs once. A crash-redrive
/// law's attempts are factories, because Restate re-runs the handler from the
/// top on every replay: the crashing attempt runs until it has crashed once,
/// then every later execution runs the redrive, which replays the crashed
/// attempt's journal.
enum ParkedAttempts {
    Once(Option<lash_conformance::ConformanceTurnJob>),
    CrashThenRedrive {
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
        crashed: bool,
    },
}

type PendingTurn = (lash_core::AdmittedScope, ParkedAttempts);

fn pending_turns() -> &'static Mutex<HashMap<String, PendingTurn>> {
    static TURNS: OnceLock<Mutex<HashMap<String, PendingTurn>>> = OnceLock::new();
    TURNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The workflow whose handler runs one parked conformance turn.
#[restate_sdk::workflow]
pub(super) trait ConformanceTurnProbe {
    async fn run(key: Json<String>) -> HandlerResult<Json<bool>>;
}

pub(super) struct ConformanceTurnProbeImpl;

impl ConformanceTurnProbe for ConformanceTurnProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(key): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let next = {
            let mut turns = pending_turns()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            turns
                .get_mut(&key)
                .and_then(|(admitted, attempts)| match attempts {
                    // Taken, not borrowed: a plain job runs once.
                    ParkedAttempts::Once(job) => {
                        job.take().map(|job| (admitted.clone(), job, false))
                    }
                    ParkedAttempts::CrashThenRedrive {
                        crashing,
                        redrive,
                        crashed,
                    } => {
                        let attempt = Arc::clone(if *crashed { redrive } else { crashing });
                        let job: lash_conformance::ConformanceTurnJob =
                            Box::new(move |scoped| attempt(scoped));
                        Some((admitted.clone(), job, !*crashed))
                    }
                })
        };
        // An invocation that finds nothing to run fails terminally rather
        // than silently succeeding without the turn it was asked to run.
        let Some((admitted, job, crashing)) = next else {
            return Err(TerminalError::new(format!(
                "conformance turn `{key}` is not parked in this process; the probe \
                 handler was re-invoked after its job already ran"
            ))
            .into());
        };
        let controller = crate::RestateRuntimeEffectController::new_for_test(ctx);
        let scoped = controller
            .scoped_effect_controller(admitted)
            .map_err(TerminalError::from_error)?;
        match (CatchUnwind { inner: job(scoped) }).await {
            Ok(()) => Ok(Json(true)),
            // The crashing attempt died as the law asked: fail retryably, so
            // Restate redelivers the invocation and the redrive replays this
            // attempt's journal — the way a deployment recovers a turn whose
            // handler died.
            Err(()) if crashing => {
                if let Some((_, ParkedAttempts::CrashThenRedrive { crashed, .. })) = pending_turns()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&key)
                {
                    *crashed = true;
                }
                Err(HandlerError::from(std::io::Error::other(format!(
                    "conformance turn `{key}` crashed; Restate redelivers it to the redrive"
                ))))
            }
            // Any other panic in the law must fail the invocation terminally:
            // an unwinding handler reads as retryable, and the retry would
            // find no job.
            Err(()) => Err(TerminalError::new(format!(
                "conformance turn `{key}` panicked inside the probe handler"
            ))
            .into()),
        }
    }
}

/// Polls a future under `catch_unwind`, turning a panic into `Err(())`. The
/// payload is already on stderr through the panic hook.
struct CatchUnwind<'a> {
    inner: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
}

impl Future for CatchUnwind<'_> {
    type Output = Result<(), ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

/// Runs each conformance turn inside a [`ConformanceTurnProbe`] handler on the
/// live endpoint.
pub(super) struct LiveTurnRunner {
    ingress_url: String,
    process_runner: std::sync::Arc<super::effect_group_conformance::LawProcessRunner>,
}

impl LiveTurnRunner {
    pub(super) fn new(
        ingress_url: String,
        process_runner: std::sync::Arc<super::effect_group_conformance::LawProcessRunner>,
    ) -> std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner> {
        std::sync::Arc::new(Self {
            ingress_url,
            process_runner,
        })
    }

    async fn run_parked(&self, admitted: lash_core::AdmittedScope, attempts: ParkedAttempts) {
        let key = format!(
            "turn-probe-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after the epoch")
                .as_nanos()
        );
        pending_turns()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), (admitted, attempts));
        let ran = RestateIngressClient::new(self.ingress_url.clone())
            .call_workflow_json::<_, bool>("ConformanceTurnProbe", &key, "run", &key)
            .await;
        let parked = pending_turns()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        assert!(
            matches!(ran, Ok(true)),
            "the live conformance turn `{key}` did not complete in its handler: {ran:?}"
        );
        if let Some((_, ParkedAttempts::CrashThenRedrive { crashed, .. })) = parked {
            assert!(
                crashed,
                "the live conformance turn `{key}` completed without its crashing attempt crashing"
            );
        }
    }
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for LiveTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        job: lash_conformance::ConformanceTurnJob,
    ) {
        self.run_parked(admitted, ParkedAttempts::Once(Some(job)))
            .await;
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.run_parked(
            admitted,
            ParkedAttempts::CrashThenRedrive {
                crashing,
                redrive,
                crashed: false,
            },
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
