//! A [`ProcessReplayStore`] over PostgreSQL, shared by every replica of a
//! host: how provisional process observation crosses replicas.
//!
//! It has its own tables, incarnation, notification channel, pool and
//! listener, and shares none of them with the session live replay store
//! (D-PROCOBS): losing either store's history gaps only its own subjects,
//! and a process burst cannot evict a session's window.
//!
//! The table is the log and notifications are only a doorbell:
//!
//! - **Publish.** A replica gathers its processes' batches for one tick and
//!   writes them in one transaction that locks each process's head row,
//!   drops redeliveries, assigns positions under the lock, appends the
//!   events, trims the window and rings one `pg_notify`. The row lock
//!   serialises one process's writers on every replica, so commit order is
//!   position order and no position is reserved and abandoned.
//! - **Ingress.** What a replica holds between `publish` and its tick is
//!   bounded by events and by bytes; a publisher past either bound waits.
//! - **Subscribe.** One LISTEN connection per replica, on a session of its
//!   own beside the data pool. A subscriber registers its process's
//!   doorbell, refcounted by the replica's subscribers, only once the
//!   LISTEN is confirmed; then it reads the rows past its cursor, and each
//!   doorbell after that re-reads past the last position it delivered. A
//!   missed doorbell (a listener reconnect) rings every subscription, which
//!   re-reads from its cursor.
//! - **Incarnation.** One logged row names the history the unlogged tables
//!   hold, and one unlogged sentinel row proves they still hold it. Crash
//!   recovery and failover truncate unlogged tables; a missing sentinel
//!   rotates the incarnation, and every older cursor gaps.
//! - **Retention.** Count, age and bytes per process, judged by database
//!   time, and two aggregate bounds across every replica: resident
//!   processes, and the bytes their windows reserve. A periodic jittered
//!   pass reclaims expired rows, idle processes and unused reservations; a
//!   process a replica's subscriber follows is not idle.
//! - **Schema.** The tables come from the published
//!   `postgres-process-replay-schema.sql`, which the store executes in its
//!   `install` schema mode and a host applies itself for the `verify_only`
//!   mode. Every connect checks them against the generated shape artifact
//!   and refuses on drift.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use lash_core::{
    ProcessId, ProcessObservationCursor, ProcessObservationEvent, ProcessReplayEventDraft,
    ProcessReplayStore, ProcessReplayStoreError, ProcessReplaySubscribeOutcome,
    ProcessReplaySubscription, ProcessSequence,
};
use lash_postgres_store::host::{
    ConnectionRole, PostgresHostConfig, ProcessReplayDataPolicy, ReconnectPolicy, ReplaySchemaMode,
    RetryPolicy,
};
use lash_postgres_store::{
    PostgresConnectionFactory, PostgresEndpoints, PostgresHostConfigError, TransactionPrelude,
};
use lash_sansio::sync::MutexExt as _;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

mod cleanup;
mod codec;
mod heads;
mod listener;
mod publisher;
mod schema;
mod schema_shape;
mod subscription;

pub use schema_shape::{PostgresProcessReplaySchemaFinding, PostgresProcessReplaySchemaReport};

use codec::Doorbell;
use schema::Statements;
use subscription::Read;

/// Why a [`PostgresProcessReplayStore`] could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostgresProcessReplayError {
    /// A configuration value is outside its range, or the configuration
    /// has no `process_replay` section.
    #[error(transparent)]
    Config(#[from] PostgresHostConfigError),
    /// The database refused the connection, the tables or the listener.
    #[error(transparent)]
    Database(#[from] ProcessReplayStoreError),
    /// The store's tables differ from the published artifact; nothing was
    /// repaired.
    #[error("the postgres process replay tables refuse: {0}")]
    SchemaDrift(PostgresProcessReplaySchemaReport),
}

/// The process replay store every replica of a host shares through one
/// PostgreSQL database. Plug it in with
/// [`LashCoreBuilder::process_replay_store`](crate::LashCoreBuilder::process_replay_store).
pub struct PostgresProcessReplayStore {
    shared: Arc<Shared>,
    publisher: mpsc::UnboundedSender<publisher::PublishRequest>,
    /// The events and the bytes waiting for a tick: the publisher queue's
    /// bounds.
    pending_events: Arc<Semaphore>,
    pending_bytes: Arc<Semaphore>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for PostgresProcessReplayStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresProcessReplayStore")
            .field("schema", &self.shared.config.schema)
            .finish_non_exhaustive()
    }
}

