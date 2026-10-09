//! The host's one PostgreSQL contract (FIG-5240): [`PostgresHostConfig`],
//! the endpoints it connects through, the role-aware connection factory
//! every lash PostgreSQL connection is made by, and the role pools a
//! [`PostgresStorage`](crate::PostgresStorage) routes its work to.
//!
//! Every connection lash opens is one of the [`ConnectionRole`]s. The
//! factory names it `<prefix>/<role>` in `application_name`, applies the
//! shared transport policy and statement cache, sizes its pool exactly as
//! its [`PoolPolicy`] says, and installs its guard profile: as session
//! defaults on a direct connection, and on every transaction through a
//! [`TransactionPrelude`] sent with `BEGIN` itself, so the limits hold
//! before the transaction's first lock or data statement (the writer fence
//! included), whoever built the pool.

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::Semaphore;

pub(crate) mod config;
pub(crate) mod serde_ms;
pub(crate) mod validate;

pub use config::{
    ConnectionPolicy, ConnectionTopology, DedicatedConnectionPolicy, DeploymentBudget,
    GuardPolicies, LiveReplayPolicy, MaintenancePolicy, PoolPolicy, PostgresHostConfig,
    ProcessReplayDataPolicy, ProcessReplayPolicy, ReconnectPolicy, ReplayDataPolicy,
    ReplaySchemaMode, RetryPolicies, RetryPolicy, RolePolicies, ServerTimeout, SignalPolicy,
    SslMode, TransactionGuards, TransportOverrides,
};
pub use validate::PostgresHostConfigError;

use crate::{
    PostgresConnectionBudgetRefusal, PostgresConnectionCapacity, StoreError, store_sqlx_error,
};

/// Where lash connects: the primary endpoint every pooled role uses, and
/// optionally a session endpoint for the roles that need a server session of
/// their own. Credentials live here and nowhere in the configuration; the
/// `Debug` form names only host, port and database.
#[derive(Clone)]
pub struct PostgresEndpoints {
    primary: PgConnectOptions,
    session: Option<PgConnectOptions>,
}

impl PostgresEndpoints {
    /// Every role through `primary`.
    pub fn new(primary: PgConnectOptions) -> Self {
        Self {
            primary,
            session: None,
        }
    }

    /// Every role through the connection string `url`.
    ///
    /// # Errors
    ///
    /// [`PostgresHostError::Endpoint`] when `url` does not parse; the error
    /// does not repeat it.
    pub fn from_url(url: &str) -> Result<Self, PostgresHostError> {
        url.parse::<PgConnectOptions>()
            .map(Self::new)
            .map_err(|_| PostgresHostError::Endpoint { which: "primary" })
    }

    /// Connect the session roles through `session`: required when the
    /// primary endpoint is a transaction-mode pooler.
    #[must_use]
    pub fn with_session(mut self, session: PgConnectOptions) -> Self {
        self.session = Some(session);
        self
    }

    /// [`with_session`](Self::with_session) from a connection string.
    ///
    /// # Errors
    ///
    /// [`PostgresHostError::Endpoint`] when `url` does not parse.
    pub fn with_session_url(self, url: &str) -> Result<Self, PostgresHostError> {
        let session = url
            .parse::<PgConnectOptions>()
            .map_err(|_| PostgresHostError::Endpoint { which: "session" })?;
        Ok(self.with_session(session))
    }

    /// The primary endpoint.
    pub fn primary(&self) -> &PgConnectOptions {
        &self.primary
    }

    /// The session endpoint, when one is given.
    pub fn session(&self) -> Option<&PgConnectOptions> {
        self.session.as_ref()
    }
}

fn redacted(options: &PgConnectOptions) -> String {
    format!(
        "{}:{}/{}",
        options.get_host(),
        options.get_port(),
        options.get_database().unwrap_or("")
    )
}

impl std::fmt::Debug for PostgresEndpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresEndpoints")
            .field("primary", &redacted(&self.primary))
            .field("session", &self.session.as_ref().map(redacted))
            .finish()
    }
}

