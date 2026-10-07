//! A deployment under measurement: one database, N in-process lash nodes
//! serving it through `lash_core::runtime::durable::node::serve`, each with
//! its own backend (and, on PostgreSQL, its own pool and listener).
//!
//! A producer commits through a node's own backend, as a host's `send()`
//! does in the process that serves it, so its wake hint reaches the runner.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use lash_core::runtime::durable::node::{NodeServe, serve};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::{
    Backend, BackendParts, CompletionKeySecrets, DurableSettings, KeyVersion,
    NoProjectionProviders, SecretBytes, StoreSet,
};
use lash_durable::domain::PROCESS_FORMATS;
use lash_durable::runner::Stopped;
use lash_durable::{DurableError, FormatSet, NoProbe, NodeId};
use lash_postgres_store::{PostgresStorage, PostgresStoreConfig, PostgresStoreSet};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::process::{BenchEngine, NoSteps, ProcessBoard};
use crate::recorder::{Recorder, RecordingStores};
use crate::turn::{BenchServices, SESSION_FORMATS, Scripts};

/// Which database the deployment serves.
#[derive(Clone, Debug)]
pub enum Database {
    /// One SQLite database file.
    Sqlite(PathBuf),
    /// One PostgreSQL database.
    Postgres(String),
}

impl Database {
    /// The dialect's name in reports.
    pub fn dialect(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "sqlite-file",
            Self::Postgres(_) => "postgres",
        }
    }
}

/// One serving node.
pub struct Node {
    /// Its backend: producers on this node commit through it.
    pub backend: Backend,
    storage: Option<PostgresStorage>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<Stopped, DurableError>>,
}

impl Node {
    /// Ask the node to stop, releasing its actors, and wait for it.
    pub async fn stop(mut self) -> Result<Stopped> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let stopped = self.task.await.context("join the node")??;
        if let Some(storage) = self.storage.take() {
            storage.pool().close().await;
        }
        Ok(stopped)
    }
}

/// The completion secret every node verifies keys under.
fn secrets() -> Result<CompletionKeySecrets> {
    CompletionKeySecrets::new(
        KeyVersion(1),
        vec![(
            KeyVersion(1),
            SecretBytes::new(b"durable-substrate-bench-secret-0123456789abcdef".to_vec()),
        )],
    )
    .map_err(|refusal| anyhow::anyhow!("{refusal}"))
}

/// The pool each PostgreSQL node opens.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PoolSize {
    /// The most connections the node's pool holds.
    pub max: u32,
    /// The connections it keeps open.
    pub min: u32,
}

/// The shared parts of a deployment's nodes.
pub struct Deployment {
    /// The database.
    pub database: Database,
    /// Every node's recorder.
    pub recorder: Arc<Recorder>,
    /// The scripted turns.
    pub scripts: Arc<Scripts>,
    /// The engines' feed.
    pub board: Arc<ProcessBoard>,
    /// The substrate parameters every node runs with.
    pub settings: DurableSettings,
    /// Each PostgreSQL node's pool.
    pub pool: PoolSize,
    /// The serving nodes.
    pub nodes: Vec<Node>,
    booted: usize,
}

impl Deployment {
    /// A deployment over `database` with no node yet.
    pub fn new(database: Database, settings: DurableSettings, pool: PoolSize) -> Self {
        Self {
            database,
            recorder: Arc::default(),
            scripts: Arc::default(),
            board: Arc::default(),
            settings,
            pool,
            nodes: Vec::new(),
            booted: 0,
        }
    }

    async fn stores(&self) -> Result<(Arc<dyn StoreSet>, Option<PostgresStorage>)> {
        Ok(match &self.database {
            Database::Sqlite(path) => (
                Arc::new(
                    lash_sqlite_store::SqliteStoreSet::open(path)
                        .await
                        .with_context(|| format!("open {}", path.display()))?,
                ),
                None,
            ),
            Database::Postgres(url) => {
                let storage = PostgresStorage::connect_with(
                    url,
                    PostgresStoreConfig {
                        max_connections: self.pool.max,
                        min_connections: self.pool.min,
                        ..PostgresStoreConfig::default()
                    },
                )
                .await
                .map_err(|error| anyhow::anyhow!("connect PostgreSQL: {error}"))?;
                let stores = PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
                );
                (Arc::new(stores), Some(storage))
            }
        })
    }

    /// A backend over a fresh store set, recorded as `node`'s.
    async fn backend(&self, node: &str) -> Result<(Backend, Option<PostgresStorage>)> {
        let (stores, storage) = self.stores().await?;
        let recorded = RecordingStores::new(stores, node, Arc::clone(&self.recorder));
        let backend = Backend::assemble(BackendParts {
            stores: Arc::new(recorded),
            settings: self.settings,
            secrets: Some(secrets()?),
            engines: vec![Arc::new(BenchEngine::new(Arc::clone(&self.board)))],
            providers: Arc::new(NoProjectionProviders),
        })
        .map_err(|error| anyhow::anyhow!("assemble the backend: {error}"))?;
        Ok((backend, storage))
    }

    /// Boot one more node: a fresh backend and pool, so nothing it serves
    /// is cached from before. Returns its index.
    pub async fn boot(&mut self) -> Result<usize> {
        if matches!(self.database, Database::Sqlite(_)) && !self.nodes.is_empty() {
            bail!("a SQLite deployment is one node");
        }
        let name = format!("node-{}", self.booted);
        self.booted += 1;
        let (backend, storage) = self.backend(&name).await?;
        let sessions = SessionActivation::new(
            backend.clone(),
            Arc::new(BenchServices::new(
                Arc::clone(&self.recorder),
                Arc::clone(&self.scripts),
            )),
            Arc::new(NoProbe),
        );
        let processes =
            ProcessActivation::new(backend.clone(), Arc::new(NoSteps), Arc::new(NoProbe));
        let (stop, stopped) = oneshot::channel::<()>();
        let served = backend.clone();
        let node = NodeId::new(name);
        let task = tokio::spawn(async move {
            serve(
                &served,
                NodeServe {
                    node,
                    decodes: vec![
                        FormatSet::new(SESSION_FORMATS),
                        FormatSet::new(PROCESS_FORMATS),
                    ],
                    sessions: Arc::new(sessions),
                    processes: Arc::new(processes),
                },
                async move {
                    let _ = stopped.await;
                },
            )
            .await
        });
        self.nodes.push(Node {
            backend,
            storage,
            stop: Some(stop),
            task,
        });
        Ok(self.nodes.len() - 1)
    }

    /// Boot `count` nodes.
    pub async fn boot_many(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.boot().await?;
        }
        // Let every node register and take its first claim poll.
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    /// Stop every node.
    pub async fn shutdown(&mut self) -> Result<()> {
        for node in self.nodes.drain(..) {
            node.stop().await?;
        }
        Ok(())
    }

    /// The node a producer of item `index` commits through.
    pub fn producer(&self, index: usize) -> &Backend {
        &self.nodes[index % self.nodes.len()].backend
    }
}

/// Microseconds between two instants, saturating.
pub fn micros(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_micros()).unwrap_or(u64::MAX)
}
