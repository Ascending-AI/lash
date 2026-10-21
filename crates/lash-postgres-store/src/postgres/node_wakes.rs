//! [`PostgresNodeWakes`]: the durability engine's node wakes over
//! PostgreSQL (L8, FIG-5178).
//!
//! - **Wakes.** A batch is sent with one `pg_notify` statement on the shared
//!   pool, after the commits that woke its actors and never inside a writing
//!   transaction: S2 (FIG-5167) measured post-commit delivery at
//!   0.41 ms p50 at sixteen listeners, and an in-transaction notify takes
//!   the notification queue's lock on every writer's commit. Each node has
//!   one channel. A ready hint rings the one node picked to claim a readied
//!   unowned actor; mail for an owned actor rings its owner's channel; an
//!   actor whose event log grew is named on one channel that every node
//!   listens to. All carry the shared, escaped, size-bounded wake envelope.
//! - **Listener.** Each node has one listener on a connection of its own. It
//!   subscribes to its node's channel and the appended-log channel, then takes its boot's liveness lock, a
//!   session advisory lock, before [`NodeWakes::listen`] returns. When its
//!   session is lost it reconnects, subscribes and locks again, and only then
//!   reports [`NodeWakeEvent::Resubscribed`], so the runner rescans after it. Its
//!   reconnects back off under the host's `signals.reconnect` policy, and a
//!   store opens at most `roles.served_nodes` listeners at once.
//! - **Observer.** A process that serves no node opens an observer
//!   ([`NodeWakes::observe`], FIG-7055): a connection of its own that
//!   subscribes to the appended-log channel alone and takes no lock, so no
//!   reaper sees it and it holds no served-node slot. It reconnects as a
//!   listener does.
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
    BootLiveness, CommitCapacity, DurableError, NodeId, NodeLease, NodeWakeEvent, NodeWakeFeed,
    NodeWakes, Owner, Reaped, WakeBatch, node_wake_payload,
};
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};

use crate::host::ReconnectPolicy;

use super::{PostgresDurableStore, SQL, sqlx_failure};

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

/// The channel every node listens to for the actors whose event logs grew.
/// No node's own channel has this name: each starts `lash_node_`.
const APPENDED_CHANNEL: &str = "lash_appended";

/// The `(channel, payload)` notifications that send `batch`.
fn notifications(batch: &WakeBatch) -> (Vec<String>, Vec<String>) {
    let mut channels = Vec::new();
    let mut payloads = Vec::new();
    for node in &batch.ready {
        channels.push(node_channel(node));
        payloads.push(node_wake_payload::READY.to_owned());
    }
    for (node, actors) in &batch.owned {
        let channel = node_channel(node);
        for payload in node_wake_payload::owned(actors) {
            channels.push(channel.clone());
            payloads.push(payload);
        }
    }
    for payload in node_wake_payload::appended(&batch.appended) {
        channels.push(APPENDED_CHANNEL.to_owned());
        payloads.push(payload);
    }
    (channels, payloads)
}

/// The node wakes of one PostgreSQL catalog.
#[derive(Clone, Debug)]
pub struct PostgresNodeWakes {
    store: PostgresDurableStore,
}

impl PostgresNodeWakes {
    pub(crate) fn new(store: PostgresDurableStore) -> Self {
        Self { store }
    }
}

/// How one boot's listener, or an observer, (re)opens its session.
struct Session {
    /// A pool of one listener-role connection that the listener alone
    /// uses.
    pool: PgPool,
    /// The node's channel and boot, for a node's listener; `None` for an
    /// observer, which hears the appended-log channel alone and holds no
    /// lock.
    node: Option<(String, String)>,
    /// How long one open may take, connecting and the lock wait included.
    open_within: Duration,
    reconnect: ReconnectPolicy,
    /// The served-node slot a node's listener occupies until its session
    /// ends.
    _slot: Option<OwnedSemaphorePermit>,
}

