//! One crash-matrix world: lash-restate's engine over a SQLite memory store
//! set, on the server double or a live `restate-server` ([`Engine`]), and the
//! deployment that runs on it.
//!
//! A deployment is a [`lash::LashCore`] built over the world's backend, the
//! session driver it installs, and its recovery interval. The interval does
//! not tick on wall time: [`CrashWorld::tick`] moves the engine's clock by
//! one jittered `T` and runs the deployment's recovery pass, so a detection
//! bound is measured in sim time. [`CrashWorld::kill`] ends the deployment
//! where it stands — its host tasks aborted, every attempt the engine was
//! running on it dropped and replayed — and [`CrashWorld::restart`] brings up
//! a fresh process (a new core, a new owner incarnation, a new interval with
//! a fresh cursor).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use lash_core::Backend;
use lash_core::sync::MutexExt as _;

use super::deployment::{
    CrashProcessPort, CrashSessionFactory, CrashSessionWork, DriveLog, DriverProxy, HostFaults,
    Trip,
};
use super::engine::{Engine, EngineHold, EngineInvocation, EngineKind};

/// Builds one deployment's core over the world's backend under a fresh
/// owner incarnation.
pub type CoreBuild = Arc<
    dyn Fn(Backend, lash_core::LeaseOwnerIdentity) -> Result<lash::LashCore, String> + Send + Sync,
>;

/// How long a recovery pass may run in wall time before the world calls it
/// hung.
const TICK_WALL_LIMIT: Duration = Duration::from_secs(30);

/// One recovery interval: its cursor, owned by the process that runs it. A
/// restart starts a fresh one.
struct Interval {
    cursor: lash_core::engine::ReconcileCursor,
    /// Engine time at the interval's last tick, or at its start.
    last_tick_ms: u64,
}

impl Interval {
    fn fresh(now_ms: u64) -> Self {
        Self {
            cursor: lash_core::engine::ReconcileCursor::default(),
            last_tick_ms: now_ms,
        }
    }
}

struct Deployment {
    core: lash::LashCore,
    tasks: Vec<tokio::task::AbortHandle>,
}

/// `engine`'s backend as a deployment reaches it: the crash levers on the
/// session work, the session catalog and the process-work port.
fn layered(engine: Backend, work: &Arc<CrashSessionWork>, faults: &Arc<HostFaults>) -> Backend {
    let factory_faults = Arc::clone(faults);
    let port_faults = Arc::clone(faults);
    lash_core::testing::runtime_helpers::LayeredBackend::over(engine)
        .with_session_work(Some(
            Arc::clone(work) as Arc<dyn lash_core::SessionWorkEngine>
        ))
        .map_session_store_factory(move |factory| {
            Arc::new(CrashSessionFactory::new(factory, factory_faults))
        })
        .map_process_work_port(move |port| Arc::new(CrashProcessPort::new(port, port_faults)))
        .into_backend()
}

/// Whichever deployment is up, as a host handler attempt reaches it.
#[derive(Clone)]
pub struct LiveCore(tokio::sync::watch::Receiver<Option<lash::LashCore>>);

impl std::fmt::Debug for LiveCore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveCore")
            .field("up", &self.0.borrow().is_some())
            .finish()
    }
}

impl LiveCore {
    /// The live deployment's core, waiting while none is up.
    pub async fn get(&self) -> Result<lash::LashCore, String> {
        let mut cores = self.0.clone();
        cores
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "the world ended".to_owned())
            .map(|core| {
                core.clone()
                    .unwrap_or_else(|| unreachable!("waited for a core"))
            })
    }
}

/// See the module documentation.
pub struct CrashWorld {
    seed: u64,
    rng: Mutex<fastrand::Rng>,
    engine: Engine,
    backend: Backend,
    /// The backend the next deployment is built over: the first build's, or
    /// the one [`add_generation`](Self::add_generation) registered last.
    deploy: Mutex<Backend>,
    work: Arc<CrashSessionWork>,
    proxy: Arc<DriverProxy>,
    faults: Arc<HostFaults>,
    trip: Arc<Trip>,
    killing: Arc<AtomicBool>,
    build: CoreBuild,
    serve_processes: bool,
    live: Mutex<Option<Deployment>>,
    /// The live deployment's core, for host handler attempts that replay
    /// onto whichever deployment is up ([`replay_host_handlers`]).
    ///
    /// [`replay_host_handlers`]: Self::replay_host_handlers
    cores: tokio::sync::watch::Sender<Option<lash::LashCore>>,
    /// Whether a kill crashes the host's own handler invocations, so the
    /// server replays them, instead of killing them.
    host_handlers_replay: AtomicBool,
    incarnations: AtomicU32,
    interval: tokio::sync::Mutex<Interval>,
    ticks_run: std::sync::atomic::AtomicUsize,
    /// How long [`quiesce`](Self::quiesce) waits in wall time for the server
    /// to settle, in milliseconds.
    quiesce_ms: std::sync::atomic::AtomicU64,
    /// The drive requests the session work saw.
    drives: Arc<DriveLog>,
}