/// Why a host's PostgreSQL connect was refused or failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum PostgresHostError {
    /// The configuration breaks a rule; nothing connected.
    Config(PostgresHostConfigError),
    /// An endpoint connection string does not parse.
    Endpoint {
        /// `primary` or `session`.
        which: &'static str,
    },
    /// The declared deployment does not fit the server's capacity; only
    /// the capacity probe connected.
    Budget(PostgresConnectionBudgetRefusal),
    /// The session endpoint reaches another catalog than the primary.
    EndpointCatalogMismatch {
        /// The primary endpoint's catalog identity.
        primary: String,
        /// The session endpoint's.
        session: String,
    },
    /// The open did not finish within `guards.store_startup_ms`.
    StartupTimedOut {
        /// The bound it exceeded.
        after: Duration,
    },
    /// The store refused or failed the open.
    Store(StoreError),
}

impl std::fmt::Display for PostgresHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Endpoint { which } => write!(f, "the {which} PostgreSQL endpoint does not parse"),
            Self::Budget(refusal) => write!(f, "postgres connection budget refused: {refusal}"),
            Self::EndpointCatalogMismatch { primary, session } => write!(
                f,
                "the session endpoint reaches catalog {session}, the primary {primary}"
            ),
            Self::StartupTimedOut { after } => {
                write!(f, "the postgres store open did not finish within {after:?}")
            }
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PostgresHostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Budget(refusal) => Some(refusal),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PostgresHostConfigError> for PostgresHostError {
    fn from(error: PostgresHostConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<StoreError> for PostgresHostError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// One kind of lash PostgreSQL connection: what its `application_name`
/// names and its pool metrics report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ConnectionRole {
    /// Store calls and ordinary durable commits.
    Work,
    /// Claims, adoption, owned scans, node drain, liveness probes.
    Scheduler,
    /// Reap, node release, hand-back, cancels and terminals.
    Critical,
    /// Node registration and heartbeat.
    Renewal,
    /// A node's notification listener and liveness lock.
    Listener,
    /// Detached schema verification and attachment sweep sessions.
    Session,
    /// The live replay store's data pool.
    Replay,
    /// The live replay store's listener.
    ReplayListener,
    /// The process replay store's data pool.
    ProcessReplay,
    /// The process replay store's listener.
    ProcessReplayListener,
    /// A preflight probe.
    Preflight,
    /// `lash migrate`.
    Migration,
}

impl ConnectionRole {
    /// The role's name in `application_name` and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Scheduler => "scheduler",
            Self::Critical => "critical",
            Self::Renewal => "renewal",
            Self::Listener => "listener",
            Self::Session => "session",
            Self::Replay => "replay",
            Self::ReplayListener => "replay-listener",
            Self::ProcessReplay => "process-replay",
            Self::ProcessReplayListener => "process-replay-listener",
            Self::Preflight => "preflight",
            Self::Migration => "migration",
        }
    }

    /// Whether the role needs a server session that outlives a transaction.
    fn needs_session(self) -> bool {
        matches!(
            self,
            Self::Listener
                | Self::Session
                | Self::ReplayListener
                | Self::ProcessReplayListener
                | Self::Preflight
                | Self::Migration
        )
    }
}

impl std::fmt::Display for ConnectionRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `SET` value of a timeout in milliseconds, or `None` to inherit.
fn timeout_setting(timeout: ServerTimeout) -> Option<String> {
    match timeout {
        ServerTimeout::Inherit => None,
        ServerTimeout::Disabled => Some("0".to_owned()),
        // `validate` refused limits that are not whole milliseconds or that
        // overflow PostgreSQL's integer setting.
        ServerTimeout::Limit(limit) => Some(limit.as_millis().to_string()),
    }
}

fn guard_settings(guards: &TransactionGuards) -> Vec<(&'static str, String)> {
    [
        ("lock_timeout", guards.lock),
        ("statement_timeout", guards.statement),
        (
            "idle_in_transaction_session_timeout",
            guards.idle_in_transaction,
        ),
        ("transaction_timeout", guards.transaction),
    ]
    .into_iter()
    .filter_map(|(name, timeout)| timeout_setting(timeout).map(|value| (name, value)))
    .collect()
}

