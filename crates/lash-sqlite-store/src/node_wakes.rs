//! [`SqliteNodeWakes`]: the durability engine's node wakes over a SQLite
//! database file, which several processes on one machine serve as separate
//! nodes (FIG-5422).
//!
//! - **Wakes.** A batch is one write transaction on the store's connection,
//!   after the commits that woke its actors and never inside one: a row for
//!   each node picked to claim a readied unowned actor, with no actors, and a
//!   row for each owner node with the keys of its actors that took mail. The
//!   same transaction deletes the rows older than [`RETENTION`]: a hint that
//!   old is worth nothing, since the polls have found its work.
//! - **Listener.** Each node's listener runs on a thread of its own with a
//!   read-only connection of its own, and asks it for `PRAGMA data_version`
//!   every [`POLL`]. The version moves exactly when another connection
//!   commits, so a quiet database costs one pragma and one file check per
//!   poll. When it moves, the listener reads the wake rows past its cursor
//!   and forwards its own node's. A publish in this process also rings this
//!   process's listeners of the database at once, so a wake between two
//!   nodes of one process waits for no poll.
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
    ActorKey, BootId, BootLiveness, DurableError, NodeLease, NodeWakeEvent, NodeWakeFeed,
    NodeWakes, Owner, Reaped, StoreFailure, StoreFailureKind, WakeBatch,
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
    node_id TEXT NOT NULL,
    actors TEXT NOT NULL,
    published_at_ms INTEGER NOT NULL
);
";

/// Append one hint for node `?1`: the newline-separated keys `?2` of its
/// owned actors that took mail, or none for a ready hint.
const INSERT: &str =
    "INSERT INTO node_wakes (node_id, actors, published_at_ms) VALUES (?1, ?2, ?3)";

/// Delete every hint published before `?1`. Rows are numbered in commit
/// order, so the scan stops at the first young row.
const PRUNE: &str = "DELETE FROM node_wakes WHERE seq < COALESCE(
    (SELECT seq FROM node_wakes WHERE published_at_ms >= ?1 ORDER BY seq LIMIT 1),
    (SELECT COALESCE(MAX(seq), 0) + 1 FROM node_wakes))";

/// The last hint published so far: where a new listener's cursor starts.
const TAIL: &str = "SELECT COALESCE(MAX(seq), 0) FROM node_wakes";

/// Node `?2`'s hints past cursor `?1`, in publish order.
const PAST: &str =
    "SELECT seq, actors FROM node_wakes WHERE seq > ?1 AND node_id = ?2 ORDER BY seq";

/// How often a listener asks whether another connection committed.
pub(crate) const POLL: Duration = Duration::from_millis(25);

/// How long a hint is kept for its listener.
const RETENTION: Duration = Duration::from_secs(10);

/// How long a listener waits for its boot's lock to come free: an earlier
/// session of the same boot ending, or a probe letting it go.
const HOLD_WITHIN: Duration = Duration::from_secs(5);

/// How long a listener whose session failed waits before it opens another,
/// first and at most.
const REOPEN_FIRST: Duration = Duration::from_millis(25);
const REOPEN_MOST: Duration = Duration::from_secs(1);

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

/// The `(node, actors)` rows that send `batch`.
fn rows(batch: &WakeBatch) -> Vec<(String, String)> {
    let ready = batch
        .ready
        .iter()
        .map(|node| (node.as_str().to_owned(), String::new()));
    let owned = batch
        .owned
        .iter()
        .filter(|(_, actors)| !actors.is_empty())
        .map(|(node, actors)| {
            let keys: Vec<&str> = actors.iter().map(ActorKey::as_str).collect();
            (node.as_str().to_owned(), keys.join("\n"))
        });
    ready.chain(owned).collect()
}

/// The wake event one row carries, if anything.
fn node_wake_of(actors: &str) -> Option<NodeWakeEvent> {
    if actors.is_empty() {
        return Some(NodeWakeEvent::Ready);
    }
    let actors: Vec<ActorKey> = actors
        .split('\n')
        .filter_map(|key| ActorKey::parse(key).ok())
        .collect();
    (!actors.is_empty()).then_some(NodeWakeEvent::Owned(actors))
}

