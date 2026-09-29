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
use lash_core::{
    AdmittedScope, ScopedEffectController, SessionDriver, SessionWorkEngine, StoreSet,
};
use lash_core_worker::DurableProcessWorker;
use lash_restate::{
    RestateAuthorityId, RestateConfig, RestateConnection, RestateEngine, RestateHttpError,
    RestateIngressClient, RestateNamespace, RestateProcessServing, RestateProcessWorkerSlot,
    RestateRegistrationError, RestateSessionWork, turn_workflow_key,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use crate::server::{
    CrashPoint, CrashRule, DeploymentHooks, DeploymentId, RestateTestServer, ServerConfig,
    StartError,
};

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
    /// The engine refused to register its deployment: another deployment on
    /// the server serves its names (FIG-3898).
    #[error(transparent)]
    Registration(Box<RestateRegistrationError>),
    /// The handler host's name in the backend's namespace is not a Restate
    /// service name.
    #[error("the handler host's name is not a Restate service name: {0}")]
    HandlerHostName(String),
}

impl From<RestateRegistrationError> for BackendError {
    fn from(error: RestateRegistrationError) -> Self {
        Self::Registration(Box::new(error))
    }
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
    restate: Arc<RestateEngine>,
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    engine_stores: Arc<dyn StoreSet>,
    clock: Arc<TestClock>,
    connection: RestateConnection,
    processes: RestateProcessWorkerSlot,
    jobs: Arc<ParkedJobs>,
    loans: Arc<crate::open_handler::Loans>,
    authority: RestateAuthorityId,
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
    backend_with(seed, config, |stores| stores).await
}

/// [`backend`] over a decorated store set: `decorate_stores` wraps the SQLite
/// memory store set before the engine is built, so the engine, its process
/// deployment and every service it binds run over the decorated stores. A
/// layer added after the engine exists reaches only the ports read through
/// the backend, not the stores the engine's own services hold.
pub async fn backend_with(
    seed: u64,
    config: ServerConfig,
    decorate_stores: impl FnOnce(Arc<dyn StoreSet>) -> Arc<dyn StoreSet>,
) -> Result<RestateTestBackend, BackendError> {
    RestateTestBackend::build(
        config.with_seed(seed),
        None,
        "",
        DeploymentHooks::default(),
        decorate_stores,
    )
    .await
}

/// [`backend`] with a `label` and deployment `hooks` on the first build, so
/// a test can tell it apart from the builds [`add_build`] adds and refuse
/// or record its dispatches the same way.
///
/// [`add_build`]: RestateTestBackend::add_build
pub async fn backend_with_build(
    seed: u64,
    config: ServerConfig,
    label: impl Into<String>,
    hooks: DeploymentHooks,
) -> Result<RestateTestBackend, BackendError> {
    RestateTestBackend::build(config.with_seed(seed), None, label, hooks, |stores| stores).await
}

/// [`backend`], whose endpoint cuts every process segment after
/// `segment_effect_budget` completed effects instead of the default 10,000,
/// so a short process crosses segment boundaries.
pub async fn backend_with_segment_budget(
    seed: u64,
    config: ServerConfig,
    segment_effect_budget: u64,
) -> Result<RestateTestBackend, BackendError> {
    RestateTestBackend::build(
        config.with_seed(seed),
        Some(segment_effect_budget),
        "",
        DeploymentHooks::default(),
        |stores| stores,
    )
    .await
}

impl RestateTestBackend {
    async fn build(
        config: ServerConfig,
        segment_effect_budget: Option<u64>,
        first_label: impl Into<String>,
        first_hooks: DeploymentHooks,
        decorate_stores: impl FnOnce(Arc<dyn StoreSet>) -> Arc<dyn StoreSet>,
    ) -> Result<Self, BackendError> {
        let clock = Arc::new(TestClock::new(config.start_time_ms));
        let server = RestateTestServer::new(config)?;
        let follower = Arc::clone(&clock);
        server.on_time_moved(Arc::new(move |now_ms| follower.set(now_ms)));
        Self::build_on(
            server,
            clock,
            false,
            RestateNamespace::default(),
            segment_effect_budget,
            first_label,
            first_hooks,
            decorate_stores,
        )
        .await
    }

