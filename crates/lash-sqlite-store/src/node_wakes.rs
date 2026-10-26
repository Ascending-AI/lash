//! [`SqliteNodeWakes`]: the durability engine's node wakes over a SQLite
//! database file, which several processes on one machine serve as separate
//! nodes (FIG-5422).
//!
//! - **Wakes.** Each producer writes its hints in the transaction that wakes
//!   its actors. In-memory capture belongs to each member; its hints are
//!   written inside that member's savepoint, so refused work leaves no hint.
//!   A group answers only after its shared durable commit. A post-commit flush signals listeners without writing.
//!   Standalone transport publications write their own batch: a row for
//!   each node picked to claim a readied unowned actor, with no actors, and a
//!   row for each owner node with the keys of its actors that took mail, and
//!   a row for every node, naming none, with the keys of the actors whose
//!   event logs grew. The
//!   bounded liveness-maintenance pass deletes expired hints, at most 4,096
//!   per pass. Producers retain hints without doing expiry work: an old
//!   hint is disposable because the polls also find its work.
//! - **Listener.** Each node's listener runs on a thread of its own with a
//!   read-only connection of its own, and asks it for `PRAGMA data_version`
//!   every the configured wake poll. The version moves exactly when another connection
//!   commits, so a quiet database costs one pragma and one file check per
//!   poll. When it moves, the listener reads the wake rows past its cursor
//!   and forwards its own node's and every node's. Once a commit that wrote
//!   rows is durable, it rings the listeners they name
//!   ([`doorbell`]): this process's at once, every other process's through
//!   the datagram endpoint its listener bound for the database, so
//!   a wake between nodes on one machine waits for no poll (FIG-7044). The
//!   poll stays the fallback for a ring that never came.
//! - **Observer.** A process that serves no node opens an observer
//!   ([`NodeWakes::observe`], FIG-7055): a listener of its own that reads
//!   only the rows every node hears, binds a doorbell endpoint of its own so
//!   a ring to every node reaches it, and holds no lock: no reaper sees it
//!   and it fences nothing.
//! - **Liveness.** The listener holds its boot's liveness lock, a file lock
//!   beside the database ([`crate::liveness_locks`]), for as long as it
//!   lives. The kernel drops it when the listener's process dies, however it
//!   dies, so a watcher sees it free within one probe and reaps the boot at
//!   once. A boot that is merely slow still holds it, and is reaped by its
//!   lease.
//! - **Session.** The lock and the connection are the listener's session.
//!   When its lock file is deleted under it or its connection fails, it
//!   takes both again, reads a fresh cursor, and only then reports
//!   [`NodeWakeEvent::Resubscribed`], so the runner rescans after it.
//!
//! A memory store set has no node wakes: it lives in one process, and its
//! nodes find each other's work through the claim poll and the mail scan.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use lash_durable::{
    BootId, BootLiveness, DurableError, NodeLease, NodeWakeEvent, NodeWakeFeed, NodeWakes, Owner,
    Reaped, StoreFailure, StoreFailureKind, WakeBatch, node_wake_payload,
};
use tokio::sync::{mpsc, oneshot};

use super::{SqliteDurableStore, store_failure};
use crate::conn::cached_execute;
use crate::liveness_locks::{HeldLock, LivenessLocks};
use crate::location::DatabaseTarget;

/// The wake rows: one hint each, consumed by the listener of the node it
/// names. Not engine state: no fence reads it and nothing is lost with it.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS node_wakes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id TEXT,
    actors TEXT NOT NULL,
    published_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_node_wakes_published_at ON node_wakes(published_at_ms);
";

/// Append one hint for node `?1`, or for every node with none: the shared
/// wake envelope `?2`.
const INSERT: &str =
    "INSERT INTO node_wakes (node_id, actors, published_at_ms) VALUES (?1, ?2, ?3)";

