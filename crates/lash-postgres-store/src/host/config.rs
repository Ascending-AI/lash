//! [`PostgresHostConfig`]: every setting of every PostgreSQL connection lash
//! opens, as one plain-data value (FIG-5240).
//!
//! It is serializable, non-secret configuration: durations are whole
//! milliseconds (`*_ms` fields), an unknown field is refused, and an absent
//! section takes its default. Credentials and endpoints are not here; a host
//! supplies them separately as [`PostgresEndpoints`](super::PostgresEndpoints).
//! A leaf policy whose defaults depend on its role ([`PoolPolicy`],
//! [`TransactionGuards`], [`RetryPolicy`], [`ReconnectPolicy`],
//! [`DedicatedConnectionPolicy`]) is given whole when it is given at all, so
//! a partial object never silently takes another role's defaults; so is
//! `node`, the durable substrate's settings.
//!
//! The values take effect only through [`PostgresHostConfig::validate`],
//! which every connect runs before any I/O. `docs/operations/postgres.md`
//! is the reference: each field, the sizing formula and the pooler rules.

use std::path::PathBuf;
use std::time::Duration;

use lash_durable::{DurableSettings, GroupCommit, LeaseSettings, Notifier};
use serde::{Deserialize, Serialize};

use super::serde_ms;
use crate::SchemaCheck;

/// Every lash PostgreSQL connection role's sizes, guards, retries and
/// names, and the durable node settings, validated together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostgresHostConfig {
    /// Transport policy every role shares.
    pub connection: ConnectionPolicy,
    /// Each pooled and dedicated role's capacity.
    pub roles: RolePolicies,
    /// The durable substrate's parameters, the node lease's included. The
    /// durable backend takes them from here and from nowhere else.
    #[serde(with = "DurableSettingsDef")]
    pub node: DurableSettings,
    /// SQL guards and whole-operation deadlines, by operation role.
    pub guards: GuardPolicies,
    /// Bounded retries of known-aborted database work.
    pub retry: RetryPolicies,
    /// The durable notification listener's reconnect policy.
    pub signals: SignalPolicy,
    /// The live replay store; `None` opens no replay pool or listener.
    pub live_replay: Option<LiveReplayPolicy>,
    /// The process replay store; `None` opens no process replay pool or
    /// listener.
    pub process_replay: Option<ProcessReplayPolicy>,
    /// Preflight, migration and detached-session limits.
    pub maintenance: MaintenancePolicy,
    /// What open does when the live schema drifts.
    pub schema_check: SchemaCheck,
    /// The deployment's connection budget. `None` skips the server
    /// capacity check. Both presets leave this optional; a host can declare it.
    pub deployment: Option<DeploymentBudget>,
}

impl Default for PostgresHostConfig {
    fn default() -> Self {
        Self::standard()
    }
}

impl PostgresHostConfig {
    /// Standard production preset: the role sizes, acquire/SQL deadlines,
    /// retry and maintenance values documented in `docs/operations/postgres.md`.
    /// Pool recycling matches SQLx 0.8.6; exact sizes, deadlines and retries
    /// have no universal workload measurement. TLS inherits endpoints,
    /// schema checks enforce compatibility, live and process replay are off, and no
    /// deployment capacity claim is invented (`deployment = None`). Set a
    /// deployment budget to check capacity against the server before serving.
    pub fn standard() -> Self {
        Self {
            connection: ConnectionPolicy::default(),
            roles: RolePolicies::default(),
            node: DurableSettings::standard(),
            guards: GuardPolicies::default(),
            retry: RetryPolicies::default(),
            signals: SignalPolicy::default(),
            live_replay: None,
            process_replay: None,
            maintenance: MaintenancePolicy::default(),
            schema_check: SchemaCheck::default(),
            deployment: None,
        }
    }

