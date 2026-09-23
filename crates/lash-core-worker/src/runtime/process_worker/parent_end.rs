use super::*;
use crate::{CancelOrigin, ParentEndPlan, ParentScope, ProcessId};

/// The parent-end recovery pass's state, shared by every clone of one worker.
#[derive(Default)]
pub(super) struct ParentEndRecovery {
    /// Where the last pass stopped reading candidates, so the passes a single
    /// worker runs advance one cursor instead of each restarting at the lowest
    /// scope id.
    cursor: tokio::sync::Mutex<Option<String>>,
    /// The drains whose owed end a detached task is writing right now, by
    /// parent storage id: a later pass that finds the same candidate still
    /// unended leaves it to that task rather than stacking a second one on
    /// the same session lane.
    owed_drain_ends: std::sync::Mutex<BTreeSet<String>>,
}

/// Holds one drain's slot in [`ParentEndRecovery::owed_drain_ends`] for the
/// task writing its end, and frees it however the task ends.
struct OwedDrainEndSlot {
    recovery: Arc<ParentEndRecovery>,
    key: String,
}

impl Drop for OwedDrainEndSlot {
    fn drop(&mut self) {
        self.recovery
            .owed_drain_ends
            .lock_recover()
            .remove(&self.key);
    }
}

/// Page size for both the ledger scan and the per-parent children scan.
#[expect(clippy::expect_used, reason = "256 is a non-zero literal")]
fn page_bound() -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(256).expect("parent-end page bound is non-zero")
}