impl std::fmt::Debug for CrashWorld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CrashWorld")
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

impl CrashWorld {
    /// A world under `seed` whose deployments `build` builds, on the engine
    /// the environment names ([`EngineKind::from_env`]). No deployment is up
    /// until [`restart`](Self::restart). `serve_processes` installs each
    /// deployment's durable process worker on the engine's endpoint.
    pub async fn new(seed: u64, build: CoreBuild, serve_processes: bool) -> Result<Self, String> {
        Self::on_engine(
            seed,
            Engine::start(&EngineKind::from_env()?, seed).await?,
            build,
            serve_processes,
        )
        .await
    }

    /// [`new`](Self::new) on a server double configured by `config`, whatever
    /// the environment names: its time mode and retry policy decide how a
    /// replay's retries spread over virtual time.
    pub async fn on_server(
        seed: u64,
        build: CoreBuild,
        serve_processes: bool,
        config: lash_restate_test::ServerConfig,
    ) -> Result<Self, String> {
        let engine = lash_restate_test::backend(seed, config)
            .await
            .map(Engine::Double)
            .map_err(|error| format!("build the Restate test backend: {error}"))?;
        Self::on_engine(seed, engine, build, serve_processes).await
    }

    async fn on_engine(
        seed: u64,
        engine: Engine,
        build: CoreBuild,
        serve_processes: bool,
    ) -> Result<Self, String> {
        let clock: Arc<dyn lash_core::Clock> = engine.clock();
        let trip = Arc::new(Trip::new(clock));
        let faults = Arc::new(HostFaults::new(Arc::clone(&trip)));
        let proxy = Arc::new(DriverProxy::default());
        let drives = Arc::new(DriveLog::default());
        let work = Arc::new(CrashSessionWork::new(
            engine.explicit_reconcile_session_work(),
            Arc::clone(&proxy),
            Arc::clone(&faults),
            Arc::clone(&drives),
        ));
        let backend = layered(engine.lash_backend(), &work, &faults);
        let killing = Arc::new(AtomicBool::new(false));
        {
            // An engine crash rule dropped an attempt: the deployment died
            // there. Take it down before the replay starts, so the replay
            // waits for the restarted one.
            let trip: Weak<Trip> = Arc::downgrade(&trip);
            let proxy: Weak<DriverProxy> = Arc::downgrade(&proxy);
            let killing = Arc::clone(&killing);
            engine.on_crash(Arc::new(move |target| {
                if killing.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(trip) = trip.upgrade() {
                    trip.fire(format!("engine:{target}"));
                }
                if let Some(proxy) = proxy.upgrade() {
                    proxy.down();
                }
            }));
        }
        let engine_now_ms = engine.now_ms();
        Ok(Self {
            seed,
            rng: Mutex::new(fastrand::Rng::with_seed(seed)),
            engine,
            deploy: Mutex::new(backend.clone()),
            backend,
            work,
            proxy,
            faults,
            trip,
            killing,
            build,
            serve_processes,
            live: Mutex::new(None),
            cores: tokio::sync::watch::channel(None).0,
            host_handlers_replay: AtomicBool::new(false),
            incarnations: AtomicU32::new(0),
            interval: tokio::sync::Mutex::new(Interval::fresh(engine_now_ms)),
            ticks_run: std::sync::atomic::AtomicUsize::new(0),
            quiesce_ms: std::sync::atomic::AtomicU64::new(2_000),
            drives,
        })
    }

    /// The drive requests the deployments' session work saw: what waiters
    /// awaited and what the engine accepted as asks.
    #[must_use]
    pub fn drives(&self) -> &DriveLog {
        &self.drives
    }

    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// A seeded draw in `range`.
    pub fn draw(&self, range: std::ops::Range<u64>) -> u64 {
        self.rng.lock_recover().u64(range)
    }

    /// The engine this world runs on.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The server double this world runs on; a world on a live server has
    /// none.
    pub fn double(&self) -> Result<&lash_restate_test::RestateTestBackend, String> {
        match &self.engine {
            Engine::Double(double) => Ok(double),
            Engine::Live(_) => {
                Err("this world runs on a live restate-server, not the double".to_owned())
            }
        }
    }

    /// Arm a journal-step crash on the engine.
    pub fn crash_on(&self, rule: lash_restate_test::CrashRule) {
        self.engine.crash_on(rule);
    }

