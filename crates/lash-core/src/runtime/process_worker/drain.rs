use super::*;

impl DurableProcessWorker {
    /// Graceful owner drain: terminalize this host's own started `OwnerBound`
    /// work as `Abandoned{OwnerDrain}` at close (ADR 0019).
    ///
    /// This is an explicit **host lever on the worker**, never an implicit
    /// consequence of closing a session. Processes are global and outlive any
    /// one session ([ADR 0011]), so `LashSession::close`/`park` must not touch
    /// them; a host that wants its in-flight owner-bound work terminalized at
    /// shutdown calls this on the worker it is tearing down.
    /// Restate-owned rows use a substrate invocation owner rather than this
    /// worker's configured owner, so this local-worker drain does not select
    /// them; their recovery and abandonment remain Restate/sweep concerns.
    ///
    /// Drain sequence (the operations runbook owns the surrounding steps; this
    /// is the terminal-writing step):
    /// 1. stop admitting new work to this worker;
    /// 2. cancel or await the worker's in-flight run tasks so they release their
    ///    per-run leases — for **Rerunnable** in-flight work that is the whole
    ///    story: stopping the local run task without any terminal write leaves
    ///    the row non-terminal so the next worker re-runs it (its contract);
    /// 3. call this lever: for every non-terminal **OwnerBound** row this exact
    ///    worker started (`first_started.owner == self.config.lease_owner`),
    ///    claim a fresh drain lease and, being the owner completing its own
    ///    work, write `Abandoned{OwnerDrain}` under it — the ordinary graceful
    ///    completion path, respecting the single-writer rule.
    ///
    /// A row still held by a live foreign lease (an in-flight run under one of
    /// this worker's own recovery incarnations that step 2 has not yet released)
    /// is deferred rather than reclaimed, so the drain never races a still-live
    /// run; such a row reaches `Abandoned` on the next drain pass or at a peer's
    /// recovery sweep. Rows started by a different owner, not-yet-started
    /// OwnerBound rows (still claimable by anyone), Rerunnable rows, and
    /// Externally-Owned rows are all left untouched.
    ///
    /// [ADR 0011]: durable process registration is session-independent.
    pub async fn drain_owner_bound_work(&self) -> Result<ProcessDrainReport, PluginError> {
        let mut abandoned = Vec::new();
        let mut deferred = Vec::new();
        let limit = std::num::NonZeroUsize::new(DEFAULT_PROCESS_EXECUTION_CONCURRENCY)
            .expect("default process execution concurrency is non-zero");
        let mut continuation = None;
        loop {
            let page = self
                .config
                .process_registry()
                .list_non_terminal_page(limit, continuation)
                .await?;
            let next = page.continuation;
            for record in page.records {
                if record.disposition != RecoveryContract::OwnerBound {
                    continue;
                }
                let Some(first_started) = record.first_started.as_ref() else {
                    continue;
                };
                if first_started.owner != self.config.lease_owner {
                    continue;
                }
                let owner = first_started.owner.clone();
                match self.drain_one_owner_bound(&record.id, owner).await {
                    RecoveryCompletionDisposition::Committed => abandoned.push(record.id),
                    RecoveryCompletionDisposition::Busy => deferred.push(ProcessDrainDeferred {
                        process_id: record.id,
                        disposition: ProcessRecoveryAttemptOutcome::Busy,
                    }),
                    RecoveryCompletionDisposition::Absent => deferred.push(ProcessDrainDeferred {
                        process_id: record.id,
                        disposition: ProcessRecoveryAttemptOutcome::Absent,
                    }),
                    RecoveryCompletionDisposition::AlreadyApplied(terminal_status) => {
                        deferred.push(ProcessDrainDeferred {
                            process_id: record.id,
                            disposition: ProcessRecoveryAttemptOutcome::AlreadyApplied {
                                terminal_status,
                            },
                        });
                    }
                    RecoveryCompletionDisposition::SettledByPeer(terminal_status) => {
                        deferred.push(ProcessDrainDeferred {
                            process_id: record.id,
                            disposition: ProcessRecoveryAttemptOutcome::SettledByPeer {
                                terminal_status,
                            },
                        });
                    }
                    RecoveryCompletionDisposition::LeaseLost(operation) => {
                        deferred.push(ProcessDrainDeferred {
                            process_id: record.id,
                            disposition: ProcessRecoveryAttemptOutcome::LeaseLost { operation },
                        });
                    }
                    RecoveryCompletionDisposition::BackendError(error) => {
                        deferred.push(ProcessDrainDeferred {
                            process_id: record.id,
                            disposition: error.into_public(),
                        });
                    }
                }
            }
            let Some(next) = next else {
                break;
            };
            continuation = Some(next);
        }
        Ok(ProcessDrainReport {
            abandoned,
            deferred,
        })
    }
}
