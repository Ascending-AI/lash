use super::super::{WorkerSlotKind, WorkerSlotPermit};
use super::*;

impl DurableProcessWorker {
    /// Install one intake page into the scheduler, reporting per row whether it
    /// was admitted now or coalesced onto an attempt this worker already has in
    /// flight.
    pub(super) fn install_worklist_page(
        &self,
        page: crate::ProcessWorklistPage,
    ) -> ProcessAdmissionReport {
        let mut report = ProcessAdmissionReport::default();
        let mut state = self.execution_scheduler.state.lock_recover();
        for record in page.records {
            // ADR 0019: Lash never executes an externally-owned row, so it is
            // never an admission and never contends with one. The row's own
            // declared disposition decides this, not whether some earlier pass
            // already queued it — a second pass over the same row must not
            // relabel it as `Busy`.
            let externally_owned = record.disposition == RecoveryContract::ExternallyOwned;
            if state.scheduled.insert(record.id.clone()) {
                // The inline worker still queues it, because a pending Abandon
                // Request on such a row is reconciled there — but it reports
                // the same typed deferral the Restate tier reports for it.
                if externally_owned {
                    report.deferred.push(ProcessAdmissionDeferred {
                        process_id: record.id.clone(),
                        disposition: ProcessRecoveryAttemptOutcome::ExternallyOwned,
                    });
                } else {
                    report.admitted.push(record.id.clone());
                }
                state.pending.push_back(record);
            } else {
                // A live attempt on this worker already owns the row; this pass
                // did not admit it.
                report.deferred.push(ProcessAdmissionDeferred {
                    process_id: record.id.clone(),
                    disposition: if externally_owned {
                        ProcessRecoveryAttemptOutcome::ExternallyOwned
                    } else {
                        ProcessRecoveryAttemptOutcome::Busy
                    },
                });
                // Coalesce a newer host-driven pass instead of dropping it.
                // The row may have gained an Abandon Request or other
                // execution-relevant state while its prior attempt was still
                // queued or finishing.
                state.rerun.insert(record.id.clone(), record);
            }
        }
        state.worklist_scan = match page.continuation {
            Some(continuation) => ProcessWorklistScan::Ready(Some(continuation)),
            None if state.rescan_requested => {
                state.rescan_requested = false;
                ProcessWorklistScan::Ready(None)
            }
            None => ProcessWorklistScan::Idle,
        };
        self.execution_scheduler.metrics.intake_depth(
            WorkerSlotKind::Process,
            state.pending.len() + state.rerun.len(),
        );
        report
    }

    pub(super) fn next_worklist_page_request(
        &self,
    ) -> Option<(std::num::NonZeroUsize, Option<crate::ProcessWorklistCursor>)> {
        let available = self
            .execution_scheduler
            .slots
            .available_slots(WorkerSlotKind::Process);
        let mut state = self.execution_scheduler.state.lock_recover();
        if !state.pending.is_empty() {
            return None;
        }
        if available == 0 && state.active != 0 {
            return None;
        }
        let limit = std::num::NonZeroUsize::new(
            available.clamp(1, self.config.native_substrate.worker_sweep.intake_page),
        )
        .expect("the clamped intake page bound is non-zero");
        let ProcessWorklistScan::Ready(continuation) = &state.worklist_scan else {
            return None;
        };
        let continuation = continuation.clone();
        state.worklist_scan = ProcessWorklistScan::Fetching(continuation.clone());
        Some((limit, continuation))
    }

    pub(super) async fn fetch_worklist_page_with_retry(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<crate::ProcessWorklistCursor>,
    ) -> Result<crate::ProcessWorklistPage, PluginError> {
        let policy = &self.config.native_substrate.worker_sweep;
        let mut retry_after = policy.fetch_retry_base;
        for attempt in 1..=policy.fetch_attempts {
            match self
                .config
                .process_registry()
                .list_non_terminal_page(limit, continuation.clone())
                .await
            {
                Ok(page) => return Ok(page),
                Err(error) if attempt == policy.fetch_attempts => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        attempt,
                        "retrying incomplete process worklist scan"
                    );
                    tokio::time::sleep(retry_after).await;
                    retry_after = retry_after.saturating_mul(2);
                }
            }
        }
        unreachable!("the non-zero worklist retry budget returns from the loop")
    }

    pub(super) async fn next_process_execution(&self) -> Option<(ProcessRecord, WorkerSlotPermit)> {
        if self
            .execution_scheduler
            .state
            .lock_recover()
            .pending
            .is_empty()
        {
            return None;
        }
        let reserve = self
            .execution_scheduler
            .slots
            .reserve_slot(WorkerSlotKind::Process);
        tokio::pin!(reserve);
        let permit = tokio::select! {
            biased;
            () = self.execution_scheduler.shutdown.cancelled() => return None,
            permit = &mut reserve => permit,
        };
        if self.execution_scheduler.shutdown.is_cancelled() {
            drop(permit);
            return None;
        }
        let mut state = self.execution_scheduler.state.lock_recover();
        let Some(record) = state.pending.pop_front() else {
            drop(permit);
            return None;
        };
        state.active += 1;
        self.execution_scheduler.metrics.intake_depth(
            WorkerSlotKind::Process,
            state.pending.len() + state.rerun.len(),
        );
        Some((record, permit))
    }
}
