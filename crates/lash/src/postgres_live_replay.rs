//! A [`LiveReplayStore`] over PostgreSQL, shared by every replica of a host
//! (FIG-5101).
//!
//! The table is the log and notifications are only a doorbell:
//!
//! - **Publish.** A replica gathers its sessions' batches for one tick and
//!   writes them in one transaction that locks each session's head row,
//!   assigns positions under the lock, appends the events, trims the window
//!   and rings one `pg_notify`. The row lock serialises one session's
//!   writers on every replica, so commit order is position order and no
//!   position is reserved and abandoned.
//! - **Subscribe.** One LISTEN connection per replica, on a session of its
//!   own beside the data pool (the host's `live_replay.listener`). A subscriber
//!   registers its session's doorbell, refcounted by the replica's
//!   subscribers, only once the LISTEN is confirmed; then it reads the rows
//!   past its cursor, and each doorbell after that re-reads past the last
//!   position it delivered. A missed doorbell (a listener reconnect) rings
//!   every subscription, which re-reads from its cursor.
//! - **Incarnation.** One logged row names the history the unlogged tables
//!   hold. Crash recovery and failover truncate unlogged tables; a missing
//!   sentinel row rotates the incarnation, and every older cursor gaps.
//! - **Retention.** Count, age and bytes per session, judged by database
//!   time; a periodic jittered pass reclaims expired rows and forgets idle
//!   sessions.
//! - **Schema.** The tables come from the published
//!   `postgres-live-replay-schema.sql`, which the store executes in its
//!   `install` schema mode and a host applies itself for the `verify_only`
//!   mode (FIG-5220). Every connect checks them against the generated shape
//!   artifact and refuses on drift.
//!
//! [`current_cursor`](LiveReplayStore::current_cursor) is synchronous, so
//! each replica keeps a mirror of every session's head, merged from its own
//! publications and every doorbell; what it has not learned only makes it
//! answer earlier.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use lash_core::{
    LiveReplayEventDraft, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, LiveReplaySubscription, SessionCursor, SessionObservationEvent,
    SessionRevision,
};
use lash_postgres_store::host::{
    ConnectionRole, PostgresHostConfig, ReconnectPolicy, ReplayDataPolicy, ReplaySchemaMode,
    RetryPolicy,
};
use lash_postgres_store::{
    PostgresConnectionFactory, PostgresEndpoints, PostgresHostConfigError, TransactionPrelude,
};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

mod cleanup;
mod codec;
mod heads;
mod listener;
mod mirror;
mod publisher;
mod schema;
mod schema_shape;
mod subscription;

pub use schema_shape::{PostgresLiveReplaySchemaFinding, PostgresLiveReplaySchemaReport};

use codec::Doorbell;
use mirror::Mirror;
use schema::{Incarnation, Statements};
use subscription::Read;

/// Why a [`PostgresLiveReplayStore`] could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostgresLiveReplayError {
    /// A configuration value is outside its range, or the configuration
    /// has no `live_replay` section.
    #[error(transparent)]
    Config(#[from] PostgresHostConfigError),
    /// The database refused the connection, the tables or the listener.
    #[error(transparent)]
    Database(#[from] LiveReplayStoreError),
    /// The store's tables differ from the published artifact; nothing was
    /// repaired.
    #[error("the postgres live replay tables refuse: {0}")]
    SchemaDrift(PostgresLiveReplaySchemaReport),
}

/// The live replay store every replica of a host shares through one
/// PostgreSQL database. Plug it in with
/// [`LashCoreBuilder::live_replay_store`](crate::LashCoreBuilder::live_replay_store).
pub struct PostgresLiveReplayStore {
    shared: Arc<Shared>,
    publisher: mpsc::UnboundedSender<publisher::PublishRequest>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for PostgresLiveReplayStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresLiveReplayStore")
            .field("schema", &self.shared.config.schema)
            .finish_non_exhaustive()
    }
}

/// What the store's tasks and subscriptions share.
struct Shared {
    /// Publication, reads, cleanup and reloads.
    pool: sqlx::PgPool,
    /// The listener's own session.
    listener_pool: sqlx::PgPool,
    config: ReplayDataPolicy,
    /// How long a subscribe or a connect waits for the listener.
    listener_wait: std::time::Duration,
    reconnect: ReconnectPolicy,
    retry: RetryPolicy,
    /// The replay guard profile each publication and cleanup transaction
    /// begins with.
    prelude: TransactionPrelude,
    sql: Statements,
    mirror: StdMutex<Mirror>,
    /// `Some(epoch)` while the listener's LISTEN is confirmed.
    listening: watch::Sender<Option<u64>>,
    bells: StdMutex<HashMap<SessionId, Bell>>,
}

/// A session's doorbell on this replica, refcounted by its subscribers.
struct Bell {
    sender: watch::Sender<u64>,
    subscribers: usize,
}