/// A transaction's guard profile, sent with its `BEGIN` in one simple query:
/// the limits are in force before the transaction's first lock or data
/// statement, on any connection, whether lash or the host built its pool.
#[derive(Clone, Debug)]
pub struct TransactionPrelude {
    begin: Arc<str>,
    /// The guards, the writer fence's shared lock and then `BEGIN`, in one
    /// simple query: what a guarded writer begins with.
    fenced: Arc<str>,
    deadline: Option<Duration>,
}

impl TransactionPrelude {
    /// The prelude installing `guards`.
    pub fn new(guards: &TransactionGuards) -> Self {
        Self::with_begin("BEGIN", guards)
    }

    /// The prelude installing `guards` after `begin`, a `BEGIN` with its
    /// isolation options (`BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`).
    pub fn with_begin(begin: &str, guards: &TransactionGuards) -> Self {
        let mut settings = String::new();
        for (name, value) in guard_settings(guards) {
            settings.push_str("SET LOCAL ");
            settings.push_str(name);
            settings.push_str(" = ");
            settings.push_str(&value);
            settings.push_str("; ");
        }
        let statement = if settings.is_empty() {
            begin.to_owned()
        } else {
            format!("{begin}; {}", settings.trim_end_matches("; "))
        };
        Self {
            begin: statement.into(),
            fenced: fenced_begin(&settings).into(),
            deadline: guards.operation_deadline,
        }
    }

    /// A plain `BEGIN` that installs nothing: a migration's transactions
    /// keep the deployment's settings.
    pub(crate) fn inherit() -> Self {
        Self {
            begin: "BEGIN".into(),
            fenced: fenced_begin("").into(),
            deadline: None,
        }
    }

    /// The `BEGIN` statement, guards included.
    pub fn statement(&self) -> &str {
        &self.begin
    }

    /// A guarded writer's `BEGIN`: the guards, then the writer fence's
    /// shared lock, then `BEGIN`, in one simple query
    /// ([`crate::guarded_tx`]).
    pub(crate) fn fenced(&self) -> &str {
        &self.fenced
    }

    /// The whole-operation deadline of the profile.
    pub fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    /// Begin a guarded transaction on `pool`.
    ///
    /// # Errors
    ///
    /// The driver's error when the checkout or `BEGIN` fails.
    pub async fn begin(
        &self,
        pool: &PgPool,
    ) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
        crate::observed_sql::control(&self.begin, pool.begin_with(self.begin.to_string())).await
    }

    /// Begin a guarded transaction on a connection the caller holds.
    ///
    /// # Errors
    ///
    /// The driver's error when `BEGIN` fails.
    pub async fn begin_on<'c>(
        &self,
        connection: &'c mut sqlx::PgConnection,
    ) -> Result<Transaction<'c, Postgres>, sqlx::Error> {
        crate::observed_sql::control(
            &self.begin,
            sqlx::Connection::begin_with(connection, self.begin.to_string()),
        )
        .await
    }

    /// Run `operation` within the profile's whole-operation deadline:
    /// `Err(elapsed)` when it did not finish in time.
    pub async fn bounded<T>(
        &self,
        operation: impl std::future::Future<Output = T>,
    ) -> Result<T, Duration> {
        match self.deadline {
            Some(deadline) => tokio::time::timeout(deadline, operation)
                .await
                .map_err(|_| deadline),
            None => Ok(operation.await),
        }
    }
}

/// A guarded writer's `BEGIN` after `settings` (`SET LOCAL ...; ` each).
///
/// The statements before `BEGIN` run as the simple query's implicit
/// transaction block, which `BEGIN` then turns into the transaction itself,
/// lock and settings kept. So a lock wait the guards refuse rolls that block
/// back whole and leaves the connection outside any transaction, and the
/// fence's read, the transaction's next statement, takes its snapshot only
/// once the lock is held: read committed, whatever the deployment's default.
fn fenced_begin(settings: &str) -> String {
    format!(
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED; {settings}{}; BEGIN",
        crate::connection_sql::connection_sql()
            .lock_xact_fleet_fence_shared
            .sql()
    )
}