    /// Explicit local-development preset: work pool 4, scheduler 1, critical
    /// 1, four store operations, and one preflight/migration connection each.
    /// Checkpoint chunks are 256 and event-release pages 32.
    /// Uses `DurableSettings::development`; other standard values apply,
    /// including schema enforcement. No measurement backs these smaller
    /// capacities. It makes no deployment capacity claim.
    pub fn development() -> Self {
        let mut config = Self::standard();
        config.roles.work.max_connections = 4;
        config.roles.critical.max_connections = 1;
        config.roles.max_store_operations = 4;
        config.maintenance.preflight_pool.max_connections = 1;
        config.maintenance.migration_pool.max_connections = 1;
        config.node = DurableSettings::development();
        config.maintenance.checkpoint_ref_chunk = 256;
        config.maintenance.process_event_release_page_rows = 32;
        config
    }
}

/// Transport policy shared by every role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionPolicy {
    /// Each connection's `application_name` is this prefix and its role:
    /// `lash/work`, `lash/renewal`, ... Default `lash`; 1 to 32 characters of
    /// `[A-Za-z0-9_-]`.
    pub application_name_prefix: String,
    /// TLS settings that override the endpoint's own; none by default.
    pub transport: TransportOverrides,
    /// Prepared statements each connection caches. Default 100 (SQLx's);
    /// 0 disables caching for a pooler that cannot carry them.
    pub statement_cache_capacity: usize,
    /// How the endpoints reach the server.
    pub topology: ConnectionTopology,
}

impl Default for ConnectionPolicy {
    fn default() -> Self {
        Self {
            application_name_prefix: "lash".to_owned(),
            transport: TransportOverrides::default(),
            statement_cache_capacity: 100,
            topology: ConnectionTopology::Direct,
        }
    }
}

/// TLS settings applied over the endpoint's. An absent value keeps the
/// endpoint's own.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransportOverrides {
    /// The TLS mode; production hosts usually want `verify_full`.
    pub ssl_mode: Option<SslMode>,
    /// The trusted root certificate file.
    pub ssl_root_cert: Option<PathBuf>,
    /// The client certificate file; given together with `ssl_client_key`.
    pub ssl_client_cert: Option<PathBuf>,
    /// The client key file; given together with `ssl_client_cert`.
    pub ssl_client_key: Option<PathBuf>,
}

/// PostgreSQL's `sslmode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// How the endpoints reach the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionTopology {
    /// Directly, or through a session-mode pooler: every connection is one
    /// server session for its life. Roles install their guards as session
    /// defaults as well as per transaction.
    #[default]
    Direct,
    /// The primary endpoint is a transaction-mode pooler. Guards are
    /// installed per transaction only, and every role that needs a session
    /// of its own (the listeners, schema and sweep sessions, preflight and
    /// migration) connects through the separate session endpoint.
    TransactionPool,
}

/// A pooled role's sizing and recycling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolPolicy {
    /// The most connections the pool opens; at least 1.
    pub max_connections: u32,
    /// Connections kept open while idle; at most `max_connections`.
    pub min_connections: u32,
    /// How long a checkout may wait, connecting included.
    #[serde(rename = "acquire_timeout_ms", with = "serde_ms")]
    pub acquire_timeout: Duration,
    /// Close a connection idle this long; `null` never closes it for
    /// idleness.
    #[serde(rename = "idle_timeout_ms", with = "serde_ms::option")]
    pub idle_timeout: Option<Duration>,
    /// Recycle a connection this old; `null` never recycles it for age.
    #[serde(rename = "max_lifetime_ms", with = "serde_ms::option")]
    pub max_lifetime: Option<Duration>,
    /// Ping a connection before handing it out.
    pub test_before_acquire: bool,
}

impl PoolPolicy {
    const fn standard(max_connections: u32, acquire_timeout: Duration) -> Self {
        Self {
            max_connections,
            min_connections: 0,
            acquire_timeout,
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
            test_before_acquire: true,
        }
    }
}

/// A role served by connections of its own, one per served node, that are
/// never recycled for idleness or age.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedConnectionPolicy {
    /// How long opening (or reopening) the connection may take.
    #[serde(rename = "acquire_timeout_ms", with = "serde_ms")]
    pub acquire_timeout: Duration,
}

