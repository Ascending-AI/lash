//! One deployment of the production durable runtime under the matrix: the
//! session and process activations behind one [`ActorDispatch`], on
//! [`SimNodes`] `a` and `b` over one database (SQLite memory by default),
//! with the host's writes through its own producer store.
//!
//! One node serves and the other stands by: a supervisor starts the standby
//! the moment the primary stops serving or pauses, and the standby then
//! reaps the primary and takes its actors over. With one node claiming at a
//! time, which node runs an actor is a function of the run, so every cut
//! point the uncut run recorded recurs when the matrix re-runs it.
//!
//! A [`Deployment`] is one cell's run of one [`Case`] at one seed. It is a
//! [`Scenario`]: the matrix builds a fresh one for every cell, runs it to
//! quiescence and asks it for its laws, which are the common
//! [`invariants`](super::invariants) and the case's own.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::runtime::durable::session::SessionActivation;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::{Backend, BackendParts, DurableSettings, NoProjectionProviders};
use lash_durable::runner::Activation;
use lash_durable::{ActorDispatch, ActorKey, DurableStore, LeaseConfig};
use lash_durable_test::{Cut, Matrix, OffClockWork, Scenario, SimClock, SimNodes, SimNodesConfig};

use super::Case;
use super::engine::{SimProcessEngine, SimSteps};
use super::invariants;
use super::services::SimServices;
use super::world::World;

/// The deployment's nodes, in the order an even seed starts them.
pub const NODES: [&str; 2] = ["a", "b"];
/// How many `Until` children a cascade marks per batch: small, so a
/// cascade over a few children takes several batches.
pub const CASCADE_BATCH: usize = 2;
/// How long a session with nothing to do stays hot before it releases.
pub const IDLE_EVICT: Duration = Duration::from_secs(1);

/// One case's work in a deployment: what it seeds, when it is done and the
/// laws of its own seam.
#[async_trait::async_trait]
pub trait Workload: Send + Sync {
    /// Seed the case's rows and start its host's tasks.
    async fn seed(&self, world: &Arc<World>, nodes: &Arc<SimNodes>) -> Result<(), String>;

    /// Whether the case reached its end.
    async fn done(&self, world: &World, nodes: &SimNodes) -> bool;

    /// The case's own laws after the run, one line per violation.
    async fn laws(&self, world: &World, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String>;

    /// The virtual time the uncut case takes at most; a cell's bound adds a
    /// failover and the stop grace for its one cut.
    fn bound(&self) -> Duration {
        Duration::from_secs(30)
    }

    /// How often one turn of the case restores from its checkpoint uncut.
    fn max_restores(&self) -> usize {
        1
    }
}

/// The substrate parameters the deployment runs under.
#[must_use]
pub fn settings() -> DurableSettings {
    DurableSettings {
        cascade_batch: CASCADE_BATCH,
        idle_evict: IDLE_EVICT,
        ..DurableSettings::default()
    }
}

/// How the deployment's nodes run: each decodes every format set
/// `backend`'s build writes.
#[must_use]
pub fn nodes_config(backend: &Backend) -> SimNodesConfig {
    SimNodesConfig {
        lease: LeaseConfig::default(),
        decodes: backend.formats().decodes(),
        max_active: 8,
    }
}

/// How long a failover takes: past a dead owner's lease, a reap and a
/// claim.
#[must_use]
pub fn failover(lease: LeaseConfig) -> Duration {
    let lease = lease.settings();
    lease.ttl + lease.reap_every + lease.claim_poll
}

/// The database a deployment runs over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// SQLite in memory: the cheapest tier.
    SqliteMemory,
    /// A fresh SQLite database file in a temporary directory: the tier a
    /// single-host deployment runs on.
    SqliteFile,
    /// A fresh isolated database on the PostgreSQL server at this URL.
    Postgres(String),
}

