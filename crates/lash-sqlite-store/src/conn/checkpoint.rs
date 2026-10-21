//! The file database's WAL checkpoint worker, driven by write volume
//! (FIG-6907).
//!
//! Every writer connection on a file database reports, after each commit,
//! how many frames the WAL holds (SQLite's WAL hook, which takes the place
//! of SQLite's own autocheckpoint, so no commit checkpoints on the writer
//! thread). Once the WAL holds the policy's threshold, the one worker per
//! file wakes and checkpoints on its one connection, opened at its first
//! checkpoint and kept: first `PASSIVE`, which copies committed frames into
//! the database while writers and readers go on, and then, while the WAL
//! still holds the threshold, `RESTART` with the in-process gates held.
//! The next write reuses the checkpointed WAL allocation. If that allocation
//! exceeds the host's retention bound, `TRUNCATE` releases it instead. A
//! reset that a busy writer or another process's reader blocked is retried
//! each checkpoint interval without another
//! write; nothing else wakes the worker.
//!
//! Checkpointing moves only committed frames and changes no commit's
//! durability: under `synchronous=FULL` a commit answers after its own WAL
//! sync, and a checkpoint syncs the WAL before it copies frames and the
//! database before it resets the WAL.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_int;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, LazyLock, Mutex, RwLock, Weak};
use std::time::Duration;

use crate::location::DatabaseTarget;
use crate::observed_sql::open_connection;

/// One checkpoint worker per file database, shared by all of its writer
/// connections. The worker holds the state only while it checkpoints, so
/// closing the last writer connection ends the worker and closes its
/// connection.
static CHECKPOINTS: LazyLock<Mutex<HashMap<String, Weak<CheckpointState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    /// The checkpoint worker this connection thread's commits report to.
    static REPORTS_TO: RefCell<Option<Arc<CheckpointState>>> = const { RefCell::new(None) };
}

pub(super) struct CheckpointState {
    path: PathBuf,
    write_gate: Arc<Mutex<()>>,
    read_gate: Arc<RwLock<()>>,
    threshold_pages: std::sync::atomic::AtomicU64,
    retained_bytes: std::sync::atomic::AtomicU64,
    wal_path: PathBuf,
    busy_timeout: Mutex<Duration>,
    /// The worker's connection, opened at its first checkpoint.
    connection: Mutex<Option<rusqlite::Connection>>,
    signal: Arc<Signal>,
    /// Checkpoints the worker ran, for the volume law.
    #[cfg(test)]
    runs: std::sync::atomic::AtomicUsize,
}

impl Drop for CheckpointState {
    fn drop(&mut self) {
        self.signal.update(|wake| wake.closed = true);
    }
}

/// What wakes the worker; it waits on this without holding the state.
struct Signal {
    wake: Mutex<Wake>,
    ready: Condvar,
    /// The pause between retries of a blocked WAL reset.
    interval: Mutex<Duration>,
}

#[derive(Default)]
struct Wake {
    /// A commit left the WAL at or past the threshold.
    due: bool,
    /// The last writer connection closed.
    closed: bool,
}