/// Each role's capacity.
///
/// The routing is lash's, not the host's: node registration and heartbeat
/// run on the renewal connections; claim, adoption, owned scans, node drain
/// and liveness probes on the scheduler pool; reap, node release, hand-back
/// and every cancel and terminal commit on the critical pool; everything
/// else on the work pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RolePolicies {
    /// Store calls and ordinary durable commits. Default 16 connections,
    /// 30 s acquire.
    pub work: PoolPolicy,
    /// Claims, adoption, liveness. Default 1 connection, 500 ms acquire.
    pub scheduler: PoolPolicy,
    /// Reap, release, hand-back, cancels and terminals. Default 3
    /// connections, 2 s acquire.
    pub critical: PoolPolicy,
    /// Lease renewal: one connection per served node. Default 500 ms acquire.
    pub renewal: DedicatedConnectionPolicy,
    /// The notification listener, one session per served node, which also
    /// holds the node's liveness lock. Default 2 s acquire.
    pub listener: DedicatedConnectionPolicy,
    /// Nodes this process serves over one storage; each has its own renewal
    /// connection and listener session. Default 1.
    pub served_nodes: u32,
    /// Durable engine operations on the work pool at once; the rest of the
    /// work pool serves store API calls. Admission is taken before checkout
    /// and released with the connection, never held across a tool or model
    /// body. Default 16; 1 to `work.max_connections`.
    pub max_store_operations: usize,
}

impl Default for RolePolicies {
    fn default() -> Self {
        Self {
            work: PoolPolicy::standard(16, Duration::from_secs(30)),
            scheduler: PoolPolicy::standard(1, Duration::from_millis(500)),
            critical: PoolPolicy::standard(3, Duration::from_secs(2)),
            renewal: DedicatedConnectionPolicy {
                acquire_timeout: Duration::from_millis(500),
            },
            listener: DedicatedConnectionPolicy {
                acquire_timeout: Duration::from_secs(2),
            },
            served_nodes: 1,
            max_store_operations: 16,
        }
    }
}

/// A server-side timeout: left as the server, role or database sets it,
/// turned off, or a limit.
///
/// Serialized as `"inherit"`, `"disabled"`, or a whole number of
/// milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerTimeout {
    /// Install nothing: the deployment's own setting applies.
    Inherit,
    /// Install `0`: no limit.
    Disabled,
    /// Install this limit, at least 1 ms and at most `i32::MAX` ms.
    Limit(Duration),
}

impl ServerTimeout {
    const fn ms(millis: u64) -> Self {
        Self::Limit(Duration::from_millis(millis))
    }

    /// The limit, when there is one.
    pub fn limit(self) -> Option<Duration> {
        match self {
            Self::Limit(limit) => Some(limit),
            Self::Inherit | Self::Disabled => None,
        }
    }
}

impl Serialize for ServerTimeout {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inherit => serializer.serialize_str("inherit"),
            Self::Disabled => serializer.serialize_str("disabled"),
            Self::Limit(limit) => serde_ms::serialize(limit, serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ServerTimeout {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Millis(u64),
            Word(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Millis(millis) => Ok(Self::Limit(Duration::from_millis(millis))),
            Wire::Word(word) => match word.as_str() {
                "inherit" => Ok(Self::Inherit),
                "disabled" => Ok(Self::Disabled),
                _ => Err(serde::de::Error::custom(format!(
                    "expected \"inherit\", \"disabled\" or milliseconds, found \"{word}\""
                ))),
            },
        }
    }
}

/// The limits one kind of transaction runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionGuards {
    /// `lock_timeout`: the longest one lock wait, the first one included.
    #[serde(rename = "lock_timeout")]
    pub lock: ServerTimeout,
    /// `statement_timeout`: the longest one statement.
    #[serde(rename = "statement_timeout")]
    pub statement: ServerTimeout,
    /// `idle_in_transaction_session_timeout`: the longest a transaction
    /// may sit idle; the server ends the session after it.
    #[serde(rename = "idle_in_transaction_timeout")]
    pub idle_in_transaction: ServerTimeout,
    /// `transaction_timeout` (PostgreSQL 17+): the longest whole
    /// transaction; the server ends the session after it.
    #[serde(rename = "transaction_timeout")]
    pub transaction: ServerTimeout,
    /// The client's bound on the whole operation: admission, checkout,
    /// every statement and commit. `null` only for explicit operator work.
    #[serde(rename = "operation_deadline_ms", with = "serde_ms::option")]
    pub operation_deadline: Option<Duration>,
}