    /// Another lash deployment on this backend's server, in `namespace`
    /// (FIG-3898): its own engine over a fresh SQLite memory store set on the
    /// server's clock, under an authority of its own, with every lash
    /// service and the handler host bound under `namespace`'s names. The two
    /// share nothing but the server. Its endpoint registers through
    /// [`RestateEngine::register_deployment`], so a namespace another
    /// deployment on the server already serves is refused with
    /// [`BackendError::Registration`]. The open-handler lender is bound only
    /// on the server's first backend.
    pub async fn beside(
        &self,
        namespace: RestateNamespace,
        label: impl Into<String>,
    ) -> Result<Self, BackendError> {
        Self::build_on(
            self.server.clone(),
            Arc::clone(&self.clock),
            true,
            namespace,
            None,
            label,
            DeploymentHooks::default(),
            |stores| stores,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::fn_params_excessive_bools,
        reason = "the server's first backend and one beside it differ in every one"
    )]
    async fn build_on(
        server: RestateTestServer,
        clock: Arc<TestClock>,
        beside: bool,
        namespace: RestateNamespace,
        segment_effect_budget: Option<u64>,
        first_label: impl Into<String>,
        first_hooks: DeploymentHooks,
        decorate_stores: impl FnOnce(Arc<dyn StoreSet>) -> Arc<dyn StoreSet>,
    ) -> Result<Self, BackendError> {
        let first_label = first_label.into();
        // Sequential test IDs keep process names simple within a run. Attempt
        // interleaving is concurrent and can change which process gets an ID.
        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(
                lash_sqlite_store::SqliteStoreSetOptions {
                    process_id_mint: lash_core::ProcessIdMint::sequential_for_testing(),
                    ..lash_sqlite_store::SqliteStoreSetOptions::memory()
                },
                Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
            )
            .await
            .map_err(|error| BackendError::Stores(error.to_string()))?,
        );
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        // The server's first deployment journals under the seed's authority;
        // a deployment beside it under one of its own.
        let authority = RestateAuthorityId::new(if beside {
            format!("lash-restate-test-{}-{first_label}", server.config().seed)
        } else {
            format!("lash-restate-test-{}", server.config().seed)
        })
        .map_err(|error| BackendError::Authority(error.to_string()))?;
        let engine_stores = decorate_stores(Arc::clone(&stores) as Arc<dyn StoreSet>);
        let restate = Arc::new(RestateEngine::new(
            Arc::clone(&engine_stores),
            RestateConfig::new(
                connection.clone(),
                connection.clone(),
                authority.clone(),
                server.config().build_generation.clone(),
            )
            .with_namespace(namespace.clone()),
        ));
        // The endpoint exists before any core over this backend does, so it
        // serves processes on whatever worker the fixture installs later.
        let processes = RestateProcessWorkerSlot::new();
        let jobs = Arc::new(ParkedJobs::default());
        let serving = RestateProcessServing::from(processes.clone());
        let serving = match segment_effect_budget {
            Some(budget) => serving.with_segment_effect_budget_selector(move |_| budget),
            None => serving,
        };
        let loans = Arc::new(crate::open_handler::Loans::default());
        let builder = bind_handler_host(
            restate.endpoint_builder(serving),
            HandlerHost {
                jobs: Arc::clone(&jobs),
                authority: authority.clone(),
                namespace: namespace.clone(),
            },
        )
        .map_err(BackendError::HandlerHostName)?;
        let builder = if namespace.is_default() {
            builder.bind(crate::open_handler::HandlerLender {
                loans: Arc::clone(&loans),
            })
        } else {
            builder
        };
        // An in-process deployment is registered through the Rust API; the
        // engine's registration still runs its name check against the
        // server's admin API first, and the double acknowledges the admin
        // registration it then sends.
        restate
            .register_deployment(&format!("restate-test:{}", deployment_name(&first_label)))
            .await?;
        server
            .register_with(builder.build(), first_label, first_hooks)
            .await?;
        Ok(Self {
            server,
            restate,
            stores,
            engine_stores,
            clock,
            connection,
            processes,
            jobs,
            loans,
            authority,
        })
    }

    /// Register another build of this backend's services on the server: a
    /// second deployment of drain generation `generation` over the same
    /// stores, effect host and session driver, under the opaque `label`.
    /// Each lash journal-bearing service is bound under its stable name and
    /// under `generation`'s lane (FIG-3795), so a new invocation of a stable
    /// name routes to the newest build, one of a generation lane only to a
    /// build of that generation, and one already in flight stays pinned to
    /// the build that started it. `hooks` are the test's levers on this
    /// build — refuse a handler call, record that this build served one.
    pub async fn add_build(
        &self,
        generation: lash_core::engine::BuildGeneration,
        label: impl Into<String>,
        hooks: DeploymentHooks,
    ) -> Result<DeploymentId, BackendError> {
        let builder = bind_handler_host(
            self.restate
                .sibling_build(generation)
                .endpoint_builder(self.processes.clone()),
            HandlerHost {
                jobs: Arc::clone(&self.jobs),
                authority: self.authority.clone(),
                namespace: self.namespace().clone(),
            },
        )
        .map_err(BackendError::HandlerHostName)?;
        let builder = if self.namespace().is_default() {
            builder.bind(crate::open_handler::HandlerLender {
                loans: Arc::clone(&self.loans),
            })
        } else {
            builder
        };
        Ok(self
            .server
            .register_with(builder.build(), label, hooks)
            .await?)
    }

    /// The namespace this backend's services are named in (FIG-3898).
    pub fn namespace(&self) -> &RestateNamespace {
        self.restate.namespace()
    }

    /// `name`'s Restate name in this backend's namespace: what a crash rule,
    /// a hold or an invocation view names one of its services by.
    pub fn service_name(&self, name: &str) -> String {
        self.namespace().service_name(name)
    }

    /// The server double: time, crashes, operator commands, introspection.
    pub fn server(&self) -> &RestateTestServer {
        &self.server
    }

    /// The backend a runtime runs on: lash-restate's engine over the store
    /// set, connected to the server double. Hand it to a core wherever a
    /// test used a SQLite effect backend.
    pub fn lash_backend(&self) -> lash_core::Backend {
        lash_core::Backend::new(self.restate.clone())
    }

    /// The engine's session work with its own reconcile schedule off: every
    /// answer is `RestateSessionWork`'s, but installing a driver fills the
    /// slot without starting the wall-clock reconcile interval. A
    /// scenario that pins one interleaving per seed reconciles explicitly
    /// through [`SessionDriver::reconcile`] (or
    /// [`lash_core::drive::reconcile_once`]) when it wants a pass, so a
    /// seed's grant order never turns on when wall time happens to run
    /// the interval's store reads.
    pub fn explicit_reconcile_session_work(&self) -> Arc<dyn SessionWorkEngine> {
        Arc::new(ExplicitlyReconciledSessionWork {
            inner: Arc::clone(self.restate.session_work_engine()),
        })
    }

    /// lash-restate's own backend value, for APIs that name it.
    pub fn restate(&self) -> &Arc<RestateEngine> {
        &self.restate
    }

    pub(crate) fn loans(&self) -> &crate::open_handler::Loans {
        &self.loans
    }

    pub(crate) fn authority(&self) -> &RestateAuthorityId {
        &self.authority
    }

    /// The storage-only store set [`backend_with`]'s `decorate_stores` was
    /// applied to — the set as it was before decoration. A decorator's
    /// layers do not apply to the ports this set hands out; the decorated
    /// set the engine and its services run over is
    /// [`engine_stores`](Self::engine_stores).
    ///
    /// [`backend_with`]: crate::backend_with
    pub fn stores(&self) -> &Arc<lash_sqlite_store::SqliteStoreSet> {
        &self.stores
    }

    /// The store set the engine was built over: `decorate_stores`'s answer
    /// on a [`backend_with`] double, [`stores`](Self::stores) itself
    /// otherwise. A test reaching a port a decorator replaced — a faulted
    /// double's registry, say — reads it here.
    ///
    /// [`backend_with`]: crate::backend_with
    pub fn engine_stores(&self) -> &Arc<dyn StoreSet> {
        &self.engine_stores
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
                .service(self.service_name(SESSION_DRIVER_SERVICE))
                .within_attempts(1),
        );
    }

    /// Drop the next execution of an admitted root's handler (`LashTurn`) at
    /// `point`, on its first attempt, and let the server replay it: the crash
    /// cuts the root's journal.
    pub fn crash_turn_drive(&self, point: CrashPoint) {
        self.server.crash_on(
            CrashRule::new(point)
                .service(self.service_name(TURN_DRIVER_SERVICE))
                .within_attempts(1),
        );
    }

    /// Hold the engine's drive of `session`: its `LashSession` object runs
    /// no attempt until the returned [`Hold`](crate::Hold) is released or
    /// dropped. Inputs accepted meanwhile stay pending — no drive admits
    /// them — while a root already admitted runs on in its own `LashTurn`
    /// and settles. A drive running when the hold is taken stops at its next
    /// step (the call it awaits, the next admission); an admission already
    /// under way completes first. A test asserting what is still pending
    /// holds the engine rather than draining the queue itself: the caller
    /// never drives.
    pub async fn hold_session_drive(&self, session: &lash_core::SessionId) -> crate::Hold {
        self.server
            .hold(&self.service_name(SESSION_DRIVER_SERVICE), session.as_str())
            .await
    }

    /// Wait until the engine has no drive of `session` in flight: every
    /// `LashSession` invocation for it has completed, with the roots it
    /// awaited, and so has every scope close those roots owed. A send's
    /// handle answers at its root's final commit, before the root's scope
    /// closes (FIG-3979), and the root's scope closes on its `LashTurn`'s
    /// `close` handler beside the drive's next admission (FIG-4035), so a
    /// test that reads what the drive leaves behind, or sends the session's
    /// next input from another core, settles the drive first: while a drive
    /// runs it admits what the session is sent, on the driver it started on.
    pub async fn settle_session_drive(&self, session: &lash_core::SessionId) {
        let drives = format!(
            "{}/{}/",
            self.service_name(SESSION_DRIVER_SERVICE),
            session.as_str()
        );
        // A `LashTurn` key is `{len}:{session}{root}`
        // (`lash_restate::turn_workflow_key`).
        let roots = format!(
            "{}/{}:{}",
            self.service_name(TURN_DRIVER_SERVICE),
            session.as_str().len(),
            session.as_str()
        );
        while self.server.invocations().iter().any(|view| {
            (view.target.starts_with(&drives) || view.target.starts_with(&roots))
                && view.status != "completed"
        }) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    #[expect(
        clippy::result_large_err,
        reason = "the ingress client's RestateHttpError is unboxed across its public API"
    )]
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

    /// The slot the endpoint serves process segments from. Clones share it,
    /// so a crash listener can swap the worker the replaying attempt meets.
    pub fn process_worker_slot(&self) -> RestateProcessWorkerSlot {
        self.processes.clone()
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
        self.run_crashes_then_redriven(admitted, vec![crashing], redrive)
            .await
    }

    /// [`Self::run_crashed_then_redriven`] over several deployments that die
    /// in turn: each of `crashing` runs until it panics, the server replays
    /// the invocation into the next, and the last crash replays into
    /// `redrive`. Returns an error if any of them never panicked.
    pub async fn run_crashes_then_redriven(
        &self,
        admitted: AdmittedScope,
        crashing: Vec<HandlerAttempt>,
        redrive: HandlerAttempt,
    ) -> Result<(), String> {
        let key = self
            .run_parked_keyed(
                admitted,
                Parked::CrashThenRedrive {
                    crashing,
                    redrive,
                    crashed: 0,
                },
            )
            .await?;
        match self.jobs.take(&key) {
            Some((
                _,
                Parked::CrashThenRedrive {
                    crashing, crashed, ..
                },
            )) if crashed < crashing.len() => Err(format!(
                "job `{key}` completed after {crashed} of its {} crashing attempts crashed",
                crashing.len()
            )),
            _ => Ok(()),
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
        let handler_host = self.service_name(HANDLER_HOST);
        let call = ingress.call_workflow_json::<_, bool>(&handler_host, &key, "run", &key);
        // A job whose handler exhausted its retries is paused, not failed:
        // report it at once instead of waiting out the attach ceiling.
        let paused = async {
            loop {
                if let Some(view) =
                    self.server
                        .find_invocation(&handler_host, &key, "run", "paused")
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
// Session work without the deployment's reconcile schedule
// ---------------------------------------------------------------------------

/// [`RestateSessionWork`] minus the wall-clock reconcile interval a real
/// deployment starts when a driver is installed (ADR 0104 O2). Every other
/// answer is the engine's — sends, attaches, control and live-work reads
/// included — but an installed driver only fills the
/// [`lash_restate::RestateSessionDriverSlot`]: nothing ticks until the
/// scenario reconciles through [`SessionDriver::reconcile`] itself, so a
/// seeded run never meets a drive ask whose request id and landing point
/// wall time picked.
pub(crate) struct ExplicitlyReconciledSessionWork {
    pub(crate) inner: Arc<RestateSessionWork>,
}

impl std::fmt::Debug for ExplicitlyReconciledSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExplicitlyReconciledSessionWork")
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl SessionWorkEngine for ExplicitlyReconciledSessionWork {
    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.inner.control()
    }

    fn schedule_drive(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::DriveRequestId,
    ) {
        self.inner.schedule_drive(session, request);
    }

    async fn request_drive(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::DriveRequestId,
    ) -> Result<(), lash_core::engine::EngineRefusal> {
        self.inner.request_drive(session, request).await
    }

    /// The same get-or-init [`RestateSessionWork::install_session_driver`]
    /// answers, without its spawned interval: the driver a core installs
    /// serves every drive and answers [`SessionDriver::reconcile`] when a
    /// scenario calls it, but no pass runs on wall time.
    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        self.inner.driver_slot().install(driver)
    }

    async fn await_drive(
        &self,
        session: &lash_core::SessionId,
        request: &lash_core::engine::DriveRequestId,
    ) -> Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
        self.inner.await_drive(session, request).await
    }
}

// ---------------------------------------------------------------------------
// The handler host
// ---------------------------------------------------------------------------

pub(crate) const HANDLER_HOST: &str = "LashTestHandlerHost";

pub(crate) enum Parked {
    Replayed(HandlerAttempt),
    CrashThenRedrive {
        /// The attempts that crash, in the order the server runs them.
        crashing: Vec<HandlerAttempt>,
        redrive: HandlerAttempt,
        /// How many of `crashing` have crashed.
        crashed: usize,
    },
}

#[derive(Default)]
pub(crate) struct ParkedJobs {
    next: AtomicU64,
    /// Spliced into every job key: empty on the double, whose server is the
    /// backend's own, and a run nonce on a live server that outlives one
    /// backend, where a workflow key runs once.
    prefix: String,
    jobs: Mutex<HashMap<String, (AdmittedScope, Parked)>>,
}

impl ParkedJobs {
    /// Jobs whose keys carry `prefix`.
    pub(crate) fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            ..Self::default()
        }
    }

    /// Drop every parked job: the host that parked them died, so a handler
    /// the server invokes again for one finds nothing to run and fails it.
    pub(crate) fn clear(&self) {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn park(&self, admitted: AdmittedScope, parked: Parked) -> String {
        let ordinal = self.next.fetch_add(1, Ordering::SeqCst);
        let prefix = &self.prefix;
        // A session-scoped job carries the session in its key exactly as a
        // `LashTurn` key does: the engine's live-work read parses the owner
        // back out and leaves a suspended job's session ingress to it.
        let key = match admitted.scope().session_id() {
            Some(session) => turn_workflow_key(
                session,
                &lash_core::TurnId::from(format!("job-{prefix}{ordinal}")),
            ),
            None => format!("job-{prefix}{ordinal}"),
        };
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), (admitted, parked));
        key
    }

    pub(crate) fn take(&self, key: &str) -> Option<(AdmittedScope, Parked)> {
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
                let next = crashing.get(*crashed);
                let attempt = Arc::clone(next.unwrap_or(redrive));
                let job: HandlerJob = Box::new(move |scoped| attempt(scoped));
                Some((admitted.clone(), job, next.is_some()))
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
            *crashed += 1;
        }
    }
}