/// What the store's tasks and subscriptions share.
struct Shared {
    /// Publication, reads and cleanup.
    pool: sqlx::PgPool,
    /// The listener's own session.
    listener_pool: sqlx::PgPool,
    config: ProcessReplayDataPolicy,
    /// How long a subscribe or a connect waits for the listener.
    listener_wait: std::time::Duration,
    reconnect: ReconnectPolicy,
    retry: RetryPolicy,
    /// The replay guard profile each publication and cleanup transaction
    /// begins with.
    prelude: TransactionPrelude,
    sql: Statements,
    /// `Some(epoch)` while the listener's LISTEN is confirmed.
    listening: watch::Sender<Option<u64>>,
    bells: StdMutex<HashMap<String, Bell>>,
}

/// A process's doorbell on this replica, refcounted by its subscribers.
struct Bell {
    sender: watch::Sender<u64>,
    subscribers: usize,
}

/// One subscriber's hold on its process's doorbell.
struct BellGuard {
    shared: Arc<Shared>,
    process: String,
    receiver: watch::Receiver<u64>,
}

impl Drop for BellGuard {
    fn drop(&mut self) {
        let mut bells = self.shared.bells.lock_recover();
        if let Some(bell) = bells.get_mut(&self.process) {
            bell.subscribers -= 1;
            if bell.subscribers == 0 {
                bells.remove(&self.process);
            }
        }
    }
}

impl Shared {
    /// Wake this replica's subscribers of every process `doorbell` names:
    /// each re-reads from its cursor.
    fn ring(&self, doorbell: &Doorbell) {
        let bells = self.bells.lock_recover();
        if doorbell.all {
            for bell in bells.values() {
                bell.sender.send_modify(|rings| *rings += 1);
            }
            return;
        }
        for process in &doorbell.processes {
            if let Some(bell) = bells.get(process) {
                bell.sender.send_modify(|rings| *rings += 1);
            }
        }
    }

    /// The processes this replica's subscribers follow.
    fn followed(&self) -> Vec<String> {
        self.bells.lock_recover().keys().cloned().collect()
    }

    /// Register a subscriber's doorbell, once the listener's LISTEN is
    /// confirmed: every doorbell rung after this reaches it.
    async fn bell(
        self: &Arc<Self>,
        process_id: &ProcessId,
    ) -> Result<BellGuard, ProcessReplayStoreError> {
        let mut listening = self.listening.subscribe();
        tokio::time::timeout(self.listener_wait, listening.wait_for(Option::is_some))
            .await
            .map_err(|_| {
                ProcessReplayStoreError::Store(
                    "postgres process replay listener is not connected".to_string(),
                )
            })?
            .map_err(|_| ProcessReplayStoreError::Closed)?;
        let process = process_id.to_string();
        let mut bells = self.bells.lock_recover();
        let bell = bells.entry(process.clone()).or_insert_with(|| Bell {
            sender: watch::channel(0).0,
            subscribers: 0,
        });
        bell.subscribers += 1;
        let mut receiver = bell.sender.subscribe();
        receiver.mark_unchanged();
        Ok(BellGuard {
            shared: Arc::clone(self),
            process,
            receiver,
        })
    }
}

/// `amount` as a share of a `bound`-permit semaphore: a request larger than
/// the whole bound takes all of it, so it waits for an empty queue and its
/// tick decides whether the window can hold it.
fn share(amount: u64, bound: usize) -> u32 {
    u32::try_from(amount.min(bound as u64)).unwrap_or(u32::MAX)
}