impl Dialect {
    /// PostgreSQL when `LASH_POSTGRES_DATABASE_URL` names a server, else
    /// `None`: a PostgreSQL leg without one is skipped.
    #[must_use]
    pub fn postgres_from_env() -> Option<Self> {
        std::env::var("LASH_POSTGRES_DATABASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .map(Self::Postgres)
    }
}

/// What must outlive a deployment's database: an isolated PostgreSQL
/// database or a SQLite file's directory, dropped with the run.
pub type Keep = Vec<Box<dyn std::any::Any + Send + Sync>>;

/// The backend of a deployment over a fresh database of `dialect` on
/// `clock`, and the durable store the nodes run on. What the database needs
/// to live is pushed onto `keep`.
///
/// # Errors
///
/// The database does not open or the backend does not assemble.
pub async fn open(
    dialect: &Dialect,
    clock: Arc<SimClock>,
    keep: &mut Keep,
) -> Result<(Backend, Arc<dyn DurableStore>), String> {
    match dialect {
        Dialect::SqliteMemory => sqlite(clock).await,
        Dialect::SqliteFile => sqlite_file(clock, keep).await,
        Dialect::Postgres(url) => postgres(url, clock, keep).await,
    }
}

/// A fresh isolated PostgreSQL database on the server at `url`, its durable
/// store on `clock`.
async fn postgres(
    url: &str,
    clock: Arc<SimClock>,
    keep: &mut Keep,
) -> Result<(Backend, Arc<dyn DurableStore>), String> {
    wait_out_renders(&clock);
    let base = url.to_owned();
    // Made on a thread and runtime of its own: its future is not `Send`
    // for every lifetime, and the deployment's runtime alone observes its
    // quiescence.
    let isolated = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|runtime| {
                runtime.block_on(lash_postgres_store::testing::IsolatedDatabase::create(
                    &base,
                ))
            })
            .map_err(|error| error.to_string())
    })
    .join()
    .map_err(|_| "the isolated database's setup panicked".to_owned())??;
    let pool = WaitedOutPool::open(isolated.url())?;
    clock.wait_out(Arc::clone(&pool) as _);
    let storage = lash_postgres_store::testing::from_pool(
        pool.pool.clone(),
        &lash_postgres_store::testing::work_pool_of(POOL_CONNECTIONS),
    )
    .await
    .map_err(|error| error.to_string())?;
    // Every port reads the virtual clock, the durable store's included: a
    // host's input row and its actor's wake are due when the nodes' clock
    // says.
    let stores = lash_postgres_store::PostgresStoreSet::with_clock_for_testing(
        &storage,
        Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        clock,
    );
    lash_core_execution::testing::process_execution_env_fixture(
        lash_core_execution::StoreSet::process_env_store(&stores).as_ref(),
    )
    .await;
    let database = lash_core_execution::StoreSet::durable_store(&stores);
    keep.push(Box::new(isolated));
    Ok((assemble(Arc::new(stores))?, database))
}

/// The URL of the isolated PostgreSQL database `keep` holds, for a fault
/// that acts on the database itself.
#[must_use]
pub fn isolated_url(keep: &Keep) -> Option<String> {
    keep.iter().find_map(|kept| {
        kept.downcast_ref::<lash_postgres_store::testing::IsolatedDatabase>()
            .map(|isolated| isolated.url().to_owned())
    })
}

/// How many connections the PostgreSQL deployment's one pool holds: every
/// role shares it.
const POOL_CONNECTIONS: u32 = 16;

/// The PostgreSQL deployment's pool, which every role of its storage
/// shares. Its queries answer over the network, which the deployment's
/// runtime does not see, so the clock waits it out: it is busy while a
/// connection is in use, and each connection's release ends one piece of
/// its work.
struct WaitedOutPool {
    pool: sqlx::PgPool,
    released: Arc<AtomicUsize>,
}

impl WaitedOutPool {
    fn open(url: &str) -> Result<Arc<Self>, String> {
        let released = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&released);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(POOL_CONNECTIONS)
            .min_connections(0)
            .after_release(move |_, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            })
            .connect_lazy(url)
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self { pool, released }))
    }
}

impl OffClockWork for WaitedOutPool {
    fn busy(&self) -> bool {
        self.pool.size() > self.pool.num_idle() as u32
    }

