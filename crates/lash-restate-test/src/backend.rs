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
use lash_core::{AdmittedScope, Backend as _, ScopedEffectController, StoreSet};
use lash_core_worker::DurableProcessWorker;
use lash_restate::{
    LashDurableWaitIndex as _, LashDurableWaitWorkflow as _, LashProcessAttach as _,
    LashProcessAttachImpl, LashProcessWorkflow as _, LashProcessWorkflowImpl, RestateAuthorityId,
    RestateBackend, RestateConnection, RestateCoreProcessRunner, RestateEffectGroupRetryPolicy,
    RestateEffectGroupServices, RestateIngressClient, RestateProcessCancelRequest,
    RestateProcessRunner, RestateQueuedWork, SegmentStarted,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use tokio_util::sync::CancellationToken;

use crate::server::{RestateTestServer, ServerConfig, StartError};

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
    #[error("the endpoint does not bind every lash service: {0}")]
    Binding(String),
}

/// A lash backend whose effect engine is lash-restate on the server double.
///
/// It is a [`lash_core::Backend`]: hand it to a runtime wherever a test used
/// `SqliteBackend::memory()`. Effects that must run inside a Restate handler
/// (a turn's) enter one through [`run_in_handler`](Self::run_in_handler).
#[derive(Clone)]
pub struct RestateTestBackend {
    server: RestateTestServer,
    restate: Arc<RestateBackend>,
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    clock: Arc<TestClock>,
    connection: RestateConnection,
    processes: Arc<DeploymentProcessRunner>,
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
            RestateQueuedWork::Disabled,
        ));
        let processes = Arc::new(DeploymentProcessRunner::default());
        let jobs = Arc::new(ParkedJobs::default());
        let endpoint = deployment_endpoint(
            &restate,
            &stores,
            &connection,
            &authority,
            &processes,
            &jobs,
        );
        restate
            .assert_endpoint_bound(&endpoint)
            .await
            .map_err(|error| BackendError::Binding(error.to_string()))?;
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

    /// Serve process segments with `worker`, as a deployment's
    /// `RestateCoreProcessRunner` does. Until a worker is installed, a
    /// process segment fails terminally naming the missing worker.
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

impl lash_core::Backend for RestateTestBackend {
    fn binding_identity(&self) -> &str {
        self.restate.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.restate.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core::SessionStoreFactory> {
        self.restate.session_store_factory()
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        lash_core::Backend::effect_host(self.restate.as_ref())
    }

    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.restate.process_registry()
    }

    fn trigger_store(&self) -> Arc<dyn lash_core::TriggerStore> {
        self.restate.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash_core::ProcessDefinitionRegistry> {
        self.restate.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.restate.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.restate.attachment_store()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        self.restate.process_work()
    }

    fn queued_work(&self) -> lash_core::BackendQueuedWork {
        self.restate.queued_work()
    }
}

/// The endpoint a lash deployment binds, plus the handler host that runs
/// test jobs.
fn deployment_endpoint(
    restate: &Arc<RestateBackend>,
    stores: &Arc<lash_sqlite_store::SqliteStoreSet>,
    connection: &RestateConnection,
    authority: &RestateAuthorityId,
    processes: &Arc<DeploymentProcessRunner>,
    jobs: &Arc<ParkedJobs>,
) -> Endpoint {
    let groups = RestateEffectGroupServices::new(
        restate.effect_host().as_ref(),
        RestateIngressClient::new(connection.clone()),
        RestateEffectGroupRetryPolicy::infinite(),
        stores.session_store_factory(),
    );
    let process_workflow = LashProcessWorkflowImpl::new(
        Arc::clone(processes),
        restate.process_registry(),
        stores.process_continuations(),
        RestateIngressClient::new(connection.clone()),
        authority.clone(),
    );
    Endpoint::builder()
        .bind(process_workflow.serve())
        .bind(LashProcessAttachImpl.serve())
        .bind(groups.index)
        .bind(groups.payload)
        .bind(groups.dispatch)
        .bind(groups.wait.workflow.serve())
        .bind(groups.wait.index.serve())
        .bind(HandlerHost {
            jobs: Arc::clone(jobs),
            authority: authority.clone(),
        })
        .build()
}

/// The process runner the endpoint's `LashProcessWorkflow` serves segments
/// with: the runtime's worker, installed once the runtime exists.
#[derive(Default)]
struct DeploymentProcessRunner {
    installed: Mutex<Option<RestateCoreProcessRunner>>,
}

impl DeploymentProcessRunner {
    fn install(&self, worker: DurableProcessWorker) {
        *self
            .installed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(RestateCoreProcessRunner::new(worker));
    }

    fn installed(&self) -> Result<RestateCoreProcessRunner, lash_core::PluginError> {
        self.installed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                lash_core::PluginError::Invoke(
                    "no process worker is installed on the lash-restate-test deployment; call \
                     RestateTestBackend::install_process_worker"
                        .to_owned(),
                )
            })
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for DeploymentProcessRunner {
    fn replay_key_grammar(&self, registration: &lash_core::ProcessRegistration) -> Option<u32> {
        self.installed()
            .ok()
            .and_then(|runner| runner.replay_key_grammar(registration))
    }

    async fn run_process_segment(
        &self,
        started: &SegmentStarted,
        registration: lash_core::ProcessRegistration,
        execution_context: lash_core::ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        let runner = self.installed()?;
        Box::pin(runner.run_process_segment(
            started,
            registration,
            execution_context,
            scoped_effect_controller,
            handover,
            cancellation,
        ))
        .await
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), lash_core::PluginError> {
        self.installed()?.request_process_cancel(request).await
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
