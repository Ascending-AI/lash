//! [`PostgresSignals`]: the durability engine's cross-node signals over
//! PostgreSQL (L8, FIG-5178).
//!
//! - **Wakes.** A batch is sent with one `pg_notify` statement on the shared
//!   pool, after the commits that woke its actors and never inside a writing
//!   transaction: S2 (FIG-5167) measured post-commit delivery at
//!   0.41 ms p50 at sixteen listeners, and an in-transaction notify takes
//!   the notification queue's lock on every writer's commit. Each node has
//!   one channel. A ready hint rings the one node picked to claim a readied
//!   unowned actor, with an empty payload; mail for an owned actor rings its
//!   owner's channel with the actor's key in the payload.
//! - **Listener.** Each node has one listener on a connection of its own. It
//!   subscribes to its node's channel, then takes its boot's liveness lock, a
//!   session advisory lock, before [`Signals::listen`] returns. When its
//!   session is lost it reconnects, subscribes and locks again, and only then
//!   reports [`Signal::Resubscribed`], so the runner rescans after it. Its
//!   reconnects back off under the host's `signals.reconnect` policy, and a
//!   store opens at most `roles.served_nodes` listeners at once.
//! - **Liveness.** A boot's lock is free exactly when no session of it
//!   lives. A crashed node's session ends with its process, so a watcher
//!   sees the lock free within one probe and reaps the boot at once. A node
//!   cut off by a network partition keeps its session until the server
//!   notices the dead connection (`tcp_keepalives_*`), and is then reaped
//!   by its lease.
//!
//! `LISTEN` and session locks need a session that outlives one transaction:
//! the store must be reached directly or through a session-mode pooler. A
//! transaction-mode pooler drops both silently.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lash_durable::{
    ActorKey, BootLiveness, CommitCapacity, DurableError, NodeId, NodeLease, Owner, Reaped, Signal,
    SignalFeed, Signals, WakeBatch,
};
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgNotification, PgPoolOptions};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};

use crate::host::ReconnectPolicy;

use super::{PostgresDurableStore, SQL, sqlx_failure};

/// The most bytes one notification's payload carries; PostgreSQL refuses
/// payloads of 8000 bytes or more.
const PAYLOAD_LIMIT: usize = 7_900;

/// A node's own channel: `lash_node_<id>`, or a digest of the id when the id
/// does not fit a channel name.
pub(crate) fn node_channel(node: &NodeId) -> String {
    let id = node.as_str();
    let plain = id.len() <= 53
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if plain {
        format!("lash_node_{id}")
    } else {
        // FNV-1a: stable across builds, so every node names one channel.
        let digest = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        format!("lash_node_{digest:016x}")
    }
}

/// The `(channel, payload)` notifications that send `batch`.
fn notifications(batch: &WakeBatch) -> (Vec<String>, Vec<String>) {
    let mut channels = Vec::new();
    let mut payloads = Vec::new();
    for node in &batch.ready {
        channels.push(node_channel(node));
        payloads.push(String::new());
    }
    for (node, actors) in &batch.owned {
        let channel = node_channel(node);
        let mut payload = String::new();
        for actor in actors {
            if !payload.is_empty() && payload.len() + 1 + actor.as_str().len() > PAYLOAD_LIMIT {
                channels.push(channel.clone());
                payloads.push(std::mem::take(&mut payload));
            }
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(actor.as_str());
        }
        if !payload.is_empty() {
            channels.push(channel);
            payloads.push(payload);
        }
    }
    (channels, payloads)
}

/// What one notification signals, if anything.
fn signal_of(notification: &PgNotification) -> Option<Signal> {
    if notification.payload().is_empty() {
        return Some(Signal::Ready);
    }
    let actors: Vec<ActorKey> = notification
        .payload()
        .split('\n')
        .filter_map(|key| ActorKey::parse(key).ok())
        .collect();
    (!actors.is_empty()).then_some(Signal::Owned(actors))
}

/// The cross-node [`Signals`] over one PostgreSQL catalog.
#[derive(Clone, Debug)]
pub struct PostgresSignals {
    store: PostgresDurableStore,
}

impl PostgresSignals {
    pub(crate) fn new(store: PostgresDurableStore) -> Self {
        Self { store }
    }
}

/// How one boot's listener (re)opens its session.
struct Session {
    /// A pool of one listener-role connection that the listener alone
    /// uses.
    pool: PgPool,
    channel: String,
    boot: String,
    /// How long one open may take, connecting and the lock wait included.
    open_within: Duration,
    reconnect: ReconnectPolicy,
    /// The served-node slot this listener occupies until its session ends.
    _slot: OwnedSemaphorePermit,
}

impl Session {
    /// Subscribe, then hold the boot's liveness lock, within the listener's
    /// startup bound.
    async fn open(&self) -> Result<PgListener, sqlx::Error> {
        let open = async {
            let mut listener = PgListener::connect_with(&self.pool).await?;
            listener.listen(&self.channel).await?;
            sqlx::query(SQL.postgres.hold_liveness.sql())
                .bind(&self.boot)
                .execute(&mut listener)
                .await?;
            Ok(listener)
        };
        tokio::time::timeout(self.open_within, open)
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
    }
}

