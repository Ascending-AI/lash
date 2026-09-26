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
            let process_id = record.id.clone();
            // A newer page's record replaces a retained rerun: the row may have
            // gained an Abandon Request or other execution-relevant state while
            // its prior attempt was still queued or finishing.
            if state.admit(process_id.clone(), record, |retained, incoming| {
                *retained = incoming
            }) {
                // The native worker still queues it, because a pending Abandon
                // Request on such a row is reconciled there — but it reports
                // the same typed deferral the Restate tier reports for it.
                if externally_owned {
                    report.deferred.push(ProcessAdmissionDeferred {
                        process_id: process_id.clone(),
                        disposition: ProcessRecoveryAttemptOutcome::ExternallyOwned,
                    });
                } else {
                    report.admitted.push(process_id);
                }
            } else {
                // A live attempt on this worker already owns the row; this pass
                // did not admit it.
                report.deferred.push(ProcessAdmissionDeferred {
                    process_id,
                    disposition: if externally_owned {
                        ProcessRecoveryAttemptOutcome::ExternallyOwned
                    } else {
                        ProcessRecoveryAttemptOutcome::Busy
                    },
                });
            }
        }
        state.extra.page_installed(page.continuation);
        self.execution_scheduler
            .metrics
            .intake_depth(WorkerSlotKind::Process, state.intake_depth());
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
        if state.has_queued() {
            return None;
        }
        if available == 0 && state.running_count() != 0 {
            return None;
        }
        let limit = std::num::NonZeroUsize::new(available)
            .unwrap_or(std::num::NonZeroUsize::MIN)
            .min(self.config.native_substrate.worker_sweep.intake_page);
        let continuation = state.extra.take_page_request()?;
        Some((limit, continuation))
    }

    pub(super) async fn fetch_worklist_page_with_retry(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<crate::ProcessWorklistCursor>,
    ) -> Result<crate::ProcessWorklistPage, PluginError> {
        let policy = &self.config.native_substrate.worker_sweep;
        let mut retry_after = policy.fetch_retry_base;
        let mut attempt = 1;
        loop {
            match self
                .config
                .process_registry()
                .list_non_terminal_page(limit, continuation.clone())
                .await
            {
                Ok(page) => return Ok(page),
                Err(error) if attempt == policy.fetch_attempts.get() => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        attempt,
                        "retrying incomplete process worklist scan"
                    );
                    tokio::time::sleep(retry_after).await;
                    retry_after = retry_after.saturating_mul(2);
                    attempt += 1;
                }
            }
        }
    }

    pub(super) async fn next_process_execution(&self) -> Option<(ProcessRecord, WorkerSlotPermit)> {
        if !self.execution_scheduler.state.lock_recover().has_queued() {
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
        let Some(record) = state.pop_next() else {
            drop(permit);
            return None;
        };
        self.execution_scheduler
            .metrics
            .intake_depth(WorkerSlotKind::Process, state.intake_depth());
        Some((record, permit))
    }
}

/// The worklist paging state the process scheduler carries beside the shared
/// coalescing protocol fields, under the same lock.
///
/// A recorded rescan rides inside the in-flight variants, so "a rescan is
/// owed while no scan is running" is unrepresentable: the fact can only ever
/// describe the scan it belongs to.
#[derive(Default)]
pub(super) enum ProcessWorklistScan {
    /// No scan is running and none is owed.
    #[default]
    Idle,
    /// A page fetch is in flight. `rescan` records that a drive arrived
    /// mid-pass and the worklist must be read again from the head once this
    /// pass ends.
    Fetching {
        continuation: Option<crate::ProcessWorklistCursor>,
        rescan: bool,
    },
    /// Between pages: `continuation` is the next page to fetch. `rescan`
    /// means a full pass from the head is owed once this one ends.
    Ready {
        continuation: Option<crate::ProcessWorklistCursor>,
        rescan: bool,
    },
}

/// What an admission pass must do about the worklist — the answer that
/// decides whether the call's report reads `Scanned` or `Coalesced`.
pub(super) enum ProcessPassBegin {
    /// The scan was idle: this call reads the first page itself.
    FetchInitialPage,
    /// A scan is already in flight: this pass recorded its rescan on it and
    /// takes no intake of its own.
    Coalesced,
}

impl ProcessWorklistScan {
    /// Begin an admission pass: idle scans start fetching the first page;
    /// anything already running records that a rescan is owed.
    pub(super) fn begin_pass(&mut self) -> ProcessPassBegin {
        match self {
            Self::Idle => {
                *self = Self::Fetching {
                    continuation: None,
                    rescan: false,
                };
                ProcessPassBegin::FetchInitialPage
            }
            Self::Fetching { rescan, .. } | Self::Ready { rescan, .. } => {
                *rescan = true;
                ProcessPassBegin::Coalesced
            }
        }
    }

    /// The pass's own fetch failed. A recorded rescan is consumed into a ready
    /// restart from the head of the worklist — the returned flag says that
    /// restart needs a dispatcher to run it.
    pub(super) fn scan_failed(&mut self) -> bool {
        let rescan = self.rescan();
        *self = if rescan {
            Self::Ready {
                continuation: None,
                rescan: false,
            }
        } else {
            Self::Idle
        };
        rescan
    }

    /// A fetched page was admitted. A trailing continuation keeps the pass
    /// ready; the last page idles the scan, or restarts it from the head when
    /// a rescan was recorded mid-pass.
    pub(super) fn page_installed(&mut self, next: Option<crate::ProcessWorklistCursor>) {
        let rescan = self.rescan();
        *self = match next {
            Some(continuation) => Self::Ready {
                continuation: Some(continuation),
                rescan,
            },
            None if rescan => Self::Ready {
                continuation: None,
                rescan: false,
            },
            None => Self::Idle,
        };
    }

    /// Take the ready continuation into an in-flight fetch. `None` while the
    /// scan is not ready to page.
    pub(super) fn take_page_request(&mut self) -> Option<Option<crate::ProcessWorklistCursor>> {
        let Self::Ready {
            continuation,
            rescan,
        } = self
        else {
            return None;
        };
        let continuation = continuation.clone();
        *self = Self::Fetching {
            continuation: continuation.clone(),
            rescan: *rescan,
        };
        Some(continuation)
    }

    /// A dispatcher-side fetch failed for good: the scan parks ready at the
    /// failed cursor with a rescan owed for whichever dispatcher runs next.
    pub(super) fn park_for_rescan(&mut self, continuation: Option<crate::ProcessWorklistCursor>) {
        *self = Self::Ready {
            continuation,
            rescan: true,
        };
    }

    /// The idle sweep's fresh scan: idle becomes ready at the head of the
    /// worklist, where the next dispatcher pass reads from.
    pub(super) fn schedule_idle_pass(&mut self) {
        if matches!(self, Self::Idle) {
            *self = Self::Ready {
                continuation: None,
                rescan: false,
            };
        }
    }

    fn rescan(&self) -> bool {
        match self {
            Self::Fetching { rescan, .. } | Self::Ready { rescan, .. } => *rescan,
            Self::Idle => false,
        }
    }
}

impl CoalescingExtra for ProcessWorklistScan {
    /// A dispatcher that exits mid-fetch rewinds the scan to the cursor it was
    /// reading, so the next dispatcher retries that cursor rather than
    /// skipping it.
    fn on_dispatcher_exit(&mut self) {
        if let Self::Fetching {
            continuation,
            rescan,
        } = self
        {
            *self = Self::Ready {
                continuation: continuation.clone(),
                rescan: *rescan,
            };
        }
    }
}