impl TransactionGuards {
    const fn new(lock: u64, statement: u64, idle: u64, operation: u64) -> Self {
        Self {
            lock: ServerTimeout::ms(lock),
            statement: ServerTimeout::ms(statement),
            idle_in_transaction: ServerTimeout::ms(idle),
            transaction: ServerTimeout::Inherit,
            operation_deadline: Some(Duration::from_millis(operation)),
        }
    }
}

/// The guard profile of each kind of transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuardPolicies {
    /// Store transactions and reads on the work pool. Default lock 10 s,
    /// statement 30 s, idle 30 s, operation 60 s.
    pub ordinary: TransactionGuards,
    /// Durable engine commits on the work and critical pools. Default lock
    /// 2 s, statement 5 s, idle 5 s, operation 10 s.
    pub durable: TransactionGuards,
    /// Node registration and heartbeat. Default lock 250 ms, statement 1 s,
    /// idle 1 s, operation 2 s; the operation deadline must be shorter than
    /// the heartbeat interval.
    pub renewal: TransactionGuards,
    /// Claims and the scheduler's other operations, which hold the node row
    /// `FOR SHARE`. Default lock 250 ms, statement 1 s, idle 1 s, operation
    /// 2 s.
    pub scheduler: TransactionGuards,
    /// Live replay and process replay transactions. Default lock 10 s,
    /// statement 30 s, idle 30 s, operation 60 s.
    pub replay: TransactionGuards,
    /// Schema verification and preflight inspection. Default lock 5 s,
    /// statement 30 s, idle 30 s, operation 30 s.
    pub inspection: TransactionGuards,
    /// The whole store open: capacity check, pools, schema verification.
    /// Default 30 s.
    #[serde(rename = "store_startup_ms", with = "serde_ms")]
    pub store_startup: Duration,
}

impl Default for GuardPolicies {
    fn default() -> Self {
        Self {
            ordinary: TransactionGuards::new(10_000, 30_000, 30_000, 60_000),
            durable: TransactionGuards::new(2_000, 5_000, 5_000, 10_000),
            renewal: TransactionGuards::new(250, 1_000, 1_000, 2_000),
            scheduler: TransactionGuards::new(250, 1_000, 1_000, 2_000),
            replay: TransactionGuards::new(10_000, 30_000, 30_000, 60_000),
            inspection: TransactionGuards::new(5_000, 30_000, 30_000, 30_000),
            store_startup: Duration::from_secs(30),
        }
    }
}

/// A bounded retry of database work whose attempt is known to have rolled
/// back, run by [`RetryPolicy::run`] around one complete, immutable
/// transaction. Never a retry of a tool or model call, and never of a
/// `COMMIT` whose outcome is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Attempts, the first included; at least 1.
    pub attempts: u32,
    /// The pause before the second attempt; each later pause doubles it.
    #[serde(rename = "initial_delay_ms", with = "serde_ms")]
    pub initial_delay: Duration,
    /// The longest pause; at least the initial delay.
    #[serde(rename = "max_delay_ms", with = "serde_ms")]
    pub max_delay: Duration,
    /// Draw each pause at random from its upper half, so contending
    /// writers do not retry in step.
    pub jitter: bool,
}

impl RetryPolicy {
    const fn new(attempts: u32, initial: u64, max: u64) -> Self {
        Self {
            attempts,
            initial_delay: Duration::from_millis(initial),
            max_delay: Duration::from_millis(max),
            jitter: true,
        }
    }
}