/// Makes every connection lash opens, by role.
#[derive(Clone, Debug)]
pub struct PostgresConnectionFactory {
    endpoints: PostgresEndpoints,
    policy: ConnectionPolicy,
}

impl PostgresConnectionFactory {
    /// The factory over `endpoints` under `policy`.
    pub fn new(endpoints: PostgresEndpoints, policy: ConnectionPolicy) -> Self {
        Self { endpoints, policy }
    }

    /// The connect options of `role`, under `session_guards` installed as
    /// session defaults on a direct topology.
    pub fn connect_options(
        &self,
        role: ConnectionRole,
        session_guards: Option<&TransactionGuards>,
    ) -> PgConnectOptions {
        let pooled = self.policy.topology == ConnectionTopology::TransactionPool;
        let base = match (&self.endpoints.session, role.needs_session()) {
            (Some(session), true) => session,
            _ => &self.endpoints.primary,
        };
        let mut options = base
            .clone()
            .application_name(&format!(
                "{}/{}",
                self.policy.application_name_prefix,
                role.as_str()
            ))
            .statement_cache_capacity(self.policy.statement_cache_capacity);
        let transport = &self.policy.transport;
        if let Some(mode) = transport.ssl_mode {
            options = options.ssl_mode(match mode {
                SslMode::Disable => PgSslMode::Disable,
                SslMode::Allow => PgSslMode::Allow,
                SslMode::Prefer => PgSslMode::Prefer,
                SslMode::Require => PgSslMode::Require,
                SslMode::VerifyCa => PgSslMode::VerifyCa,
                SslMode::VerifyFull => PgSslMode::VerifyFull,
            });
        }
        if let Some(root) = &transport.ssl_root_cert {
            options = options.ssl_root_cert(root);
        }
        if let (Some(cert), Some(key)) = (&transport.ssl_client_cert, &transport.ssl_client_key) {
            options = options.ssl_client_cert(cert).ssl_client_key(key);
        }
        // Session defaults ride the startup packet. A transaction pooler
        // would carry them to other clients, so there each transaction's
        // prelude is the only guard (and the session roles connect around
        // the pooler).
        let direct = !pooled || (role.needs_session() && self.endpoints.session.is_some());
        if direct && let Some(guards) = session_guards {
            options = options.options(guard_settings(guards));
        }
        options
    }

    /// A lazily connecting pool of `role` sized and recycled exactly as
    /// `policy` says, `None` idle and lifetime included.
    pub fn pool(
        &self,
        role: ConnectionRole,
        policy: &PoolPolicy,
        session_guards: Option<&TransactionGuards>,
    ) -> PgPool {
        PgPoolOptions::new()
            .max_connections(policy.max_connections)
            .min_connections(policy.min_connections)
            .acquire_timeout(policy.acquire_timeout)
            .idle_timeout(policy.idle_timeout)
            .max_lifetime(policy.max_lifetime)
            .test_before_acquire(policy.test_before_acquire)
            .connect_lazy_with(self.connect_options(role, session_guards))
    }

    /// A lazily connecting pool of `connections` dedicated connections of
    /// `role`, never recycled for idleness or age.
    pub fn dedicated(
        &self,
        role: ConnectionRole,
        policy: &DedicatedConnectionPolicy,
        connections: u32,
        session_guards: Option<&TransactionGuards>,
    ) -> PgPool {
        PgPoolOptions::new()
            .max_connections(connections)
            .min_connections(0)
            .acquire_timeout(policy.acquire_timeout)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_lazy_with(self.connect_options(role, session_guards))
    }
}