/// The node wakes of one SQLite database file.
#[derive(Clone)]
pub(crate) struct SqliteNodeWakes {
    store: SqliteDurableStore,
    target: DatabaseTarget,
    locks: LivenessLocks,
    ring: Arc<Ring>,
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
    pub(crate) fn new(store: SqliteDurableStore, path: &std::path::Path) -> Self {
        let target = DatabaseTarget::File(path.to_path_buf());
        Self {
            store,
            locks: LivenessLocks::beside(path),
            ring: ring(&target),
            target,
        }
    }
}

/// One listener session: its boot's lock and its connection, and where its
/// reads have reached.
struct Session {
    lock: HeldLock,
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
    lock: String,
    node: String,
}

fn unavailable(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message,
    })
}

impl Opener {
    /// Take the boot's lock, waiting up to [`HOLD_WITHIN`] for it to come
    /// free, then open the connection and read its version and cursor, in
    /// that order: a commit after the version read moves the version again,
    /// so the first poll reads past a cursor that may already include it.
    /// Gives up when `stop` is set.
    fn open(&self, stop: &AtomicBool) -> Result<Session, DurableError> {
        let deadline = Instant::now() + HOLD_WITHIN;
        let lock = loop {
            match self.locks.try_hold(&self.lock) {
                Ok(Some(lock)) => break lock,
                Ok(None) if Instant::now() < deadline && !stop.load(Ordering::Acquire) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    return Err(unavailable(format!(
                        "the liveness lock `{}` stayed held for {HOLD_WITHIN:?}",
                        self.lock
                    )));
                }
                Err(error) => return Err(lock_failure(&error)),
            }
        };
        let connection = rusqlite::Connection::open_with_flags(
            self.target.read_only_uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .and_then(|connection| {
            connection.busy_timeout(crate::connection_sql::READ_ONLY_BUSY_TIMEOUT)?;
            connection.execute_batch(crate::connection_sql::READ_ONLY_PRAGMAS)?;
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

fn data_version(connection: &rusqlite::Connection) -> rusqlite::Result<i64> {
    connection.query_row("PRAGMA data_version", [], |row| row.get(0))
}

impl Session {
    /// This node's hints committed since the last poll. `None` when the
    /// session is lost: its lock file was deleted, or its connection failed.
    fn poll(&mut self, node: &str) -> Option<Vec<NodeWakeEvent>> {
        if !self.lock.intact() {
            return None;
        }
        let version = data_version(&self.connection).ok()?;
        if version == self.version {
            return Some(Vec::new());
        }
        self.version = version;
        let rows = self
            .connection
            .prepare_cached(PAST)
            .and_then(|mut statement| {
                statement
                    .query_map(rusqlite::params![self.cursor, node], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .ok()?;
        let mut events = Vec::with_capacity(rows.len());
        for (seq, actors) in rows {
            self.cursor = self.cursor.max(seq);
            events.extend(node_wake_of(&actors));
        }
        Some(events)
    }

    /// End the session: give the lock up and delete its file, and close the
    /// connection.
    fn close(self) {
        self.lock.release();
    }
}

/// This process's publishes on one database, counted: a listener waits on
/// the count as well as on its poll, so a publish here wakes it at once.
#[derive(Default)]
struct Ring {
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
        seen = ring.wait(seen, POLL, stop);
        if stop.load(Ordering::Acquire) {
            break 'serve;
        }
        if let Some(events) = session.poll(&opener.node) {
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
        let mut wait = REOPEN_FIRST;
        session = loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match opener.open(stop) {
                Ok(reopened) => break reopened,
                Err(_) => {
                    ring.pause(wait, stop);
                    wait = (wait * 2).min(REOPEN_MOST);
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
        let now = self.store.instant()?;
        let oldest = now
            .0
            .saturating_sub(i64::try_from(RETENTION.as_millis()).unwrap_or(i64::MAX));
        self.store
            .conn
            .write(move |tx| {
                for (node, actors) in &rows {
                    cached_execute(tx, INSERT, rusqlite::params![node, actors, now.0])?;
                }
                cached_execute(tx, PRUNE, [oldest])?;
                Ok(())
            })
            .await
            .map_err(store_failure)?;
        self.ring.ring();
        Ok(())
    }

    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError> {
        let opener = Opener {
            target: self.target.clone(),
            locks: self.locks.clone(),
            lock: boot_lock(&lease.owner.boot),
            node: lease.owner.node.as_str().to_owned(),
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
        // Boots that died or were never released left their lock files; a
        // free lock is the same answer as no file.
        let _ = self.locks.sweep("boot-");
        Ok(Box::new(feed))
    }

    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError> {
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