/// The retry policy of each kind of database work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicies {
    /// Contended store maintenance transactions. Default 4 attempts, 5 to
    /// 20 ms.
    pub store: RetryPolicy,
    /// Contended durable owner and mailbox commits. Default 4 attempts, 5
    /// to 20 ms.
    pub durable: RetryPolicy,
    /// Contended wait resolutions and due-wait settlements. Default 3
    /// attempts, 5 to 20 ms.
    pub wait_resolution: RetryPolicy,
    /// Contended live replay publications and trims. Default 8 attempts,
    /// 5 to 100 ms.
    pub live_replay: RetryPolicy,
    /// Contended process replay publications and trims. Default 8
    /// attempts, 5 to 100 ms.
    pub process_replay: RetryPolicy,
}

impl Default for RetryPolicies {
    fn default() -> Self {
        Self {
            store: RetryPolicy::new(4, 5, 20),
            durable: RetryPolicy::new(4, 5, 20),
            wait_resolution: RetryPolicy::new(3, 5, 20),
            live_replay: RetryPolicy::new(8, 5, 100),
            process_replay: RetryPolicy::new(8, 5, 100),
        }
    }
}

/// How a lost listener session is reopened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconnectPolicy {
    /// The first wait after a lost session; each failed attempt doubles it.
    #[serde(rename = "initial_delay_ms", with = "serde_ms")]
    pub initial_delay: Duration,
    /// The longest wait between attempts.
    #[serde(rename = "max_delay_ms", with = "serde_ms")]
    pub max_delay: Duration,
    /// Draw each wait at random from its upper half.
    pub jitter: bool,
}

/// The durable notification listener's policy. Durable polling stays
/// authoritative while a listener reconnects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SignalPolicy {
    /// Default 250 ms to 5 s with jitter.
    pub reconnect: ReconnectPolicy,
}

impl Default for SignalPolicy {
    fn default() -> Self {
        Self {
            reconnect: ReconnectPolicy {
                initial_delay: Duration::from_millis(250),
                max_delay: Duration::from_secs(5),
                jitter: true,
            },
        }
    }
}

/// Whether a replay store creates its tables or only checks them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySchemaMode {
    /// Create the schema and its tables when absent, from the published
    /// DDL verbatim. Needs `CREATE` on the database.
    Install,
    /// Run no DDL: the host applied the published artifact itself. Absent
    /// or different tables refuse the connect.
    #[default]
    VerifyOnly,
}

/// Where the live replay store keeps its tables, how it batches and what it
/// retains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReplayDataPolicy {
    /// The schema holding the tables; it also names the notification
    /// channel. Default `lash_live_replay`; a lowercase identifier of at
    /// most 48 characters.
    pub schema: String,
    /// Default `verify_only`.
    pub schema_mode: ReplaySchemaMode,
    /// How long a replica gathers publications before one transaction
    /// writes them. Default 5 ms, 0 to 1 s.
    #[serde(rename = "publish_tick_ms", with = "serde_ms")]
    pub publish_tick: Duration,
    /// Publish transactions at once. Default 4; 1 to the data pool's
    /// `max_connections`.
    pub publish_concurrency: usize,
    /// The events a tick gathers into one transaction; a single
    /// publication is never split. Default 1024, 1 to 65536.
    pub max_batch_events: usize,
    /// Events retained per session. Default 2048, 1 to 1,000,000.
    pub max_events_per_session: usize,
    /// How long an event stays replayable. Default 120 s, 1 ms to 24 h.
    #[serde(rename = "max_age_ms", with = "serde_ms")]
    pub max_age: Duration,
    /// Encoded bytes retained per session; a larger single publication is
    /// refused. Default 8 MiB, 1 KiB to 1 GiB.
    pub max_bytes_per_session: usize,
    /// How often a replica reclaims expired events. Default 30 s, 100 ms to
    /// 1 h.
    #[serde(rename = "cleanup_interval_ms", with = "serde_ms")]
    pub cleanup_interval: Duration,
    /// The most a cleanup run is delayed past its interval, drawn at random.
    /// Default 10 s; at most the interval.
    #[serde(rename = "cleanup_jitter_ms", with = "serde_ms")]
    pub cleanup_jitter: Duration,
    /// Rows one cleanup statement reclaims. Default 256, 1 to 65536.
    pub cleanup_batch: usize,
}

