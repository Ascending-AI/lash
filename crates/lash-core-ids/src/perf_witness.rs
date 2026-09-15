//! Internal, feature-gated counters for performance witnesses.
//!
//! This is unsupported instrumentation surface. Production builds compile the
//! module and every call site out unless `perf-witness` is explicitly enabled.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

const INACTIVE: u8 = 0;
const INSTALLING: u8 = 1;
const ACTIVE: u8 = 2;

static COLLECTOR_STATE: AtomicU8 = AtomicU8::new(INACTIVE);
static HASH_PASSES: AtomicU64 = AtomicU64::new(0);
static HASHED_BYTES: AtomicU64 = AtomicU64::new(0);
static BODY_COPY_PASSES: AtomicU64 = AtomicU64::new(0);
static COPIED_BYTES: AtomicU64 = AtomicU64::new(0);
// Pool-wait sample serialization is acceptable here because this recorder is
// compiled in only for the explicitly enabled performance-witness feature.
static POOL_CHECKOUT_WAIT_NANOS: LazyLock<Mutex<Vec<u64>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static SQL_STATEMENTS: AtomicU64 = AtomicU64::new(0);
static SQL_STATEMENTS_BY_VERB: [AtomicU64; SqlVerb::COUNT] =
    [const { AtomicU64::new(0) }; SqlVerb::COUNT];

/// The statement shapes the SQL witness separates. Anything the classifier does
/// not recognise lands in [`SqlVerb::Other`] rather than being dropped, so the
/// per-verb counts always sum to the total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqlVerb {
    Select,
    Insert,
    Update,
    Delete,
    Begin,
    Commit,
    Rollback,
    Savepoint,
    Pragma,
    Other,
}

impl SqlVerb {
    const COUNT: usize = 10;

    const ALL: [Self; Self::COUNT] = [
        Self::Select,
        Self::Insert,
        Self::Update,
        Self::Delete,
        Self::Begin,
        Self::Commit,
        Self::Rollback,
        Self::Savepoint,
        Self::Pragma,
        Self::Other,
    ];

    /// The stable counter-name suffix for this verb.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Begin => "begin",
            Self::Commit => "commit",
            Self::Rollback => "rollback",
            Self::Savepoint => "savepoint",
            Self::Pragma => "pragma",
            Self::Other => "other",
        }
    }

    /// Classify a statement by its leading keyword, ignoring leading
    /// whitespace, `--` line comments and `/* */` block comments.
    pub fn classify(sql: &str) -> Self {
        let mut rest = sql;
        loop {
            rest = rest.trim_start();
            if let Some(tail) = rest.strip_prefix("--") {
                rest = tail.find('\n').map_or("", |end| &tail[end + 1..]);
                continue;
            }
            if let Some(tail) = rest.strip_prefix("/*") {
                rest = tail.find("*/").map_or("", |end| &tail[end + 2..]);
                continue;
            }
            break;
        }
        let keyword = rest
            .split(|character: char| !character.is_ascii_alphabetic())
            .next()
            .unwrap_or_default();
        match keyword.to_ascii_lowercase().as_str() {
            "select" | "with" => Self::Select,
            "insert" | "replace" => Self::Insert,
            "update" => Self::Update,
            "delete" => Self::Delete,
            "begin" => Self::Begin,
            "commit" | "end" => Self::Commit,
            "rollback" => Self::Rollback,
            "savepoint" | "release" => Self::Savepoint,
            "pragma" => Self::Pragma,
            _ => Self::Other,
        }
    }
}

/// One point-in-time view of the runtime work observed by the active witness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub hash_passes: u64,
    pub hashed_bytes: u64,
    pub body_copy_passes: u64,
    pub copied_bytes: u64,
    pub pool_checkout_wait_nanos: Vec<u64>,
    /// Total SQL statements the instrumented backends executed.
    ///
    /// Only the count is recorded, not the time: SQLite's profile clock is
    /// quantised to whole milliseconds, so summing its per-statement durations
    /// rounds every sub-millisecond statement to zero and reports a number that
    /// looks like engine time but is not.
    pub sql_statements: u64,
    /// The same total split by leading keyword; the values sum to
    /// `sql_statements`.
    pub sql_statements_by_verb: BTreeMap<&'static str, u64>,
}

/// Exclusive process-global runtime-work witness.
pub struct Collector {
    _private: (),
}

/// Another process-global collector is already active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlreadyInstalled;

impl std::fmt::Display for AlreadyInstalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a process-global performance witness is already installed")
    }
}

impl std::error::Error for AlreadyInstalled {}

