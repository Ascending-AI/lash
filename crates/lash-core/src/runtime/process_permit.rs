use lash_core_ids::execution_permit as permit;

/// Default maximum number of processes one DurableProcessWorker executes
/// natively at once.
pub const DEFAULT_PROCESS_EXECUTION_CONCURRENCY: usize = 64;

pub use permit::SharedNotify;
pub use permit::release_process_execution_permit_while;
pub use permit::{ensure_process_execution_permit, inherit_process_execution_permit};
pub use permit::{scope_process_execution_permit, scope_queued_work_execution_permit};

/// The runtime-operation scope under which the worker starts a trigger
/// delivery whose process row was never registered. It exists only to admit
/// that one process, so the process's retention pass retires it alongside the
/// process journal (FIG-2500).
pub fn trigger_delivery_reconcile_scope(process_id: &crate::ProcessId) -> crate::ExecutionScope {
    crate::ExecutionScope::runtime_operation(format!("trigger-delivery-reconcile:{process_id}"))
}