impl Default for ReplayDataPolicy {
    fn default() -> Self {
        Self {
            schema: "lash_live_replay".to_owned(),
            schema_mode: ReplaySchemaMode::VerifyOnly,
            publish_tick: Duration::from_millis(5),
            publish_concurrency: 4,
            max_batch_events: 1024,
            max_events_per_session: 2048,
            max_age: Duration::from_secs(120),
            max_bytes_per_session: 8 * 1024 * 1024,
            cleanup_interval: Duration::from_secs(30),
            cleanup_jitter: Duration::from_secs(10),
            cleanup_batch: 256,
        }
    }
}

/// The live replay store: its data, its data pool and its own listener.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LiveReplayPolicy {
    pub data: ReplayDataPolicy,
    /// Publication, reads, cleanup and reloads. Default 7 connections, 5 s
    /// acquire, no checkout ping.
    pub pool: PoolPolicy,
    /// The replay listener's own session. Default 5 s acquire.
    pub listener: DedicatedConnectionPolicy,
    /// Default 100 ms to 5 s with jitter.
    pub reconnect: ReconnectPolicy,
}

impl Default for LiveReplayPolicy {
    fn default() -> Self {
        Self {
            data: ReplayDataPolicy::default(),
            pool: PoolPolicy {
                test_before_acquire: false,
                ..PoolPolicy::standard(7, Duration::from_secs(5))
            },
            listener: DedicatedConnectionPolicy {
                acquire_timeout: Duration::from_secs(5),
            },
            reconnect: ReconnectPolicy {
                initial_delay: Duration::from_millis(100),
                max_delay: Duration::from_secs(5),
                jitter: true,
            },
        }
    }
}

/// Where the process replay store keeps its tables, how it batches and what
/// it retains. The retention values are a provisional preset: no measurement
/// backs them.
///
/// A process's window is cut by whichever bound it reaches first, so the
/// replay it can offer lasts about
/// `min(max_age, max_events / events per second, max_bytes / bytes per second)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessReplayDataPolicy {
    /// The schema holding the tables; `<schema>_process_replay` names the
    /// notification channel. Default `lash_process_replay`; a lowercase
    /// identifier of at most 48 characters.
    pub schema: String,
    /// Default `verify_only`.
    pub schema_mode: ReplaySchemaMode,
    /// How long a replica gathers publications before one transaction
    /// writes them. Default 5 ms, 0 to 1 s.
    #[serde(rename = "publish_tick_ms", with = "serde_ms")]
    pub publish_tick: Duration,
    /// Publish transactions at once. Default 4; 1 to the data pool's
    /// `max_connections`.
    pub publish_concurrency: usize,
    /// The events a tick gathers into one transaction; a single
    /// publication is never split. Default 1024, 1 to 65536.
    pub max_batch_events: usize,
    /// Events a replica holds between `publish` and their tick; a publisher
    /// past it waits. Default 8192, 1 to 1,000,000.
    pub max_pending_events: usize,
    /// Encoded bytes a replica holds between `publish` and their tick; a
    /// publisher past it waits. Default 16 MiB, 1 KiB to 1 GiB.
    pub max_pending_bytes: usize,
    /// Events retained per process. Default 2048, 1 to 1,000,000.
    pub max_events_per_process: usize,
    /// How long an event stays replayable, whether or not the process has
    /// ended. Default 120 s, 1 ms to 24 h.
    #[serde(rename = "max_age_ms", with = "serde_ms")]
    pub max_age: Duration,
    /// Encoded bytes retained per process; a larger single publication is
    /// refused. Default 8 MiB, 1 KiB to 1 GiB.
    pub max_bytes_per_process: usize,
    /// Processes with a window at once, across every replica; the idlest
    /// window is evicted to admit another. Default 4096, 1 to 1,000,000.
    pub max_processes: usize,
    /// Encoded bytes reserved across every process's window, across every
    /// replica. Default 256 MiB; `max_bytes_per_process` to 1 TiB.
    pub max_retained_bytes: u64,
    /// The step a process's share of `max_retained_bytes` grows and shrinks
    /// by: a window takes the aggregate lock once per step, not per event.
    /// Default 64 KiB; 1 KiB to `max_bytes_per_process`.
    pub reservation_bytes: usize,
    /// How often a replica reclaims expired events, idle windows and unused
    /// reservations. Default 30 s, 100 ms to 1 h.
    #[serde(rename = "cleanup_interval_ms", with = "serde_ms")]
    pub cleanup_interval: Duration,
    /// The most a cleanup run is delayed past its interval, drawn at random.
    /// Default 10 s; at most the interval.
    #[serde(rename = "cleanup_jitter_ms", with = "serde_ms")]
    pub cleanup_jitter: Duration,
    /// Rows one cleanup statement reclaims. Default 256, 1 to 65536.
    pub cleanup_batch: usize,
}