impl Collector {
    /// Reset and install the one process-global collector.
    pub fn install() -> Result<Self, AlreadyInstalled> {
        COLLECTOR_STATE
            .compare_exchange(INACTIVE, INSTALLING, Ordering::AcqRel, Ordering::Relaxed)
            .map_err(|_| AlreadyInstalled)?;
        HASH_PASSES.store(0, Ordering::Relaxed);
        HASHED_BYTES.store(0, Ordering::Relaxed);
        BODY_COPY_PASSES.store(0, Ordering::Relaxed);
        COPIED_BYTES.store(0, Ordering::Relaxed);
        SQL_STATEMENTS.store(0, Ordering::Relaxed);
        for counter in &SQL_STATEMENTS_BY_VERB {
            counter.store(0, Ordering::Relaxed);
        }
        lock_pool_checkout_waits().clear();
        COLLECTOR_STATE.store(ACTIVE, Ordering::Release);
        Ok(Self { _private: () })
    }

    /// Snapshot all work observed since this collector was installed.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            hash_passes: HASH_PASSES.load(Ordering::Relaxed),
            hashed_bytes: HASHED_BYTES.load(Ordering::Relaxed),
            body_copy_passes: BODY_COPY_PASSES.load(Ordering::Relaxed),
            copied_bytes: COPIED_BYTES.load(Ordering::Relaxed),
            pool_checkout_wait_nanos: lock_pool_checkout_waits().clone(),
            sql_statements: SQL_STATEMENTS.load(Ordering::Relaxed),
            sql_statements_by_verb: SqlVerb::ALL
                .iter()
                .map(|verb| {
                    (
                        verb.name(),
                        SQL_STATEMENTS_BY_VERB[*verb as usize].load(Ordering::Relaxed),
                    )
                })
                .collect(),
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        COLLECTOR_STATE.store(INACTIVE, Ordering::Release);
    }
}

/// Record one SHA-256 pass over a checkpoint body.
#[inline]
pub fn record_hash_pass(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) != ACTIVE {
        return;
    }
    HASH_PASSES.fetch_add(1, Ordering::Relaxed);
    HASHED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Record one explicit checkpoint-body clone or copy.
#[inline]
pub fn record_body_copy(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) != ACTIVE {
        return;
    }
    BODY_COPY_PASSES.fetch_add(1, Ordering::Relaxed);
    COPIED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Record one wait for a pooled persistence connection.
#[inline]
pub fn record_pool_checkout_wait(elapsed: Duration) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) != ACTIVE {
        return;
    }
    lock_pool_checkout_waits().push(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
}

/// Record one SQL statement executed by an instrumented store backend.
#[inline]
pub fn record_sql_statement(sql: &str) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) != ACTIVE {
        return;
    }
    SQL_STATEMENTS.fetch_add(1, Ordering::Relaxed);
    SQL_STATEMENTS_BY_VERB[SqlVerb::classify(sql) as usize].fetch_add(1, Ordering::Relaxed);
}

fn lock_pool_checkout_waits() -> MutexGuard<'static, Vec<u64>> {
    POOL_CHECKOUT_WAIT_NANOS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collector is process-global and exclusive, so the tests that install
    /// it cannot run concurrently with each other.
    static INSTALL_GUARD: Mutex<()> = Mutex::new(());

    fn install_guard() -> MutexGuard<'static, ()> {
        INSTALL_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn collector_records_pool_checkout_wait_samples() {
        let _guard = install_guard();
        let collector = Collector::install().expect("install performance witness");
        record_pool_checkout_wait(Duration::from_nanos(37));

        assert_eq!(collector.snapshot().pool_checkout_wait_nanos, vec![37]);
    }

    #[test]
    fn collector_splits_sql_statements_by_verb_and_sums_to_the_total() {
        let _guard = install_guard();
        let collector = Collector::install().expect("install performance witness");
        record_sql_statement("BEGIN IMMEDIATE");
        record_sql_statement("  -- pick the head\n SELECT 1");
        record_sql_statement("/* blob */ INSERT INTO t VALUES (1)");
        record_sql_statement("VACUUM");

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.sql_statements, 4);
        assert_eq!(
            snapshot.sql_statements_by_verb.values().sum::<u64>(),
            snapshot.sql_statements
        );
        assert_eq!(snapshot.sql_statements_by_verb["begin"], 1);
        assert_eq!(snapshot.sql_statements_by_verb["select"], 1);
        assert_eq!(snapshot.sql_statements_by_verb["insert"], 1);
        assert_eq!(snapshot.sql_statements_by_verb["other"], 1);
    }
}
