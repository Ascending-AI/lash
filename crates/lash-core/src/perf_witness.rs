//! Internal, feature-gated counters for performance witnesses.
//!
//! This is unsupported instrumentation surface. Production builds compile the
//! module and every call site out unless `perf-witness` is explicitly enabled.

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
static POOL_CHECKOUT_WAIT_NANOS: LazyLock<Mutex<Vec<u64>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// One point-in-time view of the runtime work observed by the active witness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub hash_passes: u64,
    pub hashed_bytes: u64,
    pub body_copy_passes: u64,
    pub copied_bytes: u64,
    pub pool_checkout_wait_nanos: Vec<u64>,
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

fn lock_pool_checkout_waits() -> MutexGuard<'static, Vec<u64>> {
    POOL_CHECKOUT_WAIT_NANOS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_records_pool_checkout_wait_samples() {
        let collector = Collector::install().expect("install performance witness");
        record_pool_checkout_wait(Duration::from_nanos(37));

        assert_eq!(collector.snapshot().pool_checkout_wait_nanos, vec![37]);
    }
}