impl Default for ProcessReplayDataPolicy {
    fn default() -> Self {
        Self {
            schema: "lash_process_replay".to_owned(),
            schema_mode: ReplaySchemaMode::VerifyOnly,
            publish_tick: Duration::from_millis(5),
            publish_concurrency: 4,
            max_batch_events: 1024,
            max_pending_events: 8192,
            max_pending_bytes: 16 * 1024 * 1024,
            max_events_per_process: 2048,
            max_age: Duration::from_secs(120),
            max_bytes_per_process: 8 * 1024 * 1024,
            max_processes: 4096,
            max_retained_bytes: 256 * 1024 * 1024,
            reservation_bytes: 64 * 1024,
            cleanup_interval: Duration::from_secs(30),
            cleanup_jitter: Duration::from_secs(10),
            cleanup_batch: 256,
        }
    }
}

/// The process replay store: its data, its data pool and its own listener.
/// It shares nothing with the live replay store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessReplayPolicy {
    pub data: ProcessReplayDataPolicy,
    /// Publication, reads and cleanup. Default 7 connections, 5 s acquire,
    /// no checkout ping.
    pub pool: PoolPolicy,
    /// The process replay listener's own session. Default 5 s acquire.
    pub listener: DedicatedConnectionPolicy,
    /// Default 100 ms to 5 s with jitter.
    pub reconnect: ReconnectPolicy,
}

impl Default for ProcessReplayPolicy {
    fn default() -> Self {
        Self {
            data: ProcessReplayDataPolicy::default(),
            pool: PoolPolicy {
                test_before_acquire: false,
                ..PoolPolicy::standard(7, Duration::from_secs(5))
            },
            listener: DedicatedConnectionPolicy {
                acquire_timeout: Duration::from_secs(5),
            },
            reconnect: ReconnectPolicy {
                initial_delay: Duration::from_millis(100),
                max_delay: Duration::from_secs(5),
                jitter: true,
            },
        }
    }
}

/// Operator work and the store's detached sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenancePolicy {
    /// A preflight probe's pool. Default 2 connections, 5 s acquire.
    pub preflight_pool: PoolPolicy,
    /// `lash migrate`'s pool. Default 2 connections, 30 s acquire.
    pub migration_pool: PoolPolicy,
    /// How long a migration waits for the schema lock. Default 30 s.
    #[serde(rename = "migration_lock_timeout_ms", with = "serde_ms")]
    pub migration_lock_timeout: Duration,
    /// Migration statements' `statement_timeout`. Default `inherit`.
    pub migration_statement_timeout: ServerTimeout,
    /// The whole migration job's deadline. Default none.
    #[serde(rename = "migration_deadline_ms", with = "serde_ms::option")]
    pub migration_deadline: Option<Duration>,
    /// Rows one backfill batch moves. Default 500.
    pub migration_batch_rows: u32,
    /// Attachment sweep sessions open at once per storage. Default 1.
    pub max_sweep_sessions: u32,
    /// Schema verification sessions open at once per storage. Default 1.
    pub max_schema_sessions: u32,
    /// The sweep's liveness probe lock wait. Default 500 ms.
    #[serde(rename = "sweep_liveness_probe_timeout_ms", with = "serde_ms")]
    pub sweep_liveness_probe_timeout: Duration,
    /// Rows one process-event release page reads. Default 256.
    pub process_event_release_page_rows: u32,
    /// Checkpoint refs/bodies in one query; standard 16,384 (around 1 MiB
    /// of hash text). No workload measurement establishes this chunk size.
    pub checkpoint_ref_chunk: u32,
    /// Sweep-generation collision retries; standard 3, unmeasured.
    pub sweep_mint_attempts: u32,
}

