//! The ready-made lash backend on the server double: lash-restate's engine
//! over a SQLite memory store set (storage only), with every lash-restate
//! service bound on one endpoint that the in-process server serves.
//!
//! [`backend`] is the one constructor every fixture funnels through. Its
//! shape follows the engine/storage split: the stores are built here, the
//! engine is lash-restate's, and the server double stands in for
//! `restate-server`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::testing::TestClock;
use lash_core::{AdmittedScope, ScopedEffectController, StoreSet};
use lash_core_worker::DurableProcessWorker;
use lash_restate::{
    RestateAuthorityId, RestateBackend, RestateConnection, RestateHttpError, RestateIngressClient,
    RestateProcessWorkerSlot,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use crate::server::{CrashPoint, CrashRule, RestateTestServer, ServerConfig, StartError};

/// The Restate service that drives a session: lash-restate's `LashSession`
/// object. A crash rule on it cuts the admission journal.
pub const SESSION_DRIVER_SERVICE: &str = "LashSession";

/// The Restate service that runs one admitted root: lash-restate's `LashTurn`
/// workflow. A crash rule on it cuts the root's journal.
pub const TURN_DRIVER_SERVICE: &str = "LashTurn";

/// One handler execution's run of an attempt.
type HandlerJob = Box<
    dyn for<'a> FnOnce(ScopedEffectController<'a>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send,
>;

/// One attempt of a job that the handler may run more than once: Restate
/// re-runs a handler from the top on every replay, so it is a factory.
pub type HandlerAttempt = Arc<
    dyn for<'a> Fn(ScopedEffectController<'a>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send
        + Sync,
>;

/// Why a backend could not be built.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(transparent)]
    Server(#[from] StartError),
    #[error("the SQLite memory store set could not open: {0}")]
    Stores(String),
    #[error("the Restate authority id is invalid: {0}")]
    Authority(String),
}

/// A server double with lash-restate's engine wired to it: the handle a test
/// owns.
///
/// The handle owns the server; a runtime is built over
/// [`lash_backend`](Self::lash_backend), the engine's own backend, which
/// reaches the server only through its connection, the way a deployment's
/// backend reaches `restate-server`. Building a core over the handle itself
/// would let the core's process worker — which the handle's deployment
/// serves segments with, and which holds the core's backend — keep the
/// server alive forever, so the handle is not a backend (FIG-3723). Effects
/// that must run inside a Restate handler (a turn's) enter one through
/// [`run_in_handler`](Self::run_in_handler).
#[derive(Clone)]
pub struct RestateTestBackend {
    server: RestateTestServer,
    restate: Arc<RestateBackend>,
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    clock: Arc<TestClock>,
    connection: RestateConnection,
    processes: RestateProcessWorkerSlot,
    jobs: Arc<ParkedJobs>,
}

impl std::fmt::Debug for RestateTestBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateTestBackend")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

/// The one constructor: a fresh server double under `seed` with `config`,
/// a fresh SQLite memory store set on a virtual clock the server moves, and
/// lash-restate's engine and services wired between them.
///
/// `seed` is the run's seed and replaces whatever `config.seed` holds; pass
/// `ServerConfig::default()` unless a test needs another time mode, protocol
/// version, retry policy or always-replay.
pub async fn backend(seed: u64, config: ServerConfig) -> Result<RestateTestBackend, BackendError> {
    RestateTestBackend::build(config.with_seed(seed)).await
}

impl RestateTestBackend {
    async fn build(config: ServerConfig) -> Result<Self, BackendError> {
        let clock = Arc::new(TestClock::new(config.start_time_ms));
        let server = RestateTestServer::new(config)?;
        let follower = Arc::clone(&clock);
        server.on_time_moved(Arc::new(move |now_ms| follower.set(now_ms)));
        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_clock(
                Arc::clone(&clock) as Arc<dyn lash_core::Clock>
            )
            .await
            .map_err(|error| BackendError::Stores(error.to_string()))?,
        );
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let authority =
            RestateAuthorityId::new(format!("lash-restate-test-{}", server.config().seed))
                .map_err(|error| BackendError::Authority(error.to_string()))?;
        let restate = Arc::new(RestateBackend::new(
            connection.clone(),
            authority.clone(),
            Arc::clone(&stores) as Arc<dyn StoreSet>,
        ));
        // The endpoint exists before any core over this backend does, so it
        // serves processes on whatever worker the fixture installs later.
        let processes = RestateProcessWorkerSlot::new();
        let jobs = Arc::new(ParkedJobs::default());
        let endpoint = restate
            .endpoint_builder(processes.clone())
            .bind(HandlerHost {
                jobs: Arc::clone(&jobs),
                authority,
            })
            .build();
        server.register(endpoint).await?;
        Ok(Self {
            server,
            restate,
            stores,
            clock,
            connection,
            processes,
            jobs,
        })
    }

    /// The server double: time, crashes, operator commands, introspection.
    pub fn server(&self) -> &RestateTestServer {
        &self.server
    }

    /// The backend a runtime runs on: lash-restate's engine over the store
    /// set, connected to the server double. Hand it to a core wherever a
    /// test used `SqliteBackend::memory()`.
    pub fn lash_backend(&self) -> Arc<dyn lash_core::Backend> {
        Arc::clone(&self.restate) as Arc<dyn lash_core::Backend>
    }

    /// lash-restate's own backend value, for APIs that name it.
    pub fn restate(&self) -> &Arc<RestateBackend> {
        &self.restate
    }

    /// The storage-only store set under the engine.
    pub fn stores(&self) -> &Arc<lash_sqlite_store::SqliteStoreSet> {
        &self.stores
    }

    /// The virtual clock the stores stamp with; the server moves it.
    pub fn test_clock(&self) -> Arc<TestClock> {
        Arc::clone(&self.clock)
    }

    /// A connection to the server double, for Restate clients a test builds.
    pub fn connection(&self) -> RestateConnection {
        self.connection.clone()
    }

    /// The ingress client over [`connection`](Self::connection).
    pub fn ingress(&self) -> RestateIngressClient {
        RestateIngressClient::new(self.connection.clone())
    }

    /// Drop the next execution of a session's drive handler (`LashSession`)
    /// at `point`, on its first attempt, and let the server replay it: the
    /// crash cuts the drive's admission journal.
    pub fn crash_session_drive(&self, point: CrashPoint) {
        self.server.crash_on(
            CrashRule::new(point)
                .service(SESSION_DRIVER_SERVICE)
                .within_attempts(1),
        );
    }

    /// Drop the next execution of an admitted root's handler (`LashTurn`) at
    /// `point`, on its first attempt, and let the server replay it: the crash
    /// cuts the root's journal.
    pub fn crash_turn_drive(&self, point: CrashPoint) {
        self.server.crash_on(
            CrashRule::new(point)
                .service(TURN_DRIVER_SERVICE)
                .within_attempts(1),
        );
    }

    /// Attach to `request`'s drive of `session` on the engine and return how
    /// it ended. The drive is the one the request's schedule sent, or, when
    /// none was sent, one this call starts under the same idempotency key.
    pub async fn attach_drive(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::DriveRequestId,
    ) -> Result<lash_core::engine::DriveOutcome, RestateHttpError> {
        self.restate
            .session_work_engine()
            .attach_drive(session, request)
            .await
    }

    /// Serve process segments with `worker`, the process worker of a core
    /// built over this backend. Until a worker is installed, a process
    /// segment fails naming the empty worker slot.
    pub fn install_process_worker(&self, worker: DurableProcessWorker) {
        self.processes.install(worker);
    }

    /// Run `job` inside a workflow handler on the server, on the handler's
    /// scoped controller for `admitted` — where a Restate deployment runs a
    /// turn. Returns once the handler completed.
    ///
    /// Interim turn entry: superseded by the engine's session work (the
    /// `LashSession` drive of FIG-3664's S5), after which a turn enters its
    /// handler through the backend itself and this goes away.
    ///
    /// `attempt` runs on every execution of the handler — Restate re-runs a
    /// handler from the top whenever it replays the invocation (after a
    /// suspension, a retry or a simulated crash) — so it must issue the same
    /// journaled commands each time, as a turn under one turn id does.
    pub async fn run_in_handler(
        &self,
        admitted: AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        self.run_parked(admitted, Parked::Replayed(attempt)).await
    }

    /// Run `crashing` inside a handler until it panics, which fails the
    /// attempt retryably as a dying deployment does; the server then replays
    /// the invocation into `redrive`. Returns an error if `crashing` never
    /// panicked.
    pub async fn run_crashed_then_redriven(
        &self,
        admitted: AdmittedScope,
        crashing: HandlerAttempt,
        redrive: HandlerAttempt,
    ) -> Result<(), String> {
        let key = self
            .run_parked_keyed(
                admitted,
                Parked::CrashThenRedrive {
                    crashing,
                    redrive,
                    crashed: false,
                },
            )
            .await?;
        match self.jobs.take(&key) {
            Some((_, Parked::CrashThenRedrive { crashed: true, .. })) | None => Ok(()),
            Some(_) => Err(format!(
                "job `{key}` completed without its crashing attempt crashing"
            )),
        }
    }

    async fn run_parked(&self, admitted: AdmittedScope, parked: Parked) -> Result<(), String> {
        let key = self.run_parked_keyed(admitted, parked).await?;
        self.jobs.take(&key);
        Ok(())
    }

    async fn run_parked_keyed(
        &self,
        admitted: AdmittedScope,
        parked: Parked,
    ) -> Result<String, String> {
        let key = self.jobs.park(admitted, parked);
        let ingress = self.ingress();
        let call = ingress.call_workflow_json::<_, bool>(HANDLER_HOST, &key, "run", &key);
        // A job whose handler exhausted its retries is paused, not failed:
        // report it at once instead of waiting out the attach ceiling.
        let target = format!("{HANDLER_HOST}/{key}/run");
        let paused = async {
            loop {
                if let Some(view) = self
                    .server
                    .invocations()
                    .into_iter()
                    .find(|view| view.target == target && view.status == "paused")
                {
                    return view;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        let ran = tokio::select! {
            ran = call => ran,
            view = paused => {
                self.jobs.take(&key);
                return Err(format!(
                    "job `{key}` paused after {} attempts; last failure: {:?}",
                    view.attempts, view.last_failure
                ));
            }
        };
        match ran {
            Ok(true) => Ok(key),
            Ok(false) => Err(format!("job `{key}` reported failure")),
            Err(error) => {
                self.jobs.take(&key);
                Err(format!(
                    "job `{key}` did not complete in its handler: {error}"
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The handler host
// ---------------------------------------------------------------------------

const HANDLER_HOST: &str = "LashTestHandlerHost";

enum Parked {
    Replayed(HandlerAttempt),
    CrashThenRedrive {
        crashing: HandlerAttempt,
        redrive: HandlerAttempt,
        crashed: bool,
    },
}

#[derive(Default)]
struct ParkedJobs {
    next: AtomicU64,
    jobs: Mutex<HashMap<String, (AdmittedScope, Parked)>>,
}

impl ParkedJobs {
    fn park(&self, admitted: AdmittedScope, parked: Parked) -> String {
        let key = format!("job-{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), (admitted, parked));
        key
    }

    fn take(&self, key: &str) -> Option<(AdmittedScope, Parked)> {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key)
    }

    /// The job to run for `key` on this handler execution, and whether it is
    /// the crashing attempt.
    fn next_run(&self, key: &str) -> Option<(AdmittedScope, HandlerJob, bool)> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        let (admitted, parked) = jobs.get_mut(key)?;
        match parked {
            Parked::Replayed(attempt) => {
                let attempt = Arc::clone(attempt);
                let job: HandlerJob = Box::new(move |scoped| attempt(scoped));
                Some((admitted.clone(), job, false))
            }
            Parked::CrashThenRedrive {
                crashing,
                redrive,
                crashed,
            } => {
                let attempt = Arc::clone(if *crashed { redrive } else { crashing });
                let job: HandlerJob = Box::new(move |scoped| attempt(scoped));
                Some((admitted.clone(), job, !*crashed))
            }
        }
    }

    fn mark_crashed(&self, key: &str) {
        if let Some((_, Parked::CrashThenRedrive { crashed, .. })) = self
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(key)
        {
            *crashed = true;
        }
    }
}

/// The workflow a job runs in. One key per job; the handler takes the parked
/// job and runs it on its own `ctx`-bound controller.
struct HandlerHost {
    jobs: Arc<ParkedJobs>,
    authority: RestateAuthorityId,
}

#[restate_sdk::workflow(name = "LashTestHandlerHost")]
impl HandlerHost {
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(key): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let Some((admitted, job, crashing)) = self.jobs.next_run(&key) else {
            return Err(TerminalError::new(format!(
                "job `{key}` is not parked on this backend; its handler was re-invoked after it ran"
            ))
            .into());
        };
        let controller =
            lash_restate::RestateRuntimeEffectController::new(ctx, self.authority.clone());
        let scoped = controller
            .scoped_effect_controller(admitted)
            .map_err(TerminalError::from_error)?;
        match (CatchUnwind { inner: job(scoped) }).await {
            Ok(()) => Ok(Json(true)),
            Err(()) if crashing => {
                self.jobs.mark_crashed(&key);
                Err(HandlerError::from(std::io::Error::other(format!(
                    "job `{key}` crashed; the server replays it into the redrive"
                ))))
            }
            Err(()) => {
                Err(TerminalError::new(format!("job `{key}` panicked in its handler")).into())
            }
        }
    }
}

/// Polls a future under `catch_unwind`, turning a panic into `Err(())`.
struct CatchUnwind<'a> {
    inner: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
}

impl Future for CatchUnwind<'_> {
    type Output = Result<(), ()>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let inner = &mut self.inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    }
}