impl PostgresProcessReplayStore {
    /// Connect through `endpoints` under `config.process_replay`, create the
    /// store's tables when absent (in the `install` schema mode), refuse
    /// when they differ from the published artifact, confirm the listener,
    /// and start the replica's publisher and cleanup. The configuration is
    /// validated before anything connects; one without a `process_replay`
    /// section is refused.
    pub async fn connect(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
    ) -> Result<Self, PostgresProcessReplayError> {
        config.validate()?;
        let policy = config
            .process_replay
            .clone()
            .ok_or_else(|| PostgresHostConfigError {
                field: "process_replay".to_owned(),
                reason: "is required to open a process replay store".to_owned(),
            })?;
        let factory = PostgresConnectionFactory::new(endpoints.clone(), config.connection.clone());
        let pool = factory.pool(
            ConnectionRole::ProcessReplay,
            &policy.pool,
            Some(&config.guards.replay),
        );
        let listener_pool = factory.dedicated(
            ConnectionRole::ProcessReplayListener,
            &policy.listener,
            1,
            None,
        );
        let report = match policy.data.schema_mode {
            ReplaySchemaMode::Install => schema::install(&pool, &policy.data.schema).await?,
            ReplaySchemaMode::VerifyOnly => schema::verify(&pool, &policy.data.schema).await?,
        };
        if !report.is_conformant() {
            return Err(PostgresProcessReplayError::SchemaDrift(report));
        }
        let sql = Statements::new(&policy.data.schema);
        schema::ensure_incarnation(&pool, &sql).await?;
        let shared = Arc::new(Shared {
            pool,
            listener_pool,
            sql,
            listening: watch::channel(None).0,
            bells: StdMutex::new(HashMap::new()),
            listener_wait: policy.listener.acquire_timeout,
            reconnect: policy.reconnect,
            retry: config.retry.process_replay,
            prelude: TransactionPrelude::new(&config.guards.replay),
            config: policy.data,
        });
        let (publisher, requests) = mpsc::unbounded_channel();
        let store = Self {
            tasks: vec![
                tokio::spawn(publisher::run(Arc::clone(&shared), requests)),
                tokio::spawn(listener::run(Arc::clone(&shared))),
                tokio::spawn(cleanup::run(Arc::clone(&shared))),
            ],
            publisher,
            pending_events: Arc::new(Semaphore::new(shared.config.max_pending_events)),
            pending_bytes: Arc::new(Semaphore::new(shared.config.max_pending_bytes)),
            shared,
        };
        let mut listening = store.shared.listening.subscribe();
        tokio::time::timeout(
            store.shared.listener_wait,
            listening.wait_for(Option::is_some),
        )
        .await
        .map_err(|_| {
            ProcessReplayStoreError::Store(
                "postgres process replay listener did not connect".into(),
            )
        })?
        .map_err(|_| ProcessReplayStoreError::Closed)?;
        Ok(store)
    }

    /// The DDL that provisions the store's tables, committed verbatim as
    /// `crates/lash/postgres-process-replay-schema.sql`.
    ///
    /// The `install` schema mode executes these bytes. A host that owns its
    /// migrations vendors them rather than transcribing them, applies them
    /// into the configured `schema` (they are schema-unqualified and
    /// provision into whichever schema `search_path` resolves), and runs the
    /// store in the `verify_only` mode, the default. Every statement is
    /// creation-only and idempotent.
    pub fn schema_ddl() -> &'static str {
        schema_shape::SCHEMA_DDL
    }

    /// The structure [`schema_ddl`](Self::schema_ddl) produces and every
    /// connect checks, committed as the generated
    /// `crates/lash/postgres-process-replay-schema-shape.txt`.
    pub fn schema_shape() -> &'static str {
        schema_shape::SHAPE_ARTIFACT
    }

    /// The check every connect runs, without connecting a store: a host's
    /// migration CI gates on it. It runs no DDL and never fails on drift;
    /// read [`PostgresProcessReplaySchemaReport::is_conformant`] and render
    /// the report for its findings.
    pub async fn verify_schema(
        pool: &sqlx::PgPool,
        schema: &str,
    ) -> Result<PostgresProcessReplaySchemaReport, ProcessReplayStoreError> {
        schema::verify(pool, schema).await
    }

    /// The data policy this store runs with.
    pub fn config(&self) -> &ProcessReplayDataPolicy {
        &self.shared.config
    }
}