/// Inspect and delete at most 4,096 hints per maintenance pass. The default
/// two-second cadence can retire 2,048 hints/s, above the measured plain
/// fixture's three hints/send at 146 sends/s. Recheck expiry on each row:
/// a regressed clock may delay cleanup but cannot expire a young hint.
/// The indexed expiry probe makes LIMIT zero when nothing expired, before
/// SQLite opens the candidate scan. A nonempty pass keeps the same oldest
/// 4096 candidates, including young hints, and the same per-row recheck.
const PRUNE: &str = "DELETE FROM node_wakes WHERE seq IN (
    SELECT seq FROM node_wakes ORDER BY seq LIMIT
        CASE WHEN EXISTS (SELECT 1 FROM node_wakes WHERE published_at_ms < ?1)
        THEN 4096 ELSE 0 END) AND published_at_ms < ?1";

/// The last hint published so far: where a new listener's cursor starts.
const TAIL: &str = "SELECT COALESCE(MAX(seq), 0) FROM node_wakes";

/// Node `?2`'s hints and every node's past cursor `?1`, in publish order.
const PAST: &str = "SELECT seq, actors FROM node_wakes
    WHERE seq > ?1 AND (node_id = ?2 OR node_id IS NULL) ORDER BY seq";

/// Every node's hints past cursor `?1`, in publish order: what an observer
/// hears.
const PAST_EVERY_NODE: &str = "SELECT seq, actors FROM node_wakes
    WHERE seq > ?1 AND node_id IS NULL ORDER BY seq";

/// Numbers this process's observers' doorbell endpoints.
static OBSERVERS: AtomicU64 = AtomicU64::new(0);

/// The rings of this process's listeners, one per database, so a publish
/// here wakes them without a poll. Entries are weak: a database nobody
/// listens to or publishes on is forgotten.
static RINGS: LazyLock<Mutex<HashMap<String, Weak<Ring>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn ring(target: &DatabaseTarget) -> Arc<Ring> {
    let mut rings = RINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = target.canonical_name();
    if let Some(ring) = rings.get(&key).and_then(Weak::upgrade) {
        return ring;
    }
    let ring = Arc::new(Ring::default());
    rings.retain(|_, ring| ring.strong_count() > 0);
    rings.insert(key, Arc::downgrade(&ring));
    ring
}

/// The name of boot `boot`'s liveness lock: `boot-<id>`, or a digest of the
/// id when the id does not fit a file name.
pub(crate) fn boot_lock(boot: &BootId) -> String {
    let id = boot.as_str();
    let plain = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if plain {
        format!("boot-{id}")
    } else {
        // FNV-1a: stable across builds, so every process names one file.
        let digest = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        format!("boot-{digest:016x}")
    }
}

/// A liveness lock that could not be probed or taken.
pub(crate) fn lock_failure(error: &std::io::Error) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: format!("a node liveness lock could not be read: {error}"),
    })
}

/// The `(node, envelope)` rows that send `batch`; a row with no node is
/// every node's.
fn rows(batch: &WakeBatch) -> Vec<(Option<String>, String)> {
    let ready = batch.ready.iter().map(|node| {
        (
            Some(node.as_str().to_owned()),
            node_wake_payload::READY.to_owned(),
        )
    });
    let owned = batch.owned.iter().flat_map(|(node, actors)| {
        node_wake_payload::owned(actors)
            .into_iter()
            .map(move |payload| (Some(node.as_str().to_owned()), payload))
    });
    let appended = node_wake_payload::appended(&batch.appended)
        .into_iter()
        .map(|payload| (None, payload));
    ready.chain(owned).chain(appended).collect()
}

/// The node wakes of one SQLite database file.
#[derive(Clone)]
pub(crate) struct SqliteNodeWakes {
    store: SqliteDurableStore,
    target: DatabaseTarget,
    locks: LivenessLocks,
    ring: Arc<Ring>,
    doorbell: doorbell::Doorbell,
}

impl std::fmt::Debug for SqliteNodeWakes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteNodeWakes")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl SqliteNodeWakes {
    /// The node wakes of the database file at `path`, whose durable store is
    /// `store`.
    pub(crate) async fn new(
        store: SqliteDurableStore,
        path: &std::path::Path,
    ) -> rusqlite::Result<Self> {
        let target = DatabaseTarget::File(path.to_path_buf());
        let locks = LivenessLocks::beside(path, store.conn.operational.liveness_lock_attempts);
        let ring = ring(&target);
        let doorbell = doorbell::Doorbell::new(
            locks.dir().to_path_buf(),
            &target.canonical_name(),
            Arc::clone(&ring),
        );
        store
            .conn
            .install_node_wakes(NodeWakeWriter::new(&store, doorbell.clone()))
            .await?;
        Ok(Self {
            locks,
            ring,
            doorbell,
            store,
            target,
        })
    }
}

