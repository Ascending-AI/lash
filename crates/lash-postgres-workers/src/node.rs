//! One lash node: the production runtime over PostgreSQL, in its own OS
//! process.
//!
//! The node connects to the lash database, assembles the durable backend
//! with the runbook's engine and completion secret, and serves sessions and
//! processes through `lash_core::runtime::durable::node::serve` until it is
//! told to stop on stdin, stdin ends, or it loses its lease. It reports on
//! stdout ([`crate::events`]).

use std::sync::Arc;

use lash_core::runtime::durable::node::{NodeServe, serve};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::{
    Backend, BackendParts, CompletionKeySecrets, DurableSettings, KeyVersion,
    NoProjectionProviders, SecretBytes, StoreSet,
};
use lash_durable::domain::PROCESS_FORMATS;
use lash_durable::runner::Stopped;
use lash_durable::{FormatSet, NoProbe, NodeId, Notifier};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet};
use tokio::io::AsyncBufReadExt as _;

use crate::events::{Command, Event, report};
use crate::process::{WorkerEngine, WorkerSteps};
use crate::recorded::{RecordedStore, RecordedStores};
use crate::turn::WorkerServices;
use crate::witness::{Hold, Witness};

/// What a node runs as, read from its environment.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// `LASH_WORKERS_NODE`: the node's stable name.
    pub node: String,
    /// `LASH_WORKERS_DATABASE_URL`: the lash database.
    pub database_url: String,
    /// `LASH_WORKERS_WITNESS_URL`: the witness ledger, as its writer.
    pub witness_url: String,
    /// `LASH_WORKERS_NOTIFIER`: `after-commit` (wake hints and the liveness
    /// lock) or `poll-only`.
    pub notifier: Notifier,
    /// `LASH_WORKERS_HOLD`: where the workload holds ([`Hold`]).
    pub hold: Hold,
    /// `LASH_WORKERS_COMPLETION_SECRET`: the deployment's completion secret,
    /// at least 32 bytes.
    pub secret: String,
    /// `LASH_WORKERS_ADMIT_TURN=1`: admit the runbook's turn at boot unless
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
        let notifier = match var("LASH_WORKERS_NOTIFIER").as_deref() {
            None | Some("after-commit") => Notifier::AfterCommit,
            Some("poll-only") => Notifier::PollOnly,
            Some(other) => return Err(format!("LASH_WORKERS_NOTIFIER={other} is not a notifier")),
        };
        let hold = var("LASH_WORKERS_HOLD").unwrap_or_default();
        let hold =
            Hold::parse(&hold).ok_or_else(|| format!("LASH_WORKERS_HOLD={hold} is not a hold"))?;
        Ok(Self {
            node: required("LASH_WORKERS_NODE")?,
            database_url: required("LASH_WORKERS_DATABASE_URL")?,
            witness_url: required("LASH_WORKERS_WITNESS_URL")?,
            notifier,
            hold,
            secret: required("LASH_WORKERS_COMPLETION_SECRET")?,
            admit_turn: var("LASH_WORKERS_ADMIT_TURN").as_deref() == Some("1"),
        })
    }
}

/// The settings every node runs with: the defaults, with the notifier the
/// case names.
#[must_use]
pub fn settings(notifier: Notifier) -> DurableSettings {
    DurableSettings {
        notifier,
        ..DurableSettings::default()
    }
}

/// The completion secrets of a deployment whose one secret is `secret`.
///
/// # Errors
///
/// The secret is too short.
pub fn secrets(secret: &str) -> Result<CompletionKeySecrets, String> {
    CompletionKeySecrets::new(
        KeyVersion(1),
        vec![(KeyVersion(1), SecretBytes::new(secret.as_bytes().to_vec()))],
    )
    .map_err(|refusal| refusal.to_string())
}

/// Serve as `config` says until told to stop, stdin ends, or the lease is
/// lost; report why it stopped.
///
/// # Errors
///
/// The node could not connect or assemble, or the store refused its
/// registration.
pub async fn run(config: NodeConfig) -> Result<Stopped, String> {
    let storage = PostgresStorage::connect(&config.database_url)
        .await
        .map_err(|error| format!("connect the lash database: {error}"))?;
    let stores: Arc<dyn StoreSet> = Arc::new(PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
    ));
    let recorded = RecordedStores::new(stores, &config.node);
    let store = recorded.store();
    let backend = Backend::assemble(BackendParts {
        stores: Arc::new(recorded),
        settings: settings(config.notifier),
        secrets: Some(secrets(&config.secret)?),
        engines: vec![Arc::new(WorkerEngine)],
        providers: Arc::new(NoProjectionProviders),
    })
    .map_err(|error| format!("assemble the backend: {error}"))?;
    if config.admit_turn {
        admit_turn(&backend).await?;
    }
    let witness = Witness::connect(&config.witness_url, &config.node)
        .map_err(|error| format!("connect the witness ledger: {error}"))?;
    let sessions = SessionActivation::new(
        backend.clone(),
        Arc::new(WorkerServices::new(
            backend.clone(),
            witness.clone(),
            config.hold,
        )),
        Arc::new(NoProbe),
    );
    let processes = ProcessActivation::new(
        backend.clone(),
        Arc::new(WorkerSteps::new(witness)),
        Arc::new(NoProbe),
    );
    // The commands are read on a task of their own: the runner polls its
    // stop future only between its own steps, and the partition holds it
    // inside one.
    let (stopping, stop) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(commands(store, stopping));
    let stop = async move {
        let _ = stop.await;
    };
    let stopped = serve(
        &backend,
        NodeServe {
            node: NodeId::new(config.node.clone()),
            decodes: vec![
                FormatSet::new(crate::SESSION_FORMATS),
                FormatSet::new(PROCESS_FORMATS),
            ],
            sessions: Arc::new(sessions),
            processes: Arc::new(processes),
        },
        stop,
    )
    .await;
    let why = match &stopped {
        Ok(Stopped::Requested) => "requested".to_owned(),
        Ok(Stopped::LeaseLost) => "lease_lost".to_owned(),
        Ok(Stopped::Unrenewed) => "unrenewed".to_owned(),
        Err(error) => error.to_string(),
    };
    report(&config.node, Event::Stopped { why });
    stopped.map_err(|error| format!("serve: {error}"))
}

/// Admit the runbook's turn unless its session already exists.
async fn admit_turn(backend: &Backend) -> Result<(), String> {
    let durable = backend.durable();
    let exists = durable
        .actor(&crate::turn::actor())
        .await
        .map_err(|error| format!("read the session: {error}"))?
        .is_some();
    if !exists {
        crate::turn::admit(backend).await?;
    }
    Ok(())
}

/// Follow the commands on stdin; ask the node to stop when told to or when
/// stdin ends.
async fn commands(store: Arc<RecordedStore>, stopping: tokio::sync::oneshot::Sender<()>) {
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match Command::parse(&line) {
            Some(Command::Stop) => break,
            Some(Command::BlockHeartbeat) => store.block_heartbeat(true),
            Some(Command::UnblockHeartbeat) => store.block_heartbeat(false),
            None => eprintln!("unknown command `{line}`"),
        }
    }
    let _ = stopping.send(());
}
