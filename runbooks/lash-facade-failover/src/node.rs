//! One lash node: a facade host's core over PostgreSQL, in its own OS
//! process.
//!
//! The node connects to the lash database, builds the durable backend
//! through [`lash::durable::DurableBackendBuilder`] and one
//! [`lash::LashCore`], whose node serves the store's sessions and processes
//! until the host shuts the core down (told to on stdin, or stdin ends) or
//! the node stops on its own (its lease lost or unrenewed), which the host
//! learns from [`lash::LashCore::node_stopped`]. It reports on stdout
//! ([`crate::events`]).

use std::sync::Arc;

use lash::durable::runner::Stopped;
use lash::durable::{DurableBackendBuilder, DurableSettings, Notifier};
use lash::postgres::{PostgresEndpoints, PostgresHostConfig, PostgresStorage, PostgresStoreSet};
use tokio::io::AsyncBufReadExt as _;

use crate::events::{Command, Event, report};
use crate::recorded::{RecordedStore, RecordedStores};
use crate::witness::{Hold, Witness};

/// What a node runs as, read from its environment.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// `LASH_FAILOVER_NODE`: the node's stable name.
    pub node: String,
    /// `LASH_FAILOVER_DATABASE_URL`: the lash database.
    pub database_url: String,
    /// `LASH_FAILOVER_WITNESS_URL`: the witness ledger, as its writer.
    pub witness_url: String,
    /// `LASH_FAILOVER_NOTIFIER`: `after-commit` (wake hints and the liveness
    /// lock) or `poll-only`.
    pub notifier: Notifier,
    /// `LASH_FAILOVER_HOLD`: where the workload holds ([`Hold`]).
    pub hold: Hold,
    /// `LASH_FAILOVER_ADMIT_TURN=1`: admit the runbook's turn at boot unless
    /// its session exists, for runs by hand. The cases admit it themselves.
    pub admit_turn: bool,
}

impl NodeConfig {
    /// Read the configuration from `var`, which answers an environment
    /// variable's value.
    ///
    /// # Errors
    ///
    /// A variable is missing or does not parse.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let required = |name: &str| var(name).ok_or_else(|| format!("{name} is not set"));
        let notifier = match var("LASH_FAILOVER_NOTIFIER").as_deref() {
            None | Some("after-commit") => Notifier::AfterCommit,
            Some("poll-only") => Notifier::PollOnly,
            Some(other) => return Err(format!("LASH_FAILOVER_NOTIFIER={other} is not a notifier")),
        };
        let hold = var("LASH_FAILOVER_HOLD").unwrap_or_default();
        let hold =
            Hold::parse(&hold).ok_or_else(|| format!("LASH_FAILOVER_HOLD={hold} is not a hold"))?;
        Ok(Self {
            node: required("LASH_FAILOVER_NODE")?,
            database_url: required("LASH_FAILOVER_DATABASE_URL")?,
            witness_url: required("LASH_FAILOVER_WITNESS_URL")?,
            notifier,
            hold,
            admit_turn: var("LASH_FAILOVER_ADMIT_TURN").as_deref() == Some("1"),
        })
    }
}

/// The host configuration every node runs with: the defaults, with the
/// notifier the case names.
#[must_use]
pub fn host_config(notifier: Notifier) -> PostgresHostConfig {
    PostgresHostConfig {
        node: DurableSettings {
            notifier,
            ..DurableSettings::default()
        },
        ..PostgresHostConfig::default()
    }
}

/// Serve as `config` says until the core shuts down or its node stops on
/// its own; report why the node stopped.
///
/// # Errors
///
/// The node could not connect or build its core, or the store refused its
/// node.
pub async fn run(config: NodeConfig) -> Result<Stopped, String> {
    let endpoints = PostgresEndpoints::from_url(&config.database_url)
        .map_err(|error| format!("connect the lash database: {error}"))?;
    let storage = PostgresStorage::connect(
        &endpoints,
        &host_config(config.notifier),
        Default::default(),
    )
    .await
    .map_err(|error| format!("connect the lash database: {error}"))?;
    // The workloads write no attachment; their bytes would land here.
    let attachments = std::env::temp_dir().join(format!("lash-facade-failover-{}", config.node));
    let stores: Arc<dyn lash::StoreSet> = Arc::new(PostgresStoreSet::new(
        &storage,
        Arc::new(lash::persistence::FileAttachmentStore::new(attachments)),
    ));
    let recorded = RecordedStores::new(stores, &config.node);
    let store = recorded.store();
    let backend = DurableBackendBuilder::new(Arc::new(recorded))
        .config(storage.effective_config().node)
        .build()
        .map_err(|error| format!("build the backend: {error}"))?;
    let witness = Witness::connect(&config.witness_url, &config.node)
        .map_err(|error| format!("connect the witness ledger: {error}"))?;
    // Built on the runtime, the core starts its node at once.
    let core = crate::turn::core(&backend, witness, config.hold, &config.node, true)?;
    if config.admit_turn {
        admit_turn(&backend, &core).await?;
    }
    // The commands are read on a task of their own: a heartbeat hold or
    // release must reach the store whatever the node awaits.
    tokio::spawn(commands(store, core.clone()));
    let stopped = core.node_stopped().await;
    let why = match &stopped {
        Some(Ok(Stopped::Requested)) => "requested".to_owned(),
        Some(Ok(Stopped::LeaseLost)) => "lease_lost".to_owned(),
        Some(Ok(Stopped::Unrenewed)) => "unrenewed".to_owned(),
        Some(Ok(Stopped::Drained)) => "drained".to_owned(),
        Some(Err(error)) => error.to_string(),
        None => "the core runs no node".to_owned(),
    };
    report(&config.node, Event::Stopped { why });
    match stopped {
        Some(Ok(stopped)) => Ok(stopped),
        Some(Err(error)) => Err(format!("serve: {error}")),
        None => Err("the core runs no node".to_owned()),
    }
}

/// Admit the runbook's turn unless its session already exists.
async fn admit_turn(backend: &lash::Backend, core: &lash::LashCore) -> Result<(), String> {
    let exists = backend
        .durable()
        .actor(&crate::turn::actor())
        .await
        .map_err(|error| format!("read the session: {error}"))?
        .is_some();
    if !exists {
        crate::turn::admit(core).await?;
    }
    Ok(())
}

/// Follow the commands on stdin; shut the core down, which stops its node
/// and releases its actors, when told to or when stdin ends.
async fn commands(store: Arc<RecordedStore>, core: lash::LashCore) {
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match Command::parse(&line) {
            Some(Command::Stop) => break,
            Some(Command::BlockHeartbeat) => store.block_heartbeat(true),
            Some(Command::UnblockHeartbeat) => store.block_heartbeat(false),
            None => eprintln!("unknown command `{line}`"),
        }
    }
    if let Err(error) = core.shutdown().await {
        eprintln!("the core did not shut down cleanly: {error}");
    }
}
