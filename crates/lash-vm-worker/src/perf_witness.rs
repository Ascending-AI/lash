//! The existing witness, transported in progress receipts for isolated workers.
#[cfg(feature = "testing")]
use std::cell::Cell;
#[cfg(feature = "testing")]
thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static GRAPH: Cell<[u64; 6]> = const { Cell::new([0; 6]) };
    static PEAK: Cell<u64> = const { Cell::new(0) };
    /// Pages the heap read through the wire and sealed into the parent's
    /// staging since the last receipt.
    static PAGES: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
}

pub(crate) struct Witness {
    #[cfg(feature = "testing")]
    _collector: Option<lash_core_execution::perf_witness::Collector>,
}
#[cfg(feature = "testing")]
pub(crate) fn install(args: &[String]) -> Result<Witness, crate::PoolError> {
    if !args.iter().any(|arg| arg == "--count-heap-work") {
        return Ok(Witness { _collector: None });
    }
    let collector = lash_core_execution::perf_witness::Collector::install()
        .map_err(|_| crate::PoolError::InvalidConfiguration)?;
    lash_kernel_vm::perf_witness::enable_work();
    ACTIVE.set(true);
    Ok(Witness {
        _collector: Some(collector),
    })
}
#[cfg(not(feature = "testing"))]
pub(crate) fn install(args: &[String]) -> Result<Witness, crate::PoolError> {
    if args.iter().any(|arg| arg == "--count-heap-work") {
        return Err(crate::PoolError::InvalidConfiguration);
    }
    Ok(Witness {})
}

pub(crate) fn buffer(bytes: usize) {
    #[cfg(feature = "testing")]
    if ACTIVE.get() {
        PEAK.set(PEAK.get().max(bytes as u64));
    }
    #[cfg(not(feature = "testing"))]
    let _ = bytes;
}

/// `pages` pages crossed from the parent: misses of the worker's clean
/// residency.
pub(crate) fn pages_loaded(pages: u64) {
    #[cfg(feature = "testing")]
    if ACTIVE.get() {
        let [loaded, sealed] = PAGES.get();
        PAGES.set([loaded + pages, sealed]);
    }
    #[cfg(not(feature = "testing"))]
    let _ = pages;
}

/// `pages` pages crossed to the parent's staging.
pub(crate) fn pages_sealed(pages: u64) {
    #[cfg(feature = "testing")]
    if ACTIVE.get() {
        let [loaded, sealed] = PAGES.get();
        PAGES.set([loaded, sealed + pages]);
    }
    #[cfg(not(feature = "testing"))]
    let _ = pages;
}

pub(crate) fn take() -> Option<lash_vm_protocol::WorkerWork> {
    #[cfg(feature = "testing")]
    {
        if !ACTIVE.get() {
            return None;
        }
        let work = lash_kernel_vm::perf_witness::take_work()?;
        let graph = lash_core_execution::perf_witness::graph_work();
        let previous = GRAPH.replace(graph);
        let [pages_loaded, pages_sealed] = PAGES.replace([0; 2]);
        Some(lash_vm_protocol::WorkerWork {
            serialized_body_bytes: graph[4] - previous[4],
            decoded_body_bytes: graph[5] - previous[5],
            vm_steps: work.vm_steps,
            pages_loaded,
            pages_sealed,
            encoded_nodes: graph[0] - previous[0],
            encoded_bytes: graph[1] - previous[1],
            decoded_nodes: graph[2] - previous[2],
            decoded_bytes: graph[3] - previous[3],
            peak_buffer_bytes: PEAK.replace(0),
        })
    }
    #[cfg(not(feature = "testing"))]
    None
}