    fn ended(&self) -> usize {
        self.released.load(Ordering::SeqCst)
    }
}

/// The process's prompt renders: they run on the composer's threads, off
/// the deployment's runtime.
struct PromptRenders;

impl OffClockWork for PromptRenders {
    fn busy(&self) -> bool {
        lash_core_execution::plugin::prompt::PromptRenderPool::renders_in_flight().0
    }

    fn ended(&self) -> usize {
        lash_core_execution::plugin::prompt::PromptRenderPool::renders_in_flight().1
    }
}

/// Hold `clock` while a prompt render of the process is in flight.
fn wait_out_renders(clock: &SimClock) {
    clock.wait_out(Arc::new(PromptRenders));
}

/// The backend of a deployment over a fresh SQLite memory store set on
/// `clock`, and the store set's durable store. Each store call runs to its
/// answer before its caller goes on, so the deployment's one runtime thread
/// sees its calls finish in the order it issued them, and its clock never
/// moves while one is pending.
///
/// # Errors
///
/// The store set does not open or the backend does not assemble.
pub async fn sqlite(clock: Arc<SimClock>) -> Result<(Backend, Arc<dyn DurableStore>), String> {
    wait_out_renders(&clock);
    let options = lash_sqlite_store::SqliteStoreSetOptions {
        inline_calls: true,
        ..lash_sqlite_store::SqliteStoreSetOptions::memory()
    };
    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(options, clock)
        .await
        .map_err(|error| error.to_string())?;
    lash_core_execution::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let backend = assemble(Arc::new(stores))?;
    Ok((backend, database))
}

/// The backend of a deployment over a fresh SQLite database file on
/// `clock`, and its durable store. Its calls run inline as
/// [`sqlite`]'s do; the file's directory is pushed onto `keep`.
///
/// # Errors
///
/// The directory or the store set does not open, or the backend does not
/// assemble.
pub async fn sqlite_file(
    clock: Arc<SimClock>,
    keep: &mut Keep,
) -> Result<(Backend, Arc<dyn DurableStore>), String> {
    wait_out_renders(&clock);
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let options = lash_sqlite_store::SqliteStoreSetOptions {
        inline_calls: true,
        ..lash_sqlite_store::SqliteStoreSetOptions::standard(
            lash_sqlite_store::SqliteSynchronous::Full,
        )
    };
    let stores = lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
        directory.path().join("lash.db"),
        options,
        clock,
    )
    .await
    .map_err(|error| error.to_string())?;
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let backend = assemble(Arc::new(stores))?;
    keep.push(Box::new(directory));
    Ok((backend, database))
}

/// The deployment's backend over `stores`: its settings and the simulator's
/// process engine.
///
/// # Errors
///
/// The backend does not assemble.
pub fn assemble(stores: Arc<dyn lash_core_execution::StoreSet>) -> Result<Backend, String> {
    Backend::assemble(BackendParts {
        stores,
        settings: settings(),
        engines: vec![Arc::new(SimProcessEngine)],
        providers: Arc::new(NoProjectionProviders),
        // Its cells run on the RLM worker path: actors hold the VM's state.
        formats: lash::formats::actor_state_surfaces(),
    })
    .map_err(|error| error.to_string())
}

/// What every node runs for each actor it claims: the production session
/// and process activations over `world`'s backend.
///
/// # Errors
///
/// The world has no backend yet.
pub fn activation(world: &Arc<World>, slow: Duration) -> Result<Arc<dyn Activation>, String> {
    let backend = world.backend()?;
    Ok(Arc::new(ActorDispatch {
        session: Arc::new(SessionActivation::new(
            backend.clone(),
            Arc::new(SimServices::new(Arc::clone(world), slow)),
            Arc::clone(world.tripwire()) as _,
        )),
        process: Arc::new(ProcessActivation::new(
            backend,
            Arc::new(SimSteps::new(Arc::clone(world))),
            Arc::clone(world.tripwire()) as _,
        )),
    }))
}