/// The pools a host built itself, one per pooled role, for
/// [`PostgresStorage::from_pool_set`](crate::PostgresStorage::from_pool_set).
///
/// The host's own hooks, TLS identity and sizing stay as it built them; the
/// storage reads each pool's real sizing into its effective configuration
/// and refuses a set too small for the configuration's served nodes. Every
/// transaction still begins with its role's [`TransactionPrelude`].
#[derive(Clone, Debug)]
pub struct PostgresPoolSet {
    /// Store calls and ordinary durable commits.
    pub work: PgPool,
    /// Claims, adoption, owned scans, node drain, liveness probes.
    pub scheduler: PgPool,
    /// Reap, release, hand-back, cancels and terminals.
    pub critical: PgPool,
    /// Lease renewal; at least `roles.served_nodes` connections.
    pub renewal: PgPool,
    /// Detached schema and sweep sessions; must reach a server session.
    pub session: PgPool,
    /// What each node's listener session connects with; must reach a server
    /// session.
    pub listener: PgConnectOptions,
}

impl PostgresPoolSet {
    /// Every role on `pool`, and listeners on its connect options: one
    /// physical pool for tests that import a hooked scratch pool.
    #[cfg(any(test, feature = "testing"))]
    pub fn sharing_for_testing(pool: PgPool) -> Self {
        Self {
            listener: (*pool.connect_options()).clone(),
            scheduler: pool.clone(),
            critical: pool.clone(),
            renewal: pool.clone(),
            session: pool.clone(),
            work: pool,
        }
    }
}

/// A pool's sizing as it really is, for the effective configuration.
fn observed_policy(pool: &PgPool) -> PoolPolicy {
    let options = pool.options();
    PoolPolicy {
        max_connections: options.get_max_connections(),
        min_connections: options.get_min_connections(),
        acquire_timeout: options.get_acquire_timeout(),
        idle_timeout: options.get_idle_timeout(),
        max_lifetime: options.get_max_lifetime(),
        test_before_acquire: options.get_test_before_acquire(),
    }
}

/// The guard profiles a storage installs, by kind of transaction.
#[derive(Clone, Debug)]
pub(crate) struct Preludes {
    pub(crate) ordinary: TransactionPrelude,
    pub(crate) durable: TransactionPrelude,
    pub(crate) renewal: TransactionPrelude,
    pub(crate) scheduler: TransactionPrelude,
}

impl Preludes {
    fn new(guards: &GuardPolicies) -> Self {
        Self {
            ordinary: TransactionPrelude::new(&guards.ordinary),
            durable: TransactionPrelude::new(&guards.durable),
            renewal: TransactionPrelude::new(&guards.renewal),
            scheduler: TransactionPrelude::new(&guards.scheduler),
        }
    }
}

/// Every role pool of one storage, shared by each handle it hands out.
pub(crate) struct RolePools {
    pub(crate) work: PgPool,
    pub(crate) scheduler: PgPool,
    pub(crate) critical: PgPool,
    pub(crate) renewal: PgPool,
    pub(crate) session: PgPool,
    pub(crate) listener: PgConnectOptions,
    pub(crate) listener_policy: DedicatedConnectionPolicy,
    pub(crate) reconnect: ReconnectPolicy,
    pub(crate) preludes: Preludes,
    /// `roles.max_store_operations` permits, taken before a durable work
    /// checkout and released with it.
    pub(crate) admission: Arc<Semaphore>,
    /// One permit per served node's listener session.
    pub(crate) listeners: Arc<Semaphore>,
    pub(crate) served_nodes: u32,
    pub(crate) schema_sessions: Arc<Semaphore>,
    pub(crate) sweep_sessions: Arc<Semaphore>,
    pub(crate) maintenance: MaintenancePolicy,
    pub(crate) store_retry: RetryPolicy,
    /// How a contended durable commit runs again.
    pub(crate) durable_retry: RetryPolicy,
    /// How a contended wait resolution or due settlement runs again.
    pub(crate) wait_retry: RetryPolicy,
}