impl Signal {
    fn update(&self, f: impl FnOnce(&mut Wake)) {
        f(&mut self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        self.ready.notify_one();
    }

    /// Wait until a checkpoint is due, or until a retry is, and take it;
    /// `None` once the last writer connection closed.
    fn next(&self, retry: bool) -> Option<()> {
        let interval = *self
            .interval
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut wake = self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if retry {
            wake = self
                .ready
                .wait_timeout_while(wake, interval, |wake| !wake.due && !wake.closed)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        } else {
            wake = self
                .ready
                .wait_while(wake, |wake| !wake.due && !wake.closed)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if wake.closed {
            return None;
        }
        wake.due = false;
        Some(())
    }
}

impl CheckpointState {
    /// A commit left `frames` frames in the WAL.
    fn committed(&self, frames: u64) {
        if frames
            >= self
                .threshold_pages
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.signal.update(|wake| wake.due = true);
        }
    }

    /// Checkpoint once; whether a blocked WAL reset should be retried
    /// without another commit.
    fn checkpoint(&self) -> rusqlite::Result<bool> {
        #[cfg(test)]
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let checkpointed = self.checkpoint_on(&mut connection);
        if checkpointed.is_err() {
            // Reopen at the next checkpoint rather than reuse a connection
            // in an unknown state.
            *connection = None;
        }
        checkpointed
    }

    fn checkpoint_on(
        &self,
        connection: &mut Option<rusqlite::Connection>,
    ) -> rusqlite::Result<bool> {
        let connection = match connection {
            Some(connection) => connection,
            None => {
                let opened = open_connection(
                    &self.path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                opened.busy_timeout(
                    *self
                        .busy_timeout
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )?;
                connection.insert(opened)
            }
        };
        // PASSIVE takes no lock a writer or reader waits on.
        let frames: i64 = connection
            .prepare_cached("PRAGMA wal_checkpoint(PASSIVE)")?
            .query_row([], |row| row.get(1))?;
        if frames
            < self
                .threshold_pages
                .load(std::sync::atomic::Ordering::Relaxed) as i64
        {
            return Ok(false);
        }
        // Stop new in-process readers and let their current statements
        // finish. Writers also take this shared lock before the write gate,
        // so obtaining it exclusively makes the write gate idle without a
        // lock cycle.
        let _read_gate = self
            .read_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(_write_gate) = self.write_gate.try_lock() else {
            return Ok(true);
        };
        // SQLite documents TRUNCATE as RESTART plus physical truncation.
        // Both fully checkpoint before allowing the next writer to reset.
        // Reuse bounded allocation instead of repeatedly freeing and growing
        // the same WAL extents. Metadata failure chooses the reclaiming mode.
        let retained = self
            .retained_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let within_bound = retained > 0
            && self
                .wal_path
                .metadata()
                .is_ok_and(|metadata| metadata.len() <= retained);
        let reset = if within_bound {
            "PRAGMA wal_checkpoint(RESTART)"
        } else {
            "PRAGMA wal_checkpoint(TRUNCATE)"
        };
        let busy: i64 = connection
            .prepare_cached(reset)?
            .query_row([], |row| row.get(0))?;
        Ok(busy != 0)
    }
}

/// `target`'s checkpoint worker, started on first use, or `None` for a
/// memory database or a zero threshold.
pub(super) fn checkpoint_state(
    target: &DatabaseTarget,
    write_gate: &Arc<Mutex<()>>,
    read_gate: &Arc<RwLock<()>>,
    threshold_pages: u32,
    policy: crate::SqliteOperationalSettings,
) -> Option<Arc<CheckpointState>> {
    let path = target.file_path()?;
    if threshold_pages == 0 {
        return None;
    }
    let mut states = CHECKPOINTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = target.canonical_name();
    if let Some(state) = states.get(&key).and_then(Weak::upgrade) {
        state.retained_bytes.fetch_min(
            policy.wal_retained_bytes,
            std::sync::atomic::Ordering::Relaxed,
        );
        state.threshold_pages.fetch_min(
            u64::from(threshold_pages),
            std::sync::atomic::Ordering::Relaxed,
        );
        let mut busy_timeout = state
            .busy_timeout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *busy_timeout = (*busy_timeout).min(policy.checkpoint_busy_timeout);
        drop(busy_timeout);
        let mut interval = state
            .signal
            .interval
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *interval = (*interval).min(policy.checkpoint_interval);
        drop(interval);
        return Some(state);
    }
    let signal = Arc::new(Signal {
        wake: Mutex::new(Wake::default()),
        ready: Condvar::new(),
        interval: Mutex::new(policy.checkpoint_interval),
    });
    let mut wal_path = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let state = Arc::new(CheckpointState {
        path: path.to_owned(),
        wal_path: wal_path.into(),
        retained_bytes: std::sync::atomic::AtomicU64::new(policy.wal_retained_bytes),
        write_gate: Arc::clone(write_gate),
        read_gate: Arc::clone(read_gate),
        threshold_pages: std::sync::atomic::AtomicU64::new(u64::from(threshold_pages)),
        busy_timeout: Mutex::new(policy.checkpoint_busy_timeout),
        connection: Mutex::new(None),
        #[cfg(test)]
        runs: std::sync::atomic::AtomicUsize::new(0),
        signal: Arc::clone(&signal),
    });
    let weak = Arc::downgrade(&state);
    match std::thread::Builder::new()
        .name("lash-sqlite-checkpoint".to_string())
        .spawn(move || checkpoint_loop(&signal, &weak))
    {
        Ok(_) => {
            states.insert(key, Arc::downgrade(&state));
            Some(state)
        }
        Err(error) => {
            tracing::error!(%error, "could not start SQLite checkpoint worker");
            None
        }
    }
}

fn checkpoint_loop(signal: &Signal, state: &Weak<CheckpointState>) {
    let mut retry = false;
    while signal.next(retry).is_some() {
        let Some(state) = state.upgrade() else {
            return;
        };
        let ran = crate::observed_sql::physical_scope("sqlite.background_checkpoint", || {
            state.checkpoint()
        });
        retry = ran.unwrap_or_else(|error| {
            tracing::warn!(%error, path = %state.path.display(), "SQLite checkpoint retry");
            true
        });
    }
}

/// Report this connection's commits to `state` in place of SQLite's own
/// autocheckpoint. Runs on the connection's thread, after its pragmas.
pub(super) fn report_commits(connection: &rusqlite::Connection, state: &Arc<CheckpointState>) {
    REPORTS_TO.with(|reports| *reports.borrow_mut() = Some(Arc::clone(state)));
    connection.wal_hook(Some(committed));
}

fn committed(_: &rusqlite::hooks::Wal, frames: c_int) -> rusqlite::Result<()> {
    REPORTS_TO.with(|reports| {
        if let Some(state) = reports.borrow().as_ref() {
            state.committed(u64::try_from(frames).unwrap_or_default());
        }
    });
    Ok(())
}

/// `target`'s checkpoint worker while a writer connection keeps it.
#[cfg(test)]
pub(super) fn state_for_testing(target: &DatabaseTarget) -> Option<Arc<CheckpointState>> {
    CHECKPOINTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&target.canonical_name())
        .and_then(Weak::upgrade)
}

#[cfg(test)]
impl CheckpointState {
    pub(super) fn threshold_for_testing(&self) -> u64 {
        self.threshold_pages
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