/// One subscriber's hold on its session's doorbell.
struct BellGuard {
    shared: Arc<Shared>,
    session_id: SessionId,
    receiver: watch::Receiver<u64>,
}

impl Drop for BellGuard {
    fn drop(&mut self) {
        let mut bells = self.shared.bells.lock_recover();
        if let Some(bell) = bells.get_mut(&self.session_id) {
            bell.subscribers -= 1;
            if bell.subscribers == 0 {
                bells.remove(&self.session_id);
            }
        }
    }
}

impl Shared {
    fn adopt(&self, incarnation: Incarnation) {
        self.mirror.lock_recover().adopt(incarnation);
    }

    fn observed(&self, session_id: &SessionId, floor: u64, first_live: u64) {
        self.mirror
            .lock_recover()
            .observed(session_id, floor, first_live);
    }

    fn reload_mirror(
        &self,
        incarnation: Incarnation,
        heads: Vec<(SessionId, u64, u64, u64)>,
        runs: Vec<(SessionId, u64, u64, u64)>,
    ) {
        self.mirror.lock_recover().reload(incarnation, heads, runs);
    }

    fn ring_mirror(&self, doorbells: &[Doorbell]) {
        let mut mirror = self.mirror.lock_recover();
        for doorbell in doorbells {
            mirror.ring(doorbell);
        }
    }

    /// Wake this replica's subscribers of every session `doorbells` name;
    /// a rotation wakes them all.
    fn ring_sessions(&self, doorbells: &[Doorbell]) {
        if doorbells
            .iter()
            .any(|doorbell| matches!(doorbell, Doorbell::Rotated { .. }))
        {
            self.ring_all();
            return;
        }
        let bells = self.bells.lock_recover();
        if bells.is_empty() {
            return;
        }
        let ring = |session: &str| {
            if let Ok(session_id) = SessionId::parse(session)
                && let Some(bell) = bells.get(&session_id)
            {
                bell.sender.send_modify(|rings| *rings += 1);
            }
        };
        for doorbell in doorbells {
            match doorbell {
                Doorbell::Published { session, .. }
                | Doorbell::Invalidated { session, .. }
                | Doorbell::Trimmed { session, .. } => ring(session),
                Doorbell::Forgotten { sessions, .. } => {
                    for session in sessions {
                        ring(session);
                    }
                }
                Doorbell::Rotated { .. } => {}
            }
        }
    }

    /// Wake every subscriber on this replica: each re-reads from its
    /// cursor.
    fn ring_all(&self) {
        for bell in self.bells.lock_recover().values() {
            bell.sender.send_modify(|rings| *rings += 1);
        }
    }

    /// Register a subscriber's doorbell, once the listener's LISTEN is
    /// confirmed: every doorbell rung after this reaches it.
    async fn bell(
        self: &Arc<Self>,
        session_id: &SessionId,
    ) -> Result<BellGuard, LiveReplayStoreError> {
        let mut listening = self.listening.subscribe();
        tokio::time::timeout(self.listener_wait, listening.wait_for(Option::is_some))
            .await
            .map_err(|_| {
                LiveReplayStoreError::Store(
                    "postgres live replay listener is not connected".to_string(),
                )
            })?
            .map_err(|_| LiveReplayStoreError::Closed)?;
        let mut bells = self.bells.lock_recover();
        let bell = bells.entry(session_id.clone()).or_insert_with(|| Bell {
            sender: watch::channel(0).0,
            subscribers: 0,
        });
        bell.subscribers += 1;
        let mut receiver = bell.sender.subscribe();
        receiver.mark_unchanged();
        Ok(BellGuard {
            shared: Arc::clone(self),
            session_id: session_id.clone(),
            receiver,
        })
    }
}

