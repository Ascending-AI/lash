use super::*;
use crate::{CancelOrigin, ParentEndPlan, ParentScope, ProcessId};

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
    /// and reconsidered next pass.
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
        let mut cursor = self.parent_end_cursor.lock().await;
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
                    (
                        session_id,
                        format!("queue_drain:{drain_id}"),
                        store.drain_end_exists(drain_id).await,
                    )
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
                // Interrupted, not ended: leave it for the redrive.
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