impl Default for MaintenancePolicy {
    fn default() -> Self {
        Self {
            preflight_pool: PoolPolicy::standard(2, Duration::from_secs(5)),
            migration_pool: PoolPolicy::standard(2, Duration::from_secs(30)),
            migration_lock_timeout: Duration::from_secs(30),
            migration_statement_timeout: ServerTimeout::Inherit,
            migration_deadline: None,
            migration_batch_rows: 500,
            max_sweep_sessions: 1,
            max_schema_sessions: 1,
            sweep_liveness_probe_timeout: Duration::from_millis(500),
            process_event_release_page_rows: 256,
            checkpoint_ref_chunk: 16_384,
            sweep_mint_attempts: 3,
        }
    }
}

/// What a deployment runs beside one process's lash connections, for the
/// server-wide budget. The process's own connection count is derived from
/// the rest of the configuration, never entered by hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentBudget {
    /// Processes of one release running at once.
    pub processes_per_generation: u32,
    /// Releases that overlap during a rollout; at least 2.
    pub generations: u32,
    /// Other clients of the server, other databases' included.
    pub other_clients: u32,
    /// Slots kept for administration; at least the server's reserved slots.
    pub admin_headroom: u32,
    /// Other pools each process opens on the server (a host's own).
    pub other_host_connections: u32,
    /// Operator pools that may overlap a process (preflight, migration).
    pub operator_connections: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "LeaseSettings", deny_unknown_fields)]
struct LeaseSettingsDef {
    #[serde(rename = "ttl_ms", with = "serde_ms")]
    ttl: Duration,
    #[serde(rename = "heartbeat_every_ms", with = "serde_ms")]
    heartbeat_every: Duration,
    #[serde(rename = "self_stop_after_ms", with = "serde_ms")]
    self_stop_after: Duration,
    #[serde(rename = "reap_every_ms", with = "serde_ms")]
    reap_every: Duration,
    #[serde(rename = "claim_poll_ms", with = "serde_ms")]
    claim_poll: Duration,
    #[serde(rename = "claim_backoff_ms", with = "serde_ms")]
    claim_backoff: Duration,
    #[serde(rename = "startup_ms", with = "serde_ms")]
    startup: Duration,
    #[serde(rename = "shutdown_ms", with = "serde_ms")]
    shutdown: Duration,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "GroupCommit", deny_unknown_fields)]
struct GroupCommitDef {
    max_members: usize,
    #[serde(rename = "window_ms", with = "serde_ms")]
    window: Duration,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Notifier", rename_all = "snake_case")]
enum NotifierDef {
    PollOnly,
    AfterCommit,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "DurableSettings", deny_unknown_fields)]
struct DurableSettingsDef {
    #[serde(with = "LeaseSettingsDef")]
    lease: LeaseSettings,
    claim_batch: usize,
    max_active: usize,
    #[serde(rename = "idle_evict_ms", with = "serde_ms")]
    idle_evict: Duration,
    activation_loop_budget: u32,
    #[serde(with = "GroupCommitDef")]
    group_commit: GroupCommit,
    snapshot_every_fuel: u64,
    cascade_batch: usize,
    #[serde(with = "NotifierDef")]
    notifier: Notifier,
}