/// One listener session: its boot's lock (an observer holds none) and its
/// connection, and where its reads have reached.
struct Session {
    lock: Option<HeldLock>,
    connection: rusqlite::Connection,
    /// The data version the connection last reported.
    version: i64,
    /// The last hint row read.
    cursor: i64,
}

/// What a listener's sessions are opened from.
struct Opener {
    target: DatabaseTarget,
    locks: LivenessLocks,
    /// The node's boot lock and the node, for a node's listener; `None` for
    /// an observer.
    node: Option<(String, String)>,
    policy: crate::SqliteOperationalSettings,
}

fn unavailable(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message,
    })
}

impl Opener {
    /// Take the boot's lock, waiting up to the configured lock budget for it to come
    /// free, then open the connection and read its version and cursor, in
    /// that order: a commit after the version read moves the version again,
    /// so the first poll reads past a cursor that may already include it.
    /// Gives up when `stop` is set.
    fn open(&self, stop: &AtomicBool) -> Result<Session, DurableError> {
        let lock = match &self.node {
            Some((lock, _)) => Some(self.hold(lock, stop)?),
            None => None,
        };
        let connection = rusqlite::Connection::open_with_flags(
            self.target.read_only_uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .and_then(|connection| {
            connection.busy_timeout(self.policy.readonly_busy_timeout)?;
            connection.pragma_update(None, "cache_size", self.policy.readonly_cache_size)?;
            connection.set_prepared_statement_cache_capacity(self.policy.statement_cache_capacity);
            Ok(connection)
        })
        .map_err(store_failure)?;
        let version = data_version(&connection).map_err(store_failure)?;
        let cursor = connection
            .query_row(TAIL, [], |row| row.get::<_, i64>(0))
            .map_err(store_failure)?;
        Ok(Session {
            lock,
            connection,
            version,
            cursor,
        })
    }
}

impl Opener {
    fn hold(&self, lock: &str, stop: &AtomicBool) -> Result<HeldLock, DurableError> {
        let deadline = Instant::now() + self.policy.wake_lock_timeout;
        loop {
            match self.locks.try_hold(lock) {
                Ok(Some(held)) => return Ok(held),
                Ok(None) if Instant::now() < deadline && !stop.load(Ordering::Acquire) => {
                    std::thread::sleep(self.policy.lock_poll);
                }
                Ok(None) => {
                    return Err(unavailable(format!(
                        "the liveness lock `{lock}` stayed held for {:?}",
                        self.policy.wake_lock_timeout
                    )));
                }
                Err(error) => return Err(lock_failure(&error)),
            }
        }
    }
}

fn data_version(connection: &rusqlite::Connection) -> rusqlite::Result<i64> {
    connection.query_row("PRAGMA data_version", [], |row| row.get(0))
}

impl Session {
    /// Node `node`'s hints committed since the last poll, or with none an
    /// observer's: every node's. `None` when the session is lost: its lock
    /// file was deleted, or its connection failed.
    fn poll(&mut self, node: Option<&str>) -> Option<Vec<NodeWakeEvent>> {
        if self.lock.as_ref().is_some_and(|lock| !lock.intact()) {
            return None;
        }
        let version = data_version(&self.connection).ok()?;
        if version == self.version {
            return Some(Vec::new());
        }
        self.version = version;
        let row = |row: &rusqlite::Row<'_>| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?));
        let rows = match node {
            Some(node) => self
                .connection
                .prepare_cached(PAST)
                .and_then(|mut statement| {
                    statement
                        .query_map(rusqlite::params![self.cursor, node], row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                }),
            None => self
                .connection
                .prepare_cached(PAST_EVERY_NODE)
                .and_then(|mut statement| {
                    statement
                        .query_map([self.cursor], row)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                }),
        }
        .ok()?;
        let mut events = Vec::with_capacity(rows.len());
        for (seq, actors) in rows {
            self.cursor = self.cursor.max(seq);
            events.extend(node_wake_payload::decode(&actors));
        }
        Some(events)
    }

    /// End the session: give the lock up and delete its file, and close the
    /// connection.
    fn close(self) {
        if let Some(lock) = self.lock {
            lock.release();
        }
    }
}

/// The rings of one database's listeners in this process, counted: a
/// listener waits on the count as well as on its poll, so a ring wakes it at
/// once.
#[derive(Default)]
pub(crate) struct Ring {
    rung: Mutex<u64>,
    bell: Condvar,
}

impl Ring {
    fn ring(&self) {
        *self
            .rung
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.bell.notify_all();
    }