/// The latency of [`Tool::WriteSlow`](super::services::Tool::WriteSlow) at
/// `seed`.
#[must_use]
pub fn slow(seed: u64) -> Duration {
    Duration::from_millis(50 + 30 * (seed % 2))
}

/// How often the supervisor looks at the primary.
const SUPERVISE_EVERY: Duration = Duration::from_millis(250);

/// The supervisor: once `primary` stops serving or pauses, start `standby`.
async fn supervise(
    world: Arc<World>,
    nodes: std::sync::Weak<SimNodes>,
    primary: &'static str,
    standby: &'static str,
) {
    loop {
        world.sleep(SUPERVISE_EVERY).await;
        let Some(nodes) = nodes.upgrade() else {
            return;
        };
        if !nodes.serving(primary) || nodes.life(primary) == lash_durable_test::Life::Paused {
            nodes.start(standby);
            world.note(format!("standby {standby} started"));
            return;
        }
    }
}

/// The primary and the standby at `seed`.
#[must_use]
pub fn start_order(seed: u64) -> [&'static str; 2] {
    if seed.is_multiple_of(2) {
        NODES
    } else {
        [NODES[1], NODES[0]]
    }
}

/// One cell's run of `case` at `seed`.
pub struct Deployment {
    case: Case,
    seed: u64,
    dialect: Dialect,
    world: Arc<World>,
    workload: Box<dyn Workload>,
    keep: std::sync::Mutex<Keep>,
}

impl Deployment {
    /// A fresh run of `case` at `seed` over `dialect`.
    #[must_use]
    pub fn new(case: Case, seed: u64, dialect: Dialect) -> Self {
        Self {
            case,
            seed,
            dialect,
            world: Arc::default(),
            workload: case.workload(),
            keep: std::sync::Mutex::default(),
        }
    }

    /// The run's world.
    #[must_use]
    pub fn world(&self) -> &Arc<World> {
        &self.world
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        self.world.stop_tasks();
    }
}

#[async_trait::async_trait]
impl Scenario for Deployment {
    #[expect(
        clippy::expect_used,
        reason = "a cell cannot run without its database; the matrix reports the panic"
    )]
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let mut keep = Keep::new();
        let (backend, database) = open(&self.dialect, Arc::clone(&clock), &mut keep)
            .await
            .expect("the cell's database opens");
        lash_core::sync::MutexExt::lock_recover(&self.keep).extend(keep);
        self.world.set_parts(backend, clock);
        database
    }

    #[expect(
        clippy::expect_used,
        reason = "the matrix builds the database before the config"
    )]
    fn config(&self) -> SimNodesConfig {
        // The shared factory also serves the soak driver, which keeps the
        // production lease. Crash cells use the named short test lease.
        let mut config = nodes_config(&self.world.backend().expect("the database is built first"));
        config.lease = Matrix::test_lease();
        config
    }

    #[expect(
        clippy::expect_used,
        reason = "the matrix builds the database before the activation"
    )]
    fn activation(&self) -> Arc<dyn Activation> {
        activation(&self.world, slow(self.seed)).expect("the database is built first")
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        self.world.start_host(nodes)?;
        self.workload.seed(&self.world, nodes).await?;
        let [primary, standby] = start_order(self.seed);
        nodes.start(primary);
        nodes.quiesce().await;
        self.world.spawn(supervise(
            Arc::clone(&self.world),
            Arc::downgrade(nodes),
            primary,
            standby,
        ));
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.world.actors()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        self.workload.done(&self.world, nodes).await
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let bound = self.workload.bound()
            + failover(nodes.lease())
            + lash_sansio::ExecutionBudgets::recommended().stop_grace();
        let bound_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        let mut violations = invariants::check(
            &self.world,
            nodes,
            cut,
            bound_ms,
            self.workload.max_restores(),
        )
        .await;
        violations.extend(self.workload.laws(&self.world, nodes, cut).await);
        self.world.stop_tasks();
        violations
            .into_iter()
            .map(|violation| format!("{} seed {}: {violation}", self.case.name(), self.seed))
            .collect()
    }
}