    /// Every invocation the engine holds.
    pub async fn invocations(&self) -> Vec<EngineInvocation> {
        self.engine.invocations().await
    }

    /// Kill `id` as an operator does, and wait until it can run nothing more.
    pub async fn kill_invocation(&self, id: &str) -> Result<(), String> {
        self.engine.kill_and_await(id).await
    }

    /// Hold the engine's drive of `session` ([`Engine::hold_session_drive`]).
    pub async fn hold_session_drive(&self, session: &lash_core::SessionId) -> EngineHold {
        self.engine.hold_session_drive(session).await
    }

    /// Hold every invocation of `service` ([`Engine::hold_service`]).
    pub async fn hold_service(&self, service: &str) -> EngineHold {
        self.engine.hold_service(service).await
    }

    /// The first build's backend: the stores every check reads.
    #[must_use]
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The build generation the next deployment runs.
    #[must_use]
    pub fn generation(&self) -> lash_core::engine::BuildGeneration {
        self.deploy.lock_recover().build_generation().clone()
    }

    /// Register a build of drain generation `generation` on the server under
    /// `label`, beside the builds already serving (a rolling deploy's new
    /// build), and build every later deployment over it: its core stamps
    /// `generation` and competes for the recovery lease as that build. Only a
    /// [`restart`](Self::restart) brings a deployment of it up.
    pub async fn add_generation(
        &self,
        generation: lash_core::engine::BuildGeneration,
        label: impl Into<String>,
    ) -> Result<lash_restate_test::DeploymentId, String> {
        let double = self.double()?;
        let deployment = double
            .add_build(
                generation.clone(),
                label,
                lash_restate_test::DeploymentHooks::default(),
            )
            .await
            .map_err(|error| format!("register the build of `{generation}`: {error}"))?;
        let engine = Backend::new(Arc::new(double.restate().sibling_build(generation)));
        *self.deploy.lock_recover() = layered(engine, &self.work, &self.faults);
        Ok(deployment)
    }

    #[must_use]
    pub fn faults(&self) -> &Arc<HostFaults> {
        &self.faults
    }

    #[must_use]
    pub fn trip(&self) -> &Arc<Trip> {
        &self.trip
    }