    fn rung(&self) -> u64 {
        *self
            .rung
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait until the count moves past `seen`, `within` passes or `stop` is
    /// set; answers the count.
    fn wait(&self, seen: u64, within: Duration, stop: &AtomicBool) -> u64 {
        let rung = self
            .rung
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (rung, _) = self
            .bell
            .wait_timeout_while(rung, within, |rung| {
                *rung == seen && !stop.load(Ordering::Acquire)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *rung
    }

    /// Sleep `wait`, waking early only for `stop`.
    fn pause(&self, wait: Duration, stop: &AtomicBool) {
        let until = Instant::now() + wait;
        let mut seen = self.rung();
        while !stop.load(Ordering::Acquire) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            seen = self.wait(seen, left, stop);
        }
    }
}

/// A listener's thread: open its first session and say how that went
/// through `opened`, then forward its node's wake events until `stop`,
/// reopening a lost session, and end the session. It polls on a thread of
/// its own, so an idle poll costs one pragma and one file check and wakes no
/// async runtime.
fn listen_on(
    opener: &Opener,
    ring: &Ring,
    wakes: &mpsc::UnboundedSender<NodeWakeEvent>,
    lost: &AtomicU64,
    stop: &AtomicBool,
    opened: oneshot::Sender<Result<(), DurableError>>,
) {
    let mut session = match opener.open(stop) {
        Ok(session) => session,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    if opened.send(Ok(())).is_err() {
        // The listen that asked for it is gone.
        session.close();
        return;
    }
    let mut seen = ring.rung();
    'serve: loop {
        seen = ring.wait(seen, opener.policy.wake_poll, stop);
        if stop.load(Ordering::Acquire) {
            break 'serve;
        }
        if let Some(events) = session.poll(opener.node.as_ref().map(|(_, node)| node.as_str())) {
            for event in events {
                if wakes.send(event).is_err() {
                    break 'serve;
                }
            }
            continue;
        }
        // The session is gone, or no longer trustworthy: drop it whole and
        // open another, so the lock and the cursor are taken anew.
        lost.fetch_add(1, Ordering::AcqRel);
        drop(session);
        let mut wait = opener.policy.wake_reopen_initial;
        session = loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match opener.open(stop) {
                Ok(reopened) => break reopened,
                Err(_) => {
                    ring.pause(wait, stop);
                    wait = wait.saturating_mul(2).min(opener.policy.wake_reopen_max);
                }
            }
        };
        if wakes.send(NodeWakeEvent::Resubscribed).is_err() {
            break 'serve;
        }
    }
    session.close();
}

/// A node's listener, as its runner holds it.
struct SqliteFeed {
    wakes: mpsc::UnboundedReceiver<NodeWakeEvent>,
    lost: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    ring: Arc<Ring>,
    /// The node's doorbell endpoint, while the listener lives.
    _answering: Option<doorbell::Answering>,
}

#[async_trait::async_trait]
impl NodeWakeFeed for SqliteFeed {
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

impl Drop for SqliteFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wakes this listener's thread, and costs the others one poll.
        self.ring.ring();
    }
}

#[async_trait::async_trait]
impl NodeWakes for SqliteNodeWakes {
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError> {
        let rows = rows(batch);
        if rows.is_empty() {
            return Ok(());
        }
        let mut rung = doorbell::Rung::default();
        for (node, _) in &rows {
            rung.add(node.as_deref());
        }
        let now = self.store.instant()?;
        self.store
            .conn
            .write(move |tx| {
                for (node, actors) in &rows {
                    cached_execute(tx, INSERT, rusqlite::params![node, actors, now.0])?;
                }
                Ok(())
            })
            .await
            .map_err(store_failure)?;
        self.doorbell.ring(&rung);
        Ok(())
    }