/// The workflow a job runs in. One key per job; the handler takes the parked
/// job and runs it on its own `ctx`-bound controller.
pub(crate) struct HandlerHost {
    pub(crate) jobs: Arc<ParkedJobs>,
    pub(crate) authority: RestateAuthorityId,
    /// The namespace of the deployment the host serves: the controller it
    /// runs a job on calls that namespace's lash services, and the host is
    /// bound under its name there.
    pub(crate) namespace: RestateNamespace,
}

/// [`HandlerHost`]'s service, declared through the trait API: its generated
/// dispatcher is a nameable type, so the binding can name the service in its
/// deployment's namespace.
mod handler_host {
    #![allow(
        deprecated,
        reason = "Restate SDK 0.11's trait service API is the one whose dispatcher a binding can rename"
    )]

    use super::*;

    #[restate_sdk::workflow]
    #[name = "LashTestHandlerHost"]
    pub(crate) trait HandlerHostService {
        async fn run(key: Json<String>) -> HandlerResult<Json<bool>>;
    }

    /// Bind `host` on `builder` under [`HANDLER_HOST`] in its namespace, or
    /// report the name Restate refuses.
    pub(crate) fn bind_handler_host(
        builder: restate_sdk::endpoint::Builder,
        host: HandlerHost,
    ) -> Result<restate_sdk::endpoint::Builder, String> {
        use restate_sdk::service::Discoverable as _;
        let name = host.namespace.service_name(HANDLER_HOST);
        let mut discovery = ServeHandlerHostService::<HandlerHost>::discover();
        discovery.name = restate_sdk::discovery::ServiceName::try_from(name.clone())
            .map_err(|error| format!("`{name}`: {error}"))?;
        Ok(
            builder.bind(restate_sdk::service::macro_support::service_definition(
                host.serve(),
                discovery,
            )),
        )
    }

    impl HandlerHostService for HandlerHost {
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
                lash_restate::RestateRuntimeEffectController::new(ctx, self.authority.clone())
                    .in_namespace(self.namespace.clone());
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
}

pub(crate) use handler_host::bind_handler_host;

/// A registration URI's last segment for the deployment labelled `label`.
fn deployment_name(label: &str) -> String {
    if label.is_empty() {
        "first".to_owned()
    } else {
        label.to_owned()
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