/// Forward `listener`'s signals until `shutdown`, then end the session.
async fn serve(
    listener: PgListener,
    session: Session,
    signals: mpsc::UnboundedSender<Signal>,
    lost: Arc<AtomicU64>,
    shutdown: oneshot::Receiver<()>,
) {
    forward(listener, &session, &signals, &lost, shutdown).await;
    // The listener's connection is back in the pool: closing the pool ends
    // the session, which releases the lock.
    session.pool.close().await;
}

/// Forward `listener`'s signals until `shutdown`, reopening a lost session.
/// Returns having dropped every listener it held.
async fn forward(
    mut listener: PgListener,
    session: &Session,
    signals: &mpsc::UnboundedSender<Signal>,
    lost: &AtomicU64,
    mut shutdown: oneshot::Receiver<()>,
) {
    'serve: loop {
        let received = tokio::select! {
            biased;
            _ = &mut shutdown => break 'serve,
            received = listener.try_recv() => received,
        };
        if let Ok(Some(notification)) = received {
            if let Some(signal) = signal_of(&notification)
                && signals.send(signal).is_err()
            {
                break 'serve;
            }
            continue;
        }
        // The session is gone, or no longer trustworthy: drop it whole and
        // open another, so the lock and the subscription are taken anew.
        lost.fetch_add(1, Ordering::AcqRel);
        drop(listener);
        let mut failures = 0;
        listener = loop {
            let opened = tokio::select! {
                biased;
                _ = &mut shutdown => break 'serve,
                opened = session.open() => opened,
            };
            match opened {
                Ok(listener) => break listener,
                Err(_) => {
                    let wait = session.reconnect.wait(failures);
                    failures = failures.saturating_add(1);
                    tokio::select! {
                        biased;
                        _ = &mut shutdown => break 'serve,
                        () = tokio::time::sleep(wait) => {}
                    }
                }
            }
        };
        if signals.send(Signal::Resubscribed).is_err() {
            break 'serve;
        }
    }
}

/// A node's listener, as its runner holds it.
struct PostgresFeed {
    signals: mpsc::UnboundedReceiver<Signal>,
    lost: Arc<AtomicU64>,
    shutdown: Option<oneshot::Sender<()>>,
}

#[async_trait::async_trait]
impl SignalFeed for PostgresFeed {
    async fn next(&mut self) -> Signal {
        match self.signals.recv().await {
            Some(signal) => signal,
            None => std::future::pending().await,
        }
    }

    fn session(&self) -> u64 {
        self.lost.load(Ordering::Acquire)
    }
}

impl Drop for PostgresFeed {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[async_trait::async_trait]
impl Signals for PostgresSignals {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        let (channels, payloads) = notifications(batch);
        if channels.is_empty() {
            return Ok(());
        }
        self.store
            .within(CommitCapacity::Work, async {
                sqlx::query(SQL.postgres.notify.sql())
                    .bind(&channels)
                    .bind(&payloads)
                    .execute(&self.store.pools.work)
                    .await
                    .map_err(sqlx_failure)?;
                Ok(())
            })
            .await
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn SignalFeed>, DurableError> {
        let pools = &self.store.pools;
        let open_within = pools.listener_policy.acquire_timeout;
        // A listener that just stopped frees its slot when its session task
        // ends; a replacement waits that moment out, no longer.
        let slot = tokio::time::timeout(open_within, Arc::clone(&pools.listeners).acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| DurableError::NodeCapacityExceeded {
                node: lease.owner.node.clone(),
                served_nodes: pools.served_nodes,
            })?;
        let session = Session {
            pool: PgPoolOptions::new()
                .max_connections(1)
                .min_connections(0)
                .acquire_timeout(open_within)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_lazy_with(pools.listener.clone()),
            channel: node_channel(&lease.owner.node),
            boot: lease.owner.boot.as_str().to_owned(),
            open_within,
            reconnect: pools.reconnect,
            _slot: slot,
        };
        let listener = match session.open().await {
            Ok(listener) => listener,
            Err(error) => {
                session.pool.close().await;
                return Err(sqlx_failure(error));
            }
        };
        let (send, signals) = mpsc::unbounded_channel();
        let (shutdown, stop) = oneshot::channel();
        let lost = Arc::new(AtomicU64::new(0));
        tokio::spawn(serve(listener, session, send, Arc::clone(&lost), stop));
        Ok(Box::new(PostgresFeed {
            signals,
            lost,
            shutdown: Some(shutdown),
        }))
    }

    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError> {
        self.store.liveness().await
    }

    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        self.store.reap_released(reaper, boot).await
    }
}

#[cfg(test)]
#[path = "durable_signals_tests.rs"]
mod tests;