impl DurableProcessWorker {
    /// Re-derive opener parent-end ledger rows a crash swallowed.
    ///
    /// An opener's ledger row is written immediately after its end evidence —
    /// the turn commit for a turn, the drain-end receipt for a queued-work
    /// drain — but not inside it, because the session store and the process
    /// registry are separate stores on every SQL tier. A crash in the gap
    /// would otherwise leave `Cancel` children naming an owner that will
    /// never end again, so recovery closes the window: the registry names the
    /// candidate scopes, and the session that owns each candidate answers
    /// whether that owner's end evidence is durable.
    ///
    /// Only an ended owner gets a row. A turn that crashed before its own
    /// commit is interrupted, not ended — the stop-backtracks-to-checkpoint
    /// rule drops its uncommitted tail so the turn replays and re-registers
    /// exactly the children a sweep would have cancelled — and a drain
    /// interrupted before its epilogue is ended by its retry under the same
    /// `drain_id` — so an unconfirmed candidate is left alone for the redrive
    /// and reconsidered next pass. The one unconfirmed candidate this pass
    /// acts on is a drain whose run already settled: it is ended, its end is
    /// owed, and [`write_owed_drain_end`](Self::write_owed_drain_end) writes
    /// it.
    ///
    /// The pass is bounded and idempotent: `record_parent_end` preserves the
    /// first row, and a scope that already has one is never reported.
    ///
    /// The page is a keyset, not a prefix. A candidate can stay unresolvable
    /// indefinitely — an uncommitted turn that is never redriven, or a session
    /// this worker's factory cannot open — and a prefix page would hand those
    /// scopes every slot on every pass, so a turn with a lexically higher id
    /// would never get its row and its `Cancel` children would outlive it for
    /// good. The cursor advances past everything this pass read, resolvable or
    /// not, and wraps to the start when a pass reads less than a full page, so
    /// a scope that becomes resolvable later is reconsidered on a later lap.
    pub(super) async fn redrive_missing_opener_parent_end_rows(&self) -> Result<(), PluginError> {
        let bound = page_bound();
        let mut cursor = self.parent_end.cursor.lock().await;
        let candidates = self
            .config
            .process_registry()
            .list_unrecorded_opener_parents(cursor.as_deref(), bound)
            .await?;
        // A short page means the scan reached the end of the candidate set, so
        // the next pass starts a new lap; a full page leaves the cursor on the
        // last scope read.
        *cursor = (candidates.len() == bound.get())
            .then(|| candidates.last().and_then(|parent| parent.storage_id()))
            .flatten();
        drop(cursor);
        let mut unopenable_sessions = 0usize;
        for parent in candidates {
            let ParentScope::Owned(opener) = &parent else {
                continue;
            };
            let (session_id, owner_label, confirmed) = match opener {
                crate::EffectOpener::Turn {
                    session_id,
                    turn_id,
                } => {
                    let Some(store) = self.open_session_store_for_read(session_id).await else {
                        unopenable_sessions += 1;
                        continue;
                    };
                    (
                        session_id,
                        format!("turn:{turn_id}"),
                        store.committed_turn_exists(turn_id).await,
                    )
                }
                crate::EffectOpener::QueueDrain {
                    session_id,
                    drain_id,
                } => {
                    let Some(store) = self.open_session_store_for_read(session_id).await else {
                        unopenable_sessions += 1;
                        continue;
                    };
                    let confirmed = store.drain_end_exists(drain_id).await;
                    if matches!(confirmed, Ok(false)) {
                        self.write_owed_drain_end(&parent, session_id, drain_id, store.as_ref())
                            .await;
                    }
                    (session_id, format!("queue_drain:{drain_id}"), confirmed)
                }
                crate::EffectOpener::Process { .. } => continue,
            };
            match confirmed {
                Ok(true) => {
                    self.config
                        .process_registry()
                        .record_parent_end(&parent)
                        .await?;
                }
                // Interrupted, not ended: leave it for the redrive. A drain
                // whose run already settled is ended but owes its end, which
                // `write_owed_drain_end` has just set writing.
                Ok(false) => {}
                // A tier that writes the row inside the same durable execution
                // as the end evidence has no window to re-derive, and reports
                // no candidates; one that reports candidates must answer the
                // read.
                Err(crate::StoreError::UnsupportedStoreOperation { .. }) => {}
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        owner = %owner_label,
                        error = %error,
                        "opener-end read failed; the parent-end row stays owed",
                    );
                }
            }
        }
        // Skipping is silent per candidate but never silent per pass: a worker
        // whose factory cannot open the sessions owning these scopes is the one
        // condition under which the sweep makes no progress at all.
        if unopenable_sessions > 0 {
            tracing::warn!(
                unopenable_sessions,
                "parent-end candidates skipped: their owning session store could not be opened",
            );
        }
        Ok(())
    }

    /// Write the end a settled drain still owes (FIG-3563).
    ///
    /// Every terminal settlement of a drain's run is its end, but the
    /// epilogue that follows the settlement withholds the end receipt while a
    /// closing group under the drain's scope owes work leased to another host
    /// — and a settled run is never retried, so no drain under that id asks
    /// again. This pass is what asks. A candidate with no receipt whose run
    /// reads terminally settled gets a runtime over its session, and
    /// [`LashRuntime::end_settled_queue_drain`] runs the one drain-end
    /// epilogue under the session lane: the closing groups resume, and the
    /// receipt and ledger row land once the owed work has settled. Until
    /// then the epilogue withholds again and the next pass retries — the
    /// candidate stays listed, since it still has no ledger row.
    ///
    /// A drain whose run is still pending is interrupted, not ended: its own
    /// retry ends it, and this pass leaves it alone.
    ///
    /// The write runs on a detached task. The epilogue waits out an
    /// obligation this process is itself running — unbounded for a
    /// `RunToCompletion` child — and the pass that found the candidate must
    /// not stall process intake behind it. One task per drain at a time.
    async fn write_owed_drain_end(
        &self,
        parent: &ParentScope,
        session_id: &crate::SessionId,
        drain_id: &str,
        store: &dyn crate::store::RuntimePersistence,
    ) {
        let scope = crate::ExecutionScope::queue_drain(session_id.clone(), drain_id);
        match store.queued_run(&scope).await {
            Ok(Some(run)) if run.terminal.is_some() => {}
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    drain_id = %drain_id,
                    error = %error,
                    "queue drain run read failed; an owed drain end waits for the next pass",
                );
                return;
            }
        }
        let Some(key) = parent.storage_id() else {
            return;
        };
        if !self
            .parent_end
            .owed_drain_ends
            .lock_recover()
            .insert(key.clone())
        {
            return;
        }
        let slot = OwedDrainEndSlot {
            recovery: Arc::clone(&self.parent_end),
            key,
        };
        let worker = self.detached_for_task();
        let session_id = session_id.clone();
        let drain_id = drain_id.to_string();
        crate::task::spawn(async move {
            let _slot = slot;
            let shutdown = worker.execution_scheduler.shutdown.clone();
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = worker.drive_owed_drain_end(&session_id, &drain_id) => {}
            }
        });
    }

    async fn drive_owed_drain_end(&self, session_id: &crate::SessionId, drain_id: &str) {
        let Some(store) = self.open_session_store_for_read(session_id).await else {
            return;
        };
        let mut runtime = match self.runtime_for_session_store(session_id, store).await {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    drain_id = %drain_id,
                    error = %error,
                    "could not open the session that owes a drain end; the next pass retries",
                );
                return;
            }
        };
        if let Err(error) = Box::pin(runtime.end_settled_queue_drain(drain_id)).await {
            tracing::debug!(
                session_id = %session_id,
                drain_id = %drain_id,
                error = %error,
                "owed drain end not written this pass",
            );
        }
    }

    /// A runtime over an existing session's own store.
    ///
    /// A session with a committed head restores its recorded state and
    /// policy; the worker's session policy only stands in for a session that
    /// never committed one — a drain whose first run failed before its commit
    /// — the way it does for every session this worker opens. Its provider is
    /// cleared: the worker never pins a provider on a session it does not own,
    /// so a recorded pin is kept and an unrecorded one stays unrecorded.
    async fn runtime_for_session_store(
        &self,
        session_id: &crate::SessionId,
        store: std::sync::Arc<dyn crate::store::RuntimePersistence>,
    ) -> Result<LashRuntime, crate::SessionError> {
        let mut policy = self.config.session_policy.clone();
        policy.session_id = Some(session_id.clone());
        policy.provider_id = String::new();
        let builder = EmbeddedRuntimeBuilder::new(
            self.config.runtime_host.durability.commit_budget,
            self.config
                .runtime_host
                .durability
                .queued_work_batching
                .clone(),
            self.config.lease_owner.clone(),
        )
        .with_session_id(session_id.to_string())
        .with_policy(policy)
        .with_plugin_host(self.config.plugin_host.as_ref().clone())
        .with_runtime_host(self.config.runtime_host.clone())
        .with_session_store_factory(Arc::clone(&self.config.session_store_factory))
        .with_trigger_store(Arc::clone(&self.config.trigger_store))
        .with_process_work(self.process_wiring())
        .with_store(store)
        .with_queued_work(Arc::clone(&self.config.queued_work));
        Box::pin(builder.build()).await
    }

    /// Never creates or admits a session: a factory without the
    /// open-an-existing-store seam yields nothing and the candidate is left
    /// for a worker whose factory has it.
    async fn open_session_store_for_read(
        &self,
        session_id: &crate::SessionId,
    ) -> Option<std::sync::Arc<dyn crate::store::RuntimePersistence>> {
        let request = crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: crate::SessionRelation::default(),
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        };
        match self
            .config
            .session_store_factory
            .open_existing_store(&request)
            .await
        {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "could not open the session owning a parent-end candidate",
                );
                None
            }
        }
    }

    /// Settle every parent scope whose ledger row is still pending.
    ///
    /// One failing row never aborts the page: the row stays pending and the
    /// next pass retries it, so a single unreachable child cannot starve every
    /// other parent's children of their cancel.
    pub(super) async fn drive_pending_parent_end_plans(&self) -> Result<(), PluginError> {
        let mut deferred: Vec<ParentScope> = Vec::new();
        loop {
            let plans = self
                .config
                .process_registry()
                .list_pending_parent_end_plans(page_bound())
                .await?;
            let unfinished = plans
                .iter()
                .filter(|plan| !deferred.contains(&plan.parent))
                .count();
            if unfinished == 0 {
                return Ok(());
            }
            for plan in plans {
                if deferred.contains(&plan.parent) {
                    continue;
                }
                if let Err(error) = self.settle_parent_end_plan(&plan).await {
                    tracing::warn!(
                        parent_kind = plan.parent.storage_kind(),
                        parent_id = plan.parent.storage_id().unwrap_or_default(),
                        error = %error,
                        "parent-end plan stays pending for the next sweep pass",
                    );
                    deferred.push(plan.parent);
                }
            }
        }
    }

    /// Settle one parent scope: request `ParentEnded` cancel on every `Cancel`
    /// child, then mark the ledger row settled.
    ///
    /// A terminal child and a child that already carries a cancel request are
    /// settled by definition and are never returned by the children query, so
    /// two sweeps racing on the same parent converge instead of conflicting.
    pub async fn settle_parent_end_plan(&self, plan: &ParentEndPlan) -> Result<(), PluginError> {
        let requester = plan
            .parent
            .storage_id()
            .unwrap_or_else(|| plan.parent.storage_kind().to_string());
        let mut after: Option<ProcessId> = None;
        loop {
            let children = self
                .config
                .process_registry()
                .list_parent_end_children(&plan.parent, after.as_ref(), page_bound())
                .await?;
            let Some(last) = children.last() else { break };
            after = Some(last.id.clone());
            for child in &children {
                match self
                    .config
                    .process_registry()
                    .request_process_cancel(
                        &crate::ProcessRef::from_record(child),
                        CancelOrigin::ParentEnded,
                        requester.clone(),
                        None,
                    )
                    .await
                {
                    Ok(_) => {}
                    // A child that went terminal between the page read and the
                    // write needs no cancel: the scope it belonged to is over
                    // for it too. Any other refusal keeps the row pending.
                    Err(PluginError::ProcessAlreadyTerminal { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        self.config
            .process_registry()
            .settle_parent_end_plan(&plan.parent)
            .await
    }

    pub(super) async fn finish_terminal_run(
        &self,
        lease: &ProcessLease,
        process_id: &ProcessId,
        output: Box<ProcessAwaitOutput>,
    ) -> super::recovery::ProcessRecoveryOutcome {
        let completion = self.complete_and_release(lease, process_id, *output).await;
        let terminal_written = matches!(
            completion,
            Ok(()) | Err(ProcessRecoveryAttemptOutcome::AlreadyApplied { .. })
        );
        let outcome = ProcessRecoveryOutcome::from_completion(completion);
        // The ledger row rode the terminal write. Sweeping it now is an
        // optimisation, not the guarantee: a failure here leaves the row
        // pending for the next pass.
        if terminal_written
            && let Some(plan) = self.pending_plan_for_process(process_id).await
            && let Err(error) = self.settle_parent_end_plan(&plan).await
        {
            tracing::warn!(
                process_id = %process_id,
                error = %error,
                "durable parent-end plan remains pending after terminal completion",
            );
        }
        outcome
    }

    async fn pending_plan_for_process(&self, process_id: &ProcessId) -> Option<ParentEndPlan> {
        let record = self
            .config
            .process_registry()
            .get_process(process_id)
            .await
            .ok()
            .flatten()?;
        let parent = ParentScope::process(crate::ProcessRef::from_record(&record));
        self.config
            .process_registry()
            .get_parent_end_plan(&parent)
            .await
            .ok()
            .flatten()
            .filter(|plan| plan.settled_at_ms.is_none())
    }
}