impl RolePools {
    /// The role pools over `pools` under the validated `config`.
    pub(crate) fn new(pools: PostgresPoolSet, config: &PostgresHostConfig) -> Self {
        Self {
            work: pools.work,
            scheduler: pools.scheduler,
            critical: pools.critical,
            renewal: pools.renewal,
            session: pools.session,
            listener: pools.listener,
            listener_policy: config.roles.listener,
            reconnect: config.signals.reconnect,
            preludes: Preludes::new(&config.guards),
            admission: Arc::new(Semaphore::new(config.roles.max_store_operations)),
            listeners: Arc::new(Semaphore::new(config.roles.served_nodes as usize)),
            served_nodes: config.roles.served_nodes,
            schema_sessions: Arc::new(Semaphore::new(
                config.maintenance.max_schema_sessions as usize,
            )),
            sweep_sessions: Arc::new(Semaphore::new(
                config.maintenance.max_sweep_sessions as usize,
            )),
            maintenance: config.maintenance,
            store_retry: config.retry.store,
            durable_retry: config.retry.durable,
            wait_retry: config.retry.wait_resolution,
        }
    }

    /// Pools over `pools` with defaults: the storage seams that open over
    /// one bare pool (migration, performance fixtures).
    pub(crate) fn sharing(pool: PgPool) -> Self {
        Self::new(
            PostgresPoolSet {
                listener: (*pool.connect_options()).clone(),
                scheduler: pool.clone(),
                critical: pool.clone(),
                renewal: pool.clone(),
                session: pool.clone(),
                work: pool,
            },
            &PostgresHostConfig::default(),
        )
    }

    /// The pool metrics handle over these pools.
    pub(crate) fn metrics(self: &Arc<Self>) -> PostgresPoolMetrics {
        PostgresPoolMetrics {
            pools: Arc::clone(self),
        }
    }
}

/// The effective configuration of an imported pool set: each pool's real
/// sizing, refused when the renewal pool cannot give every served node a
/// connection of its own.
pub(crate) fn effective_config(
    pools: &PostgresPoolSet,
    declared: &PostgresHostConfig,
) -> Result<PostgresHostConfig, PostgresHostConfigError> {
    let mut effective = declared.clone();
    effective.roles.work = observed_policy(&pools.work);
    effective.roles.scheduler = observed_policy(&pools.scheduler);
    effective.roles.critical = observed_policy(&pools.critical);
    let renewal = pools.renewal.options().get_max_connections();
    if renewal < declared.roles.served_nodes {
        return Err(PostgresHostConfigError {
            field: "roles.served_nodes".to_owned(),
            reason: format!(
                "needs a renewal connection per served node; the imported renewal pool has {renewal}"
            ),
        });
    }
    Ok(effective)
}

/// The pool set the factory builds for `config`.
pub(crate) fn factory_pool_set(
    factory: &PostgresConnectionFactory,
    config: &PostgresHostConfig,
) -> PostgresPoolSet {
    let roles = &config.roles;
    let guards = &config.guards;
    let maintenance = &config.maintenance;
    // Detached sessions leave the pool's accounting, so the pool keeps
    // nothing idle of its own; the storage's semaphores bound them.
    let session = PoolPolicy {
        max_connections: maintenance.max_schema_sessions + maintenance.max_sweep_sessions,
        min_connections: 0,
        ..roles.work
    };
    PostgresPoolSet {
        work: factory.pool(ConnectionRole::Work, &roles.work, Some(&guards.ordinary)),
        scheduler: factory.pool(
            ConnectionRole::Scheduler,
            &roles.scheduler,
            Some(&guards.scheduler),
        ),
        critical: factory.pool(
            ConnectionRole::Critical,
            &roles.critical,
            Some(&guards.durable),
        ),
        renewal: factory.dedicated(
            ConnectionRole::Renewal,
            &roles.renewal,
            roles.served_nodes,
            Some(&guards.renewal),
        ),
        session: factory.pool(ConnectionRole::Session, &session, Some(&guards.inspection)),
        listener: factory.connect_options(ConnectionRole::Listener, None),
    }
}