impl Session {
    /// Subscribe, then hold the boot's liveness lock, within the listener's
    /// startup bound.
    async fn open(&self) -> Result<PgListener, sqlx::Error> {
        let open = async {
            let mut listener = PgListener::connect_with(&self.pool).await?;
            match &self.node {
                Some((channel, boot)) => {
                    listener
                        .listen_all([channel.as_str(), APPENDED_CHANNEL])
                        .await?;
                    crate::observed_sql::query(SQL.postgres.hold_liveness.sql())
                        .bind(boot)
                        .execute(crate::observed_sql::executor(&mut listener))
                        .await?;
                }
                None => listener.listen(APPENDED_CHANNEL).await?,
            }
            Ok(listener)
        };
        tokio::time::timeout(self.open_within, open)
            .await
            .unwrap_or(Err(sqlx::Error::PoolTimedOut))
    }
}

/// Forward `listener`'s wake events until `shutdown`, then end the session.
async fn serve(
    listener: PgListener,
    session: Session,
    wakes: mpsc::UnboundedSender<NodeWakeEvent>,
    lost: Arc<AtomicU64>,
    shutdown: oneshot::Receiver<()>,
) {
    forward(listener, &session, &wakes, &lost, shutdown).await;
    // The listener's connection is back in the pool: closing the pool ends
    // the session, which releases the lock.
    session.pool.close().await;
}

/// Forward `listener`'s wake events until `shutdown`, reopening a lost
/// session. Returns having dropped every listener it held.
async fn forward(
    mut listener: PgListener,
    session: &Session,
    wakes: &mpsc::UnboundedSender<NodeWakeEvent>,
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
            if let Some(event) = node_wake_payload::decode(notification.payload())
                && wakes.send(event).is_err()
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
        if wakes.send(NodeWakeEvent::Resubscribed).is_err() {
            break 'serve;
        }
    }
}

/// A node's listener, as its runner holds it.
struct PostgresFeed {
    wakes: mpsc::UnboundedReceiver<NodeWakeEvent>,
    lost: Arc<AtomicU64>,
    shutdown: Option<oneshot::Sender<()>>,
}

#[async_trait::async_trait]
impl NodeWakeFeed for PostgresFeed {
    async fn next(&mut self) -> NodeWakeEvent {
        match self.wakes.recv().await {
            Some(event) => event,
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
impl NodeWakes for PostgresNodeWakes {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        let (channels, payloads) = notifications(batch);
        if channels.is_empty() {
            return Ok(());
        }
        self.store
            .within(CommitCapacity::Work, async {
                crate::observed_sql::query(SQL.postgres.notify.sql())
                    .bind(&channels)
                    .bind(&payloads)
                    .execute(crate::observed_sql::executor(&self.store.pools.work))
                    .await
                    .map_err(sqlx_failure)?;
                Ok(())
            })
            .await
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
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
        let session = self.session(
            Some((
                node_channel(&lease.owner.node),
                lease.owner.boot.as_str().to_owned(),
            )),
            Some(slot),
        );
        Self::start(session).await
    }

    async fn observe(&self) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        Self::start(self.session(None, None)).await
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

impl PostgresNodeWakes {
    /// A listener session over a pool of one listener-role connection.
    fn session(
        &self,
        node: Option<(String, String)>,
        slot: Option<OwnedSemaphorePermit>,
    ) -> Session {
        let pools = &self.store.pools;
        let open_within = pools.listener_policy.acquire_timeout;
        Session {
            pool: PgPoolOptions::new()
                .max_connections(1)
                .min_connections(0)
                .acquire_timeout(open_within)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_lazy_with(pools.listener.clone()),
            node,
            open_within,
            reconnect: pools.reconnect,
            _slot: slot,
        }
    }

    /// Open `session` and forward its wake events on a task of its own.
    async fn start(session: Session) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        let listener = match session.open().await {
            Ok(listener) => listener,
            Err(error) => {
                session.pool.close().await;
                return Err(sqlx_failure(error));
            }
        };
        let (send, wakes) = mpsc::unbounded_channel();
        let (shutdown, stop) = oneshot::channel();
        let lost = Arc::new(AtomicU64::new(0));
        tokio::spawn(serve(listener, session, send, Arc::clone(&lost), stop));
        Ok(Box::new(PostgresFeed {
            wakes,
            lost,
            shutdown: Some(shutdown),
        }))
    }
}

#[cfg(test)]
#[path = "node_wakes_tests.rs"]
mod tests;