    /// Every hint the runner hands here was captured as rows by the commit
    /// that woke its actors, which rang their listeners once it committed.
    async fn committed(&self, _batch: &WakeBatch) -> Result<(), DurableError> {
        Ok(())
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        let node = lease.owner.node.as_str();
        let feed = self
            .open_listener(Some((boot_lock(&lease.owner.boot), node.to_owned())), node)
            .await?;
        // Boots that died or were never released left their lock files; a
        // free lock is the same answer as no file.
        let _ = self.locks.sweep("boot-");
        Ok(feed)
    }

    async fn observe(&self) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        let endpoint = format!(
            "observer-{}-{}",
            std::process::id(),
            OBSERVERS.fetch_add(1, Ordering::Relaxed)
        );
        self.open_listener(None, &endpoint).await
    }

    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError> {
        // The runner's bounded maintenance cadence owns expiry, including
        // periods with no producer. A failed sweep leaves only hints behind.
        let oldest = self.store.instant()?.0.saturating_sub(
            i64::try_from(self.store.conn.operational.wake_retention.as_millis())
                .unwrap_or(i64::MAX),
        );
        self.store
            .observer
            .observe_sql("node_wakes.prune", "sqlite", async {
                self.store
                    .conn
                    .write_flow(move |tx| {
                        let expired = cached_execute(tx, PRUNE, [oldest])?;
                        Ok(if expired == 0 {
                            crate::conn::TxOutcome::Rollback(())
                        } else {
                            crate::conn::TxOutcome::Commit(())
                        })
                    })
                    .await
                    .map_err(store_failure)
            })
            .await?;
        self.store.liveness(self.locks.clone()).await
    }

    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        self.store
            .reap_released(self.locks.clone(), reaper, boot)
            .await
    }
}

impl SqliteNodeWakes {
    /// Start a listener on a thread of its own, answering the doorbell
    /// endpoint `endpoint`: node `node`'s, holding its boot's lock, or with
    /// none an observer's. Returns once its first session is open.
    async fn open_listener(
        &self,
        node: Option<(String, String)>,
        endpoint: &str,
    ) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        let opener = Opener {
            target: self.target.clone(),
            locks: self.locks.clone(),
            node,
            policy: self.store.conn.operational,
        };
        let (send, wakes) = mpsc::unbounded_channel();
        let (opened, open) = oneshot::channel();
        let lost = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let feed = SqliteFeed {
            wakes,
            lost: Arc::clone(&lost),
            stop: Arc::clone(&stop),
            ring: Arc::clone(&self.ring),
            _answering: self.doorbell.answer(endpoint),
        };
        let ring = Arc::clone(&self.ring);
        std::thread::Builder::new()
            .name("lash-sqlite-wakes".into())
            .spawn(move || listen_on(&opener, &ring, &send, &lost, &stop, opened))
            .map_err(|error| {
                unavailable(format!("a node's wake listener could not start: {error}"))
            })?;
        // Dropped before the thread answers (the runner's startup bound
        // passed), the feed stops the thread, which then ends its session.
        open.await
            .unwrap_or_else(|_| Err(unavailable("a node's wake listener ended".to_owned())))?;
        Ok(Box::new(feed))
    }
}

#[cfg(test)]
impl SqliteNodeWakes {
    /// Delete `boot`'s lock file under its listener, as an operator or a
    /// broken cleanup would: the listener's session is lost.
    pub(crate) fn sever_for_testing(&self, boot: &BootId) {
        self.locks.delete_for_testing(&boot_lock(boot));
    }
}

#[cfg(test)]
#[path = "node_wakes_tests.rs"]
mod tests;

#[path = "node_wakes_commit.rs"]
mod commit;
#[path = "node_wakes_doorbell.rs"]
mod doorbell;
pub(crate) use commit::{Capture, NodeWakeWriter};