/// Read the server's connection capacity, the slots reserved from normal
/// clients included.
pub(crate) async fn connection_capacity<'e, E>(
    executor: E,
) -> Result<PostgresConnectionCapacity, StoreError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let (max_connections, reserved_connections): (i64, i64) = sqlx::query_as(
        "SELECT MAX(setting::bigint) FILTER (WHERE name = 'max_connections'),
                COALESCE(SUM(setting::bigint) FILTER (WHERE name IN ('superuser_reserved_connections', 'reserved_connections')), 0)::bigint
         FROM pg_settings WHERE name IN ('max_connections', 'superuser_reserved_connections', 'reserved_connections')",
    )
    .fetch_one(crate::observed_sql::executor(executor))
    .await
    .map_err(store_sqlx_error)?;
    Ok(PostgresConnectionCapacity {
        max_connections: u32::try_from(max_connections)
            .map_err(|error| StoreError::Backend(error.to_string()))?,
        reserved_connections: u32::try_from(reserved_connections)
            .map_err(|error| StoreError::Backend(error.to_string()))?,
    })
}

/// One role pool's state at a moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostgresRolePoolMetrics {
    pub role: ConnectionRole,
    /// The pool's configured maximum.
    pub max_connections: u32,
    /// Connections open now, checked out or idle.
    pub size: u32,
    /// Open connections waiting in the pool.
    pub idle: u32,
}

/// Per-role pool state of one storage, for a host's metrics.
#[derive(Clone)]
pub struct PostgresPoolMetrics {
    pools: Arc<RolePools>,
}

impl std::fmt::Debug for PostgresPoolMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.snapshot()).finish()
    }
}

impl PostgresPoolMetrics {
    /// Each pooled role's state, and the open listener sessions as the
    /// listener role's size.
    pub fn snapshot(&self) -> Vec<PostgresRolePoolMetrics> {
        let pools = &self.pools;
        let pooled = |role, pool: &PgPool| PostgresRolePoolMetrics {
            role,
            max_connections: pool.options().get_max_connections(),
            size: pool.size(),
            idle: u32::try_from(pool.num_idle()).unwrap_or(u32::MAX),
        };
        let open_listeners = pools
            .served_nodes
            .saturating_sub(u32::try_from(pools.listeners.available_permits()).unwrap_or(u32::MAX));
        vec![
            pooled(ConnectionRole::Work, &pools.work),
            pooled(ConnectionRole::Scheduler, &pools.scheduler),
            pooled(ConnectionRole::Critical, &pools.critical),
            pooled(ConnectionRole::Renewal, &pools.renewal),
            pooled(ConnectionRole::Session, &pools.session),
            PostgresRolePoolMetrics {
                role: ConnectionRole::Listener,
                max_connections: pools.served_nodes,
                size: open_listeners,
                idle: 0,
            },
        ]
    }
}

/// A uniform draw from the upper half of `delay`: retries and reconnects
/// that start together do not stay in step.
fn upper_half(delay: Duration) -> Duration {
    let half = delay / 2;
    let span = u64::try_from((delay - half).as_micros()).unwrap_or(u64::MAX);
    if span == 0 {
        return delay;
    }
    let draw = uuid::Uuid::new_v4().as_u128() as u64;
    half + Duration::from_micros(draw % (span + 1))
}

/// `initial` doubled `doublings` times, capped at `max`.
fn doubled(initial: Duration, max: Duration, doublings: u32) -> Duration {
    1_u32
        .checked_shl(doublings)
        .and_then(|factor| initial.checked_mul(factor))
        .map_or(max, |delay| delay.min(max))
}

impl RetryPolicy {
    /// The pause before retry `retry` (0 before the second attempt).
    pub fn pause(&self, retry: u32) -> Duration {
        let delay = doubled(self.initial_delay, self.max_delay, retry);
        if self.jitter {
            upper_half(delay)
        } else {
            delay
        }
    }
}

impl ReconnectPolicy {
    /// The wait after `failures` consecutive failed reconnects (0 after the
    /// session was lost).
    pub fn wait(&self, failures: u32) -> Duration {
        let delay = doubled(self.initial_delay, self.max_delay, failures);
        if self.jitter {
            upper_half(delay)
        } else {
            delay
        }
    }
}

#[cfg(test)]
mod tests;
