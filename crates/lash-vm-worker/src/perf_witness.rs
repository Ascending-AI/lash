//! The existing witness, transported in progress receipts for isolated workers.
#[cfg(feature = "testing")]
use std::cell::Cell;
#[cfg(feature = "testing")]
thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static GRAPH: Cell<[u64; 6]> = const { Cell::new([0; 6]) };
    static PEAK: Cell<u64> = const { Cell::new(0) };
}

pub(crate) struct Witness {
    #[cfg(feature = "testing")]
    _collector: Option<lash_core_execution::perf_witness::Collector>,
}
#[cfg(feature = "testing")]
pub(crate) fn install(args: &[String]) -> Result<Witness, crate::PoolError> {
    use lash_kernel_vm::perf_witness::{Probe, enable_work};
    if !args.iter().any(|arg| arg == "--count-heap-work") {
        return Ok(Witness { _collector: None });
    }
    let probe = args.iter().find_map(|arg| arg.strip_prefix("--heap-walk="));
    let probe = match probe {
        None => None,
        Some("checkpoint") => Some(Probe::Checkpoint),
        Some("resume") => Some(Probe::Resume),
        Some("carry") => Some(Probe::Carry),
        _ => return Err(crate::PoolError::InvalidConfiguration),
    };
    let collector = lash_core_execution::perf_witness::Collector::install()
        .map_err(|_| crate::PoolError::InvalidConfiguration)?;
    enable_work(probe);
    ACTIVE.set(true);
    Ok(Witness {
        _collector: Some(collector),
    })
}
#[cfg(not(feature = "testing"))]
pub(crate) fn install(args: &[String]) -> Result<Witness, crate::PoolError> {
    if args
        .iter()
        .any(|arg| arg == "--count-heap-work" || arg.starts_with("--heap-walk="))
    {
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

pub(crate) fn take() -> Option<lash_vm_protocol::WorkerWork> {
    #[cfg(feature = "testing")]
    {
        if !ACTIVE.get() {
            return None;
        }
        let work = lash_kernel_vm::perf_witness::take_work()?;
        let graph = lash_core_execution::perf_witness::graph_work();
        let previous = GRAPH.replace(graph);
        Some(lash_vm_protocol::WorkerWork {
            serialized_body_bytes: graph[4] - previous[4],
            decoded_body_bytes: graph[5] - previous[5],
            vm_steps: work.vm_steps,
            checkpoint_values: work.checkpoint_values,
            resume_values: work.resume_values,
            carry_values: work.carry_values,
            encoded_nodes: graph[0] - previous[0],
            encoded_bytes: graph[1] - previous[1],
            decoded_nodes: graph[2] - previous[2],
            decoded_bytes: graph[3] - previous[3],
            probe_units: work.probe_units,
            peak_buffer_bytes: PEAK.replace(0),
        })
    }
    #[cfg(not(feature = "testing"))]
    None
}