    /// Virtual now, the store clock's epoch milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.engine.now_ms()
    }

    /// The live deployment's core.
    pub fn core(&self) -> Result<lash::LashCore, String> {
        self.live
            .lock_recover()
            .as_ref()
            .map(|deployment| deployment.core.clone())
            .ok_or_else(|| "no deployment is up".to_owned())
    }

    #[must_use]
    pub fn ticks_run(&self) -> usize {
        self.ticks_run.load(Ordering::SeqCst)
    }

    /// Bring up a fresh deployment: a new process with a new owner
    /// incarnation, its core, its session driver and a fresh interval.
    pub async fn restart(&self) -> Result<(), String> {
        let incarnation = self.incarnations.fetch_add(1, Ordering::SeqCst) + 1;
        let owner = lash_core::LeaseOwnerIdentity::opaque(
            "lash-sim-crash-matrix",
            format!("seed-{:x}-incarnation-{incarnation}", self.seed),
        );
        let backend = self.deploy.lock_recover().clone();
        let core = (self.build)(backend, owner)?;
        if self.serve_processes {
            let config = core
                .durable_process_worker_config()
                .map_err(|error| format!("the core's process worker config: {error}"))?;
            self.engine.install_process_worker(
                lash::durability::DurableProcessWorker::new(config)
                    .map_err(|error| format!("build the process worker: {error}"))?,
            );
        }
        *self.interval.lock().await = Interval::fresh(self.engine.now_ms());
        self.cores.send_replace(Some(core.clone()));
        *self.live.lock_recover() = Some(Deployment {
            core,
            tasks: Vec::new(),
        });
        self.engine.revive_deployment().await
    }

    /// From now on a kill crashes the host's own handler invocations as it
    /// crashes lash's, and the server replays them, as Restate retries a
    /// host service's handler on the next deployment. Their attempts must
    /// take the core from [`live_core`](Self::live_core) at each attempt,
    /// never capture one.
    pub fn replay_host_handlers(&self) {
        self.host_handlers_replay.store(true, Ordering::SeqCst);
    }

    /// A handle on whichever deployment is up, for a handler attempt.
    #[must_use]
    pub fn live_core(&self) -> LiveCore {
        LiveCore(self.cores.subscribe())
    }

    /// Run `task` as the live deployment's host work: it dies with the
    /// deployment.
    pub fn spawn_host<F, T>(&self, task: F) -> tokio::task::JoinHandle<T>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = tokio::spawn(task);
        if let Some(deployment) = self.live.lock_recover().as_mut() {
            deployment.tasks.push(handle.abort_handle());
        }
        handle
    }

    /// Run `task` as host work and wait for it, unless a crash fires first:
    /// then the host died inside it and `None` answers.
    pub async fn host_op<F, T>(&self, task: F) -> Option<T>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let seen = self.trip.fires();
        let handle = self.spawn_host(task);
        tokio::select! {
            joined = handle => joined.ok(),
            () = self.trip.fired_beyond(seen) => None,
        }
    }

    /// Kill the live deployment where it stands: its host tasks abort, its
    /// driver stops serving, and every attempt the engine was running on it
    /// dies — a lash invocation is replayed, a host's own handler job is not.
    pub async fn kill(&self) {
        self.killing.store(true, Ordering::SeqCst);
        self.proxy.down();
        self.cores.send_replace(None);
        let deployment = self.live.lock_recover().take();
        if let Some(deployment) = deployment {
            for task in &deployment.tasks {
                task.abort();
            }
            drop(deployment);
        }
        match (
            &self.engine,
            self.host_handlers_replay.load(Ordering::SeqCst),
        ) {
            // The host's own handler jobs are crashed as lash's are, and
            // the server replays them on the next deployment.
            (Engine::Double(double), true) => {
                let server = double.server();
                for view in server.invocations() {
                    if view.status == "running" {
                        let _ = server.crash(&view.id);
                    }
                }
            }
            _ => self.engine.kill_deployment().await,
        }
        self.killing.store(false, Ordering::SeqCst);
    }

    /// Kill the deployment, let a seeded outage of up to five seconds pass on
    /// the virtual clock, and bring up a fresh one.
    pub async fn crash_and_restart(&self) -> Result<(), String> {
        self.kill().await;
        let outage = Duration::from_millis(self.draw(0..5_001));
        self.engine.advance(outage);
        self.restart().await
    }

    /// Wait until the engine and the host have nothing left to do right now:
    /// every live attempt blocked on the server, repeated so work the
    /// settled attempts handed to host tasks lands too. Bounded in wall time.
    pub async fn quiesce(&self) {
        let budget = Duration::from_millis(self.quiesce_ms.load(Ordering::SeqCst));
        for _ in 0..2 {
            self.engine.settle(budget).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Bound [`quiesce`](Self::quiesce)'s wall-time wait. A world that holds
    /// an attempt open on purpose never settles, so a long recovery (hundreds
    /// of ticks) waits less per tick.
    pub fn set_quiesce_budget(&self, budget: Duration) {
        self.quiesce_ms
            .store(budget.as_millis() as u64, Ordering::SeqCst);
    }

    /// One tick of the live deployment's recovery interval: the engine's
    /// clock moves one `T` with ±10 % seeded jitter
    /// ([`Engine::advance_tick`]), then the deployment runs its recovery
    /// pass. A pass that dies with the deployment is not an
    /// error of the tick. Answers the tick's engine time.
    pub async fn tick(&self) -> Result<u64, String> {
        let tick_ms = super::TICK.as_millis() as u64;
        let jittered = tick_ms - tick_ms / 10 + self.draw(0..tick_ms / 5 + 1);
        let ticked_at = {
            let mut interval = self.interval.lock().await;
            interval.last_tick_ms = self
                .engine
                .advance_tick(interval.last_tick_ms, Duration::from_millis(jittered));
            interval.last_tick_ms
        };
        self.ticks_run.fetch_add(1, Ordering::SeqCst);
        let driver = self
            .proxy
            .current()
            .ok_or_else(|| "no deployment is up to tick".to_owned())?;
        let cursor = self.interval.lock().await.cursor.clone();
        let pass = self.spawn_host(async move {
            driver
                .reconcile(&cursor, std::num::NonZeroUsize::MIN.saturating_add(63))
                .await
        });
        match tokio::time::timeout(TICK_WALL_LIMIT, pass).await {
            Ok(Ok(Ok(next))) => {
                self.interval.lock().await.cursor = next;
                Ok(ticked_at)
            }
            // A failed pass is retried by the next tick, as the interval's is.
            Ok(Ok(Err(_))) => Ok(ticked_at),
            // Aborted: the deployment died during its pass.
            Ok(Err(_)) => Ok(ticked_at),
            Err(_) => {
                if self.trip.tripped().is_some() {
                    Ok(ticked_at)
                } else {
                    Err(format!(
                        "a recovery pass ran past {TICK_WALL_LIMIT:?} of wall time"
                    ))
                }
            }
        }
    }

    /// End the world: take the deployment down so nothing waits on it.
    pub async fn finish(&self) {
        self.kill().await;
        self.faults.clear();
        self.engine.finish().await;
    }
}
