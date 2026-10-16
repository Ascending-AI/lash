//! Opt-in physical SQLite writes. Only connections opened while a collector
//! is installed use the observing VFS; ordinary writes have no observer hook.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

mod vfs;

/// Maximum retained database × page × kind × operation × origin rows.
pub const ROW_LIMIT: usize = 16_384;
static ENABLED: AtomicBool = AtomicBool::new(false);
static EPOCH: AtomicU64 = AtomicU64::new(1);
static CURRENT: LazyLock<Mutex<Option<Arc<Mutex<State>>>>> = LazyLock::new(|| Mutex::new(None));
thread_local! {
    static CONTEXT: RefCell<(String, String)> = RefCell::new(("sqlite.setup_or_close".into(), "connection lifecycle".into()));
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    WalHeader,
    WalFrameHeader,
    WalBody,
    Checkpoint,
    Database,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WriteKey {
    pub database: String,
    pub page: Option<u32>,
    pub kind: WriteKind,
    /// The owning physical writer, not a causal assignment to a SQL statement.
    pub operation: String,
    pub origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRow {
    #[serde(flatten)]
    pub key: WriteKey,
    pub bytes: u64,
}

/// Completeness is part of the data. A cut during a connection operation,
/// failed write/parse, counter reset, or capacity loss cannot certify traffic.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Receipt {
    pub epoch: u64,
    pub complete: bool,
    pub active_operations: u64,
    pub overflow_bytes: u64,
    pub unknown_bytes: u64,
    pub failed_writes: u64,
    pub write_calls: u64,
    pub total_bytes: u64,
    pub rows: Vec<WriteRow>,
}
impl Receipt {
    fn valid_cut(&self) -> bool {
        self.complete
            && self.active_operations == 0
            && self.overflow_bytes == 0
            && self.unknown_bytes == 0
            && self.failed_writes == 0
            && self.rows.iter().map(|r| r.bytes).sum::<u64>() == self.total_bytes
    }
    /// The interval between two complete cuts in the same observer lifetime.
    pub fn difference(&self, before: &Self) -> Option<Self> {
        if !self.valid_cut() || !before.valid_cut() || self.epoch != before.epoch {
            return None;
        }
        let old: BTreeMap<_, _> = before.rows.iter().map(|r| (&r.key, r.bytes)).collect();
        let mut rows = Vec::new();
        for row in &self.rows {
            let bytes = row
                .bytes
                .checked_sub(old.get(&row.key).copied().unwrap_or(0))?;
            if bytes > 0 {
                rows.push(WriteRow {
                    key: row.key.clone(),
                    bytes,
                });
            }
        }
        let result = Self {
            epoch: self.epoch,
            complete: true,
            write_calls: self.write_calls.checked_sub(before.write_calls)?,
            total_bytes: self.total_bytes.checked_sub(before.total_bytes)?,
            rows,
            ..Self::default()
        };
        (result.rows.iter().map(|r| r.bytes).sum::<u64>() == result.total_bytes).then_some(result)
    }
    pub fn bytes(&self, kind: WriteKind) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.key.kind == kind)
            .map(|r| r.bytes)
            .sum()
    }
    /// Conserves the existing CACHE_WRITE × page_size metric exactly. Framing,
    /// checkpoint and other file writes are additional physical traffic.
    pub fn conserves(&self, store_bytes_written: u64) -> bool {
        self.valid_cut()
            && self.bytes(WriteKind::WalBody) == store_bytes_written
            && self.rows.iter().map(|r| r.bytes).sum::<u64>() == self.total_bytes
    }
}

struct State {
    epoch: u64,
    active: u64,
    rows: BTreeMap<WriteKey, u64>,
    limit: usize,
    overflow: u64,
    unknown: u64,
    failures: u64,
    calls: u64,
    bytes: u64,
}
impl State {
    fn record(&mut self, database: &str, page: Option<u32>, kind: WriteKind, bytes: u64) {
        let (operation, origin) = CONTEXT.with(|slot| slot.borrow().clone());
        let key = WriteKey {
            database: database.into(),
            page,
            kind,
            operation,
            origin,
        };
        if self.rows.contains_key(&key) || self.rows.len() < self.limit {
            *self.rows.entry(key).or_default() += bytes;
        } else {
            self.overflow += bytes;
        }
    }
    fn receipt(&self) -> Receipt {
        Receipt {
            epoch: self.epoch,
            complete: self.active == 0
                && self.overflow == 0
                && self.unknown == 0
                && self.failures == 0,
            active_operations: self.active,
            overflow_bytes: self.overflow,
            unknown_bytes: self.unknown,
            failed_writes: self.failures,
            write_calls: self.calls,
            total_bytes: self.bytes,
            rows: self
                .rows
                .iter()
                .map(|(key, bytes)| WriteRow {
                    key: key.clone(),
                    bytes: *bytes,
                })
                .collect(),
        }
    }
}

/// One process-local collector. Install before opening any measured stores;
/// existing connections keep their VFS and will fail byte conservation.
pub struct Collector {
    state: Arc<Mutex<State>>,
}
impl Collector {
    pub fn install() -> Result<Self, &'static str> {
        Self::install_bounded(ROW_LIMIT)
    }
    fn install_bounded(limit: usize) -> Result<Self, &'static str> {
        let mut current = CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_some() {
            return Err("a SQLite write observer is already installed");
        }
        vfs::register()?;
        let state = Arc::new(Mutex::new(State {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            active: 0,
            rows: BTreeMap::new(),
            limit,
            overflow: 0,
            unknown: 0,
            failures: 0,
            calls: 0,
            bytes: 0,
        }));
        *current = Some(Arc::clone(&state));
        ENABLED.store(true, Ordering::Release);
        Ok(Self { state })
    }
    pub fn snapshot(&self) -> Receipt {
        self.cut(|| ()).1
    }
    /// Read aggregate counters under the physical observer's boundary lock.
    /// Starting an operation and its first write must wait for this cut.
    pub fn cut<T>(&self, counters: impl FnOnce() -> T) -> (T, Receipt) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (counters(), state.receipt())
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Release);
        *CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

pub(crate) fn open(
    path: impl AsRef<std::path::Path>,
    flags: rusqlite::OpenFlags,
) -> rusqlite::Result<rusqlite::Connection> {
    if ENABLED.load(Ordering::Acquire) {
        rusqlite::Connection::open_with_flags_and_vfs(path, flags, vfs::NAME)
    } else {
        rusqlite::Connection::open_with_flags(path, flags)
    }
}

/// Enclose the whole connection operation, including COMMIT and CACHE_WRITE
/// accounting, so a cut cannot silently split physical and aggregate counters.
pub(crate) fn scope<T>(origin: &'static str, work: impl FnOnce() -> T) -> T {
    if !ENABLED.load(Ordering::Acquire) {
        return work();
    }
    let state = CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(state) = state else { return work() };
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active += 1;
    struct Restore(Arc<Mutex<State>>, (String, String));
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTEXT.with(|slot| *slot.borrow_mut() = std::mem::take(&mut self.1));
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active -= 1;
        }
    }
    let operation = lash_core_execution::facade_support::sql::Window::current()
        .map(|w| w.snapshot().operation)
        .unwrap_or_else(|| "sqlite.connection".into());
    let old = CONTEXT.with(|slot| slot.replace((operation, origin.into())));
    let _restore = Restore(state, old);
    work()
}

#[cfg(test)]
mod tests;
