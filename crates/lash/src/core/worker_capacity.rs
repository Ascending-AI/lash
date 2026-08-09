use super::*;

impl LashCoreBuilder {
    /// Set the number of processes each default inline worker may execute at
    /// once. A running process releases its slot while parked and reacquires it
    /// before resuming. Invalid values are reported by [`Self::build`].
    pub fn process_execution_concurrency(mut self, concurrency: usize) -> Self {
        self.process_execution_concurrency = Some(concurrency);
        self
    }

    /// Set the number of queued-work notifications the default inline driver
    /// may execute at once. Invalid values are reported by [`Self::build`].
    pub fn queued_work_execution_concurrency(mut self, concurrency: usize) -> Self {
        self.queued_work_execution_concurrency = Some(concurrency);
        self
    }

    /// Replace both fixed inline worker bounds with a host admission supplier.
    ///
    /// The supplier is consulted before process or queued work leaves its
    /// scheduler queue. Its RAII permit is held only while that work is running;
    /// parked work releases and later reacquires a slot. Unlike the default
    /// per-worker bounds, explicitly sharing one supplier across workers makes
    /// its admission policy shared across those workers.
    pub fn worker_slot_supplier(mut self, supplier: Arc<dyn WorkerSlotSupplier>) -> Self {
        self.worker_slot_supplier = Some(supplier);
        self
    }
}