impl Drop for PostgresProcessReplayStore {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        // Closing every doorbell ends the live tails: their observers
        // resubscribe elsewhere.
        self.shared.bells.lock_recover().clear();
    }
}

#[async_trait::async_trait]
impl ProcessReplayStore for PostgresProcessReplayStore {
    /// As many publications at once as the publisher runs transactions;
    /// each of at most what one tick gathers and a quarter of a process's
    /// window, so one batch never displaces what a subscriber has yet to
    /// read of the batch before it.
    fn publish_limits(&self) -> lash_core::ProcessReplayPublishLimits {
        let config = &self.shared.config;
        lash_core::ProcessReplayPublishLimits::new(
            config.publish_concurrency,
            config
                .max_batch_events
                .min(config.max_events_per_process / 4)
                .min(config.max_pending_events),
            (config.max_bytes_per_process / 4).min(config.max_pending_bytes),
        )
    }

    async fn publish(
        &self,
        process_id: &ProcessId,
        events: Vec<ProcessReplayEventDraft>,
    ) -> Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError> {
        if events.is_empty() {
            return Err(ProcessReplayStoreError::Store(
                "cannot publish an empty process replay batch".to_string(),
            ));
        }
        let drafts = events
            .into_iter()
            .map(|draft| codec::encode(process_id, draft))
            .collect::<Result<Vec<_>, _>>()?;
        let config = &self.shared.config;
        let charge = drafts.iter().map(|draft| draft.charge).sum::<u64>();
        // Both bounds are taken in one order, so two publishers never hold
        // one each.
        let held_events = Arc::clone(&self.pending_events)
            .acquire_many_owned(share(drafts.len() as u64, config.max_pending_events))
            .await
            .map_err(|_| ProcessReplayStoreError::Closed)?;
        let held_bytes = Arc::clone(&self.pending_bytes)
            .acquire_many_owned(share(charge, config.max_pending_bytes))
            .await
            .map_err(|_| ProcessReplayStoreError::Closed)?;
        let (reply, answer) = oneshot::channel();
        self.publisher
            .send(publisher::PublishRequest {
                process_id: process_id.clone(),
                drafts,
                reply,
                _ingress: [held_events, held_bytes],
            })
            .map_err(|_| ProcessReplayStoreError::Closed)?;
        answer.await.map_err(|_| ProcessReplayStoreError::Closed)?
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplaySubscribeOutcome, ProcessReplayStoreError> {
        let parsed = cursor.parse()?;
        let bell = self.shared.bell(&parsed.process_id).await?;
        // From here the replica's cleanup passes keep the head; this covers
        // the time until the first of them.
        cleanup::keep(&self.shared, std::slice::from_ref(&bell.process)).await?;
        let events = match subscription::read(&self.shared, &parsed).await? {
            Read::Gap(reason) => return Ok(ProcessReplaySubscribeOutcome::Gap(reason)),
            Read::Events(events) => events,
        };
        let last = events
            .last()
            .map_or(parsed.live_position, |last| last.live_position());
        let live = subscription::live_tail(
            Arc::clone(&self.shared),
            parsed.process_id.clone(),
            parsed.replay_incarnation_id.to_string(),
            last,
            bell,
        );
        Ok(ProcessReplaySubscribeOutcome::Subscribed(
            ProcessReplaySubscription::new(events, live),
        ))
    }

    async fn earliest_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError> {
        schema::cursor(&self.shared, process_id, sequence).await
    }

    async fn invalidate_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), ProcessReplayStoreError> {
        let doorbell = schema::invalidate(&self.shared, process_id).await?;
        self.shared.ring(&doorbell);
        Ok(())
    }

    async fn invalidate_all(&self) -> Result<(), ProcessReplayStoreError> {
        // A new incarnation is the one invalidation that needs no list of
        // processes: every cursor of the old one gaps.
        schema::rotate(&self.shared.pool, &self.shared.sql, true).await?;
        self.shared.ring(&Doorbell::all());
        Ok(())
    }
}
