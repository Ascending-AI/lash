//! Feature-gated codec work counters, coordinated by the runtime collector.
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
const ACTIVE: u8 = 2;
pub static COLLECTOR_STATE: AtomicU8 = AtomicU8::new(0);
pub static GRAPH_SERIALIZED_BODY_BYTES: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_DECODED_BODY_BYTES: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_DECODED_NODES: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_DECODED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_ENVELOPES: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_ENCODED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_ADOPT_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
pub static GRAPH_ADOPT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static HASH_PASSES: AtomicU64 = AtomicU64::new(0);
pub static HASHED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Reset codec counters while the runtime collector is installing.
pub fn reset() {
    GRAPH_SERIALIZED_BODY_BYTES.store(0, Ordering::Relaxed);
    GRAPH_DECODED_BODY_BYTES.store(0, Ordering::Relaxed);
    GRAPH_DECODED_NODES.store(0, Ordering::Relaxed);
    GRAPH_DECODED_BYTES.store(0, Ordering::Relaxed);
    GRAPH_ENVELOPES.store(0, Ordering::Relaxed);
    GRAPH_ENCODED_BYTES.store(0, Ordering::Relaxed);
    GRAPH_ADOPT_ATTEMPTS.store(0, Ordering::Relaxed);
    GRAPH_ADOPT_BYTES.store(0, Ordering::Relaxed);
    HASH_PASSES.store(0, Ordering::Relaxed);
    HASHED_BYTES.store(0, Ordering::Relaxed);
}

/// Record one complete graph envelope produced, before staging deduplication.
#[inline]
pub fn record_graph_encode(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) == ACTIVE {
        GRAPH_ENVELOPES.fetch_add(1, Ordering::Relaxed);
        GRAPH_ENCODED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Record one received envelope submitted to adoption, including refused and duplicate nodes.
#[inline]
pub fn record_graph_adopt(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) == ACTIVE {
        GRAPH_ADOPT_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        GRAPH_ADOPT_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Record one digest pass over a checkpoint body or graph envelope.
/// Legacy bodies use SHA-256; graph envelopes use the domain-separated BLAKE3 hash.
#[inline]
pub fn record_hash_pass(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) != ACTIVE {
        return;
    }
    HASH_PASSES.fetch_add(1, Ordering::Relaxed);
    HASHED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Every envelope read by a graph decoder, including repeated reads.
#[inline]
pub fn record_graph_decode(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) == ACTIVE {
        GRAPH_DECODED_NODES.fetch_add(1, Ordering::Relaxed);
        GRAPH_DECODED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Allocation-free scalar view for an isolated worker's progress receipt.
pub fn graph_work() -> [u64; 6] {
    [
        GRAPH_ENVELOPES.load(Ordering::Relaxed),
        GRAPH_ENCODED_BYTES.load(Ordering::Relaxed),
        GRAPH_DECODED_NODES.load(Ordering::Relaxed),
        GRAPH_DECODED_BYTES.load(Ordering::Relaxed),
        GRAPH_SERIALIZED_BODY_BYTES.load(Ordering::Relaxed),
        GRAPH_DECODED_BODY_BYTES.load(Ordering::Relaxed),
    ]
}

/// Logical tokens actually serialized, including before an encoding-cache hit.
#[inline]
pub fn record_graph_serialized_body(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) == ACTIVE {
        GRAPH_SERIALIZED_BODY_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Logical tokens supplied to a typed decoder after materialization.
#[inline]
pub fn record_graph_decoded_body(bytes: usize) {
    if COLLECTOR_STATE.load(Ordering::Relaxed) == ACTIVE {
        GRAPH_DECODED_BODY_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}
