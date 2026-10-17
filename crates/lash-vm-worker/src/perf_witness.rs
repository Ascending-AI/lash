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
            pages_loaded,
            pages_sealed,
            encoded_nodes: graph[0] - previous[0],
            encoded_bytes: graph[1] - previous[1],
            decoded_nodes: graph[2] - previous[2],
            decoded_bytes: graph[3] - previous[3],
            peak_buffer_bytes: PEAK.replace(0),
            ..kernel_work(work)
        })
    }
    #[cfg(not(feature = "testing"))]
    None
}

#[cfg(feature = "testing")]
fn kernel_work(work: lash_kernel_vm::perf_witness::Work) -> lash_vm_protocol::WorkerWork {
    lash_vm_protocol::WorkerWork {
        vm_steps: work.vm_steps,
        root_publishes: work.root_publishes,
        task_resolutions: work.task_resolutions,
        frame_resolutions: work.frame_resolutions,
        register_slots_initialized: work.register_slots_initialized,
        register_slots_cleared: work.register_slots_cleared,
        tree_leaf_clones: work.heap.tree_leaf_clones,
        tree_index_clones: work.heap.tree_index_clones,
        tree_rows_cloned: work.heap.tree_rows_cloned,
        accounting_certificate_reads: work.heap.accounting_certificate_reads,
        accounting_additions: work.heap.accounting_additions,
        accounting_changes: work.heap.accounting_changes,
        accounting_reservations: work.heap.accounting_reservations,
        refinement_attempts: work.heap.refinement_attempts,
        completed_refinements: work.heap.completed_refinements,
        completed_trace_work: work.heap.completed_trace_work,
        trace_bitmap_peak_bytes: (work.heap.completed_refinements > 0)
            .then_some(work.heap.trace_bitmap_peak_bytes),
        trace_work_list_peak_bytes: (work.heap.completed_refinements > 0)
            .then_some(work.heap.trace_work_list_peak_bytes),
        trace_budget_peak_bytes: (work.heap.completed_refinements > 0)
            .then_some(work.heap.trace_budget_peak_bytes),

        ..Default::default()
    }
}

#[cfg(all(test, feature = "testing"))]
mod laws {
    /// FIG-6622: the worker projection used to discard root publications.
    #[test]
    fn root_publications_survive_worker_projection_and_accumulation() {
        let receipt = super::kernel_work(lash_kernel_vm::perf_witness::Work {
            vm_steps: 19,
            root_publishes: 7,
            heap: lash_kernel_state::perf_witness::Work {
                tree_rows_cloned: 11,
                completed_refinements: 1,
                trace_budget_peak_bytes: 23,
                ..Default::default()
            },
            ..Default::default()
        });
        use lash_vm_protocol::{
            DecodeLimits, ExecutionLease, FrameCodec, FrameEpoch, MessageFence, OwnerEpoch,
            WorkerFrame, WorkerMessage, WorkerPhase,
        };
        let frame = WorkerFrame {
            header: MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1))
                .next_header(),
            message: WorkerMessage::Progress {
                phase: WorkerPhase::Computing,
                cpu_nanos: 1,
                work: Some(receipt),
            },
        };
        let codec = FrameCodec::new(DecodeLimits::standard());
        let encoded = codec.encode_worker(&frame).expect("progress encodes");
        let decoded = codec.decode_worker(&encoded).expect("progress decodes");
        assert_eq!(decoded, frame);
        let WorkerMessage::Progress {
            work: Some(receipt),
            ..
        } = decoded.message
        else {
            panic!("progress lost its work receipt");
        };
        let mut total = lash_vm_protocol::WorkerWork::default();
        total.accumulate(receipt);
        let idle = super::kernel_work(lash_kernel_vm::perf_witness::Work::default());
        assert_eq!(idle.trace_budget_peak_bytes, None);
        total.accumulate(idle);
        total.accumulate(receipt);
        assert_eq!((total.vm_steps, total.root_publishes), (38, 14));
        assert_eq!(
            (total.tree_rows_cloned, total.trace_budget_peak_bytes),
            (22, Some(23))
        );
    }
}