impl PostgresLiveReplayStore {
    /// Connect through `endpoints` under `config.live_replay`, create the
    /// store's tables when absent (in the `install` schema mode), refuse
    /// when they differ from the published artifact, confirm the listener,
    /// and start the replica's publisher and cleanup. The configuration is
    /// validated before anything connects; one without a `live_replay`
    /// section is refused.
    pub async fn connect(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
    ) -> Result<Self, PostgresLiveReplayError> {
        config.validate()?;
        let policy = config
            .live_replay
            .clone()
            .ok_or_else(|| PostgresHostConfigError {
                field: "live_replay".to_owned(),
                reason: "is required to open a live replay store".to_owned(),
            })?;
        let factory = PostgresConnectionFactory::new(endpoints.clone(), config.connection.clone());
        let pool = factory.pool(
            ConnectionRole::Replay,
            &policy.pool,
            Some(&config.guards.replay),
        );
        let listener_pool =
            factory.dedicated(ConnectionRole::ReplayListener, &policy.listener, 1, None);
        let report = match policy.data.schema_mode {
            ReplaySchemaMode::Install => schema::install(&pool, &policy.data.schema).await?,
            ReplaySchemaMode::VerifyOnly => schema::verify(&pool, &policy.data.schema).await?,
        };
        if !report.is_conformant() {
            return Err(PostgresLiveReplayError::SchemaDrift(report));
        }
        let sql = Statements::new(&policy.data.schema);
        let incarnation = schema::ensure_incarnation(&pool, &sql).await?;
        let shared = Arc::new(Shared {
            pool,
            listener_pool,
            sql,
            mirror: StdMutex::new(Mirror::new(incarnation)),
            listening: watch::channel(None).0,
            bells: StdMutex::new(HashMap::new()),
            listener_wait: policy.listener.acquire_timeout,
            reconnect: policy.reconnect,
            retry: config.retry.live_replay,
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
            shared,
        };
        // The mirror is loaded once the listener confirms, so a cursor
        // handed out after `connect` knows every head.
        let mut listening = store.shared.listening.subscribe();
        tokio::time::timeout(
            store.shared.listener_wait,
            listening.wait_for(Option::is_some),
        )
        .await
        .map_err(|_| {
            LiveReplayStoreError::Store("postgres live replay listener did not connect".into())
        })?
        .map_err(|_| LiveReplayStoreError::Closed)?;
        Ok(store)
    }

    /// The DDL that provisions the store's tables, committed verbatim as
    /// `crates/lash/postgres-live-replay-schema.sql`.
    ///
    /// The `install` schema mode executes these bytes. A host that owns its
    /// migrations vendors them rather than transcribing them, applies them
    /// into the configured `schema` (they are schema-unqualified and
    /// provision into whichever schema `search_path` resolves), and runs the
    /// store in the `verify_only` mode, the default. Every statement is creation-only and
    /// idempotent.
    pub fn schema_ddl() -> &'static str {
        schema_shape::SCHEMA_DDL
    }

    /// The structure [`schema_ddl`](Self::schema_ddl) produces and every
    /// connect checks, committed as the generated
    /// `crates/lash/postgres-live-replay-schema-shape.txt`.
    pub fn schema_shape() -> &'static str {
        schema_shape::SHAPE_ARTIFACT
    }

    /// The check every connect runs, without connecting a store: a host's
    /// migration CI gates on it. It runs no DDL and never fails on drift;
    /// read [`PostgresLiveReplaySchemaReport::is_conformant`] and render the
    /// report for its findings.
    pub async fn verify_schema(
        pool: &sqlx::PgPool,
        schema: &str,
    ) -> Result<PostgresLiveReplaySchemaReport, LiveReplayStoreError> {
        schema::verify(pool, schema).await
    }

    /// The data policy this store runs with.
    pub fn config(&self) -> &ReplayDataPolicy {
        &self.shared.config
    }
}

impl Drop for PostgresLiveReplayStore {
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
impl LiveReplayStore for PostgresLiveReplayStore {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        if events.is_empty() {
            return Err(LiveReplayStoreError::Store(
                "cannot publish an empty live replay batch".to_string(),
            ));
        }
        let drafts = events
            .into_iter()
            .map(|draft| codec::encode(session_id, draft))
            .collect::<Result<Vec<_>, _>>()?;
        let (reply, answer) = oneshot::channel();
        self.publisher
            .send(publisher::PublishRequest {
                session_id: session_id.clone(),
                revision,
                drafts,
                reply,
            })
            .map_err(|_| LiveReplayStoreError::Closed)?;
        answer.await.map_err(|_| LiveReplayStoreError::Closed)?
    }

    async fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        let parsed = cursor.parse()?;
        Ok(match subscription::read(&self.shared, &parsed).await? {
            Read::Gap(reason) => LiveReplayOutcome::Gap(reason),
            Read::Events(events) => LiveReplayOutcome::Replayed(events),
        })
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        let parsed = cursor.parse()?;
        let bell = self.shared.bell(&parsed.session_id).await?;
        let events = match subscription::read(&self.shared, &parsed).await? {
            Read::Gap(reason) => return Ok(LiveReplaySubscribeOutcome::Gap(reason)),
            Read::Events(events) => events,
        };
        let last = events
            .last()
            .and_then(|event| event.cursor.parse().ok())
            .map_or(parsed.live_position, |last| last.live_position);
        let live = subscription::live_tail(
            Arc::clone(&self.shared),
            parsed.session_id.clone(),
            parsed.replay_incarnation_id.to_string(),
            last,
            bell,
        );
        Ok(LiveReplaySubscribeOutcome::Subscribed(
            LiveReplaySubscription::new(events, live),
        ))
    }

    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        self.shared
            .mirror
            .lock_recover()
            .current_cursor(session_id, revision)
    }

    fn earliest_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        self.shared
            .mirror
            .lock_recover()
            .earliest_cursor(session_id, revision)
    }

    async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        let doorbells = schema::invalidate(&self.shared.pool, &self.shared.sql, session_id).await?;
        self.shared.ring_mirror(&doorbells);
        self.shared.ring_sessions(&doorbells);
        Ok(())
    }

    async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        cleanup::trim(&self.shared, session_id).await
    }
}
