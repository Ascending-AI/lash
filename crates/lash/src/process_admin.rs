//! The global process facade surface.
//!
//! [`Processes`] (reached via [`LashCore::processes`](crate::LashCore::processes),
//! re-exported as [`lash::process::Processes`](crate::process::Processes)) is THE
//! host-level process surface (ADR 0019 grill): start, observe, signal, cancel,
//! transfer, prune, and abandon-request every process, with the two distinct
//! scope filters — `observed_by` (what a session may address) and `originated_by`
//! (what a session created). The session-scoped
//! [`SessionProcessAdmin`](crate::admin::SessionProcessAdmin) is thin sugar over
//! this surface pre-filtered by a session's observer edge; it lives in `admin` because it
//! wraps a [`SessionAdmin`](crate::admin::SessionAdmin).

use crate::support::*;
use lash_core::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::sync::MutexExt;

struct SurveyedTriggerStore<'a> {
    inner: &'a dyn lash_core::TriggerStore,
    retention_candidates: std::sync::Mutex<Vec<lash_core::TriggerDeliveryRetentionCandidate>>,
}

impl<'a> SurveyedTriggerStore<'a> {
    fn new(
        inner: &'a dyn lash_core::TriggerStore,
        retention_candidates: Vec<lash_core::TriggerDeliveryRetentionCandidate>,
    ) -> Self {
        Self {
            inner,
            retention_candidates: std::sync::Mutex::new(retention_candidates),
        }
    }

    fn delivery_process_ids(&self) -> Vec<String> {
        self.retention_candidates
            .lock_recover()
            .iter()
            .map(|candidate| candidate.process_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn protected_process_count(&self) -> usize {
        self.delivery_process_ids().len()
    }
}

#[async_trait::async_trait]
impl lash_core::TriggerStore for SurveyedTriggerStore<'_> {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core::TriggerCommand,
    ) -> std::result::Result<lash_core::TriggerEffectResult, lash_core::PluginError> {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: lash_core::TriggerSubscriptionFilter,
    ) -> std::result::Result<Vec<lash_core::TriggerSubscriptionRecord>, lash_core::PluginError>
    {
        self.inner.list_subscriptions(filter).await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &str,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> std::result::Result<lash_core::TriggerIngressReceipt, lash_core::PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: lash_core::TriggerOccurrenceFilter,
    ) -> std::result::Result<Vec<lash_core::TriggerOccurrenceRecord>, lash_core::PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(
        &self,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner.list_deliveries().await
    }

    async fn list_delivery_process_ids(
        &self,
    ) -> std::result::Result<Vec<String>, lash_core::PluginError> {
        Ok(self.delivery_process_ids())
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> std::result::Result<
        Vec<lash_core::TriggerDeliveryRetentionCandidate>,
        lash_core::PluginError,
    > {
        Ok(self.retention_candidates.lock_recover().clone())
    }

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> std::result::Result<Vec<String>, lash_core::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[String],
    ) -> std::result::Result<lash_core::TriggerRetentionReconciliationReport, lash_core::PluginError>
    {
        let report = self
            .inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await?;
        if report.reclaimed_delivery_count == candidates.len() {
            let deleted_candidates = candidates
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            self.retention_candidates
                .lock_recover()
                .retain(|candidate| !deleted_candidates.contains(candidate));
        }
        Ok(report)
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
    ) -> std::result::Result<usize, lash_core::PluginError> {
        let deleted = self
            .inner
            .delete_delivery_retention_candidates(candidates)
            .await?;
        if deleted == candidates.len() {
            let deleted_candidates = candidates
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            self.retention_candidates
                .lock_recover()
                .retain(|candidate| !deleted_candidates.contains(candidate));
        }
        Ok(deleted)
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core::TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }
}

#[derive(Clone)]
pub struct Processes {
    pub(crate) core: LashCore,
}

impl Processes {
    fn registry(&self) -> Result<Arc<dyn lash_core::ProcessRegistry>> {
        self.core
            .env
            .process_registry
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                EmbedError::Plugin(lash_core::PluginError::Session(
                    "process registry is unavailable in this runtime".to_string(),
                ))
            })
    }

    fn make_observer(&self) -> Result<lash_core::facade_support::ProcessWorkObserver> {
        Ok(lash_core::facade_support::ProcessWorkObserver::new(
            self.registry()?,
        ))
    }

    /// The listing filter [`prune`](Self::prune) surveys effect-journal
    /// retirement candidates with. It is the caller's retention filter verbatim
    /// so the journals retired are exactly the rows the prune can delete, and
    /// absence widens to every status rather than the `Running` default.
    fn prune_selection(
        filter: Option<&lash_core::ProcessListFilter>,
    ) -> Result<lash_core::ProcessListFilter> {
        let Some(filter) = filter else {
            return Ok(lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            });
        };
        if matches!(
            filter.status,
            lash_core::ProcessStatusFilter::Running | lash_core::ProcessStatusFilter::Waiting
        ) {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                format!(
                    "process retention filter selects the live status `{}`, \
                     which no prunable row can hold; pass \
                     `ProcessStatusFilter::Any`, a terminal status, or \
                     `CallerDeparted`",
                    filter.status.label().unwrap_or("any")
                ),
            )));
        }
        Ok(filter.clone())
    }

    fn process_invocation(command: &lash_core::ProcessCommand) -> lash_core::RuntimeInvocation {
        let effect_id = command.effect_id();
        lash_core::RuntimeInvocation::effect(
            lash_core::runtime::RuntimeScope::new("runtime"),
            effect_id.clone(),
            lash_core::RuntimeEffectKind::Process,
            effect_id,
        )
    }

    async fn run_command(
        &self,
        command: lash_core::ProcessCommand,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEffectOutcome> {
        let registry = self.registry()?;
        let invocation = Self::process_invocation(&command);
        let outcome = scoped_effect_controller
            .execute_process_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                lash_core::RuntimeEffectLocalExecutor::processes(
                    registry,
                    self.core.env.process_work_driver.clone(),
                ),
            )
            .await
            .map_err(|err| EmbedError::Plugin(lash_core::PluginError::Session(err.to_string())))?;
        match outcome {
            lash_core::RuntimeEffectOutcome::Process { result } => Ok(result),
            _ => Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process effect returned non-process outcome".to_string(),
            ))),
        }
    }

    /// Engine-admission ruling (FIG-1488): this route deliberately stays outside
    /// the gate. It is an operator seam — the host names the registration
    /// itself, on its own authority, exactly as a host calling the process
    /// registry directly does. The gate exists to stop a *model or leaf* payload
    /// from becoming a committed start; it is not a guard against the operator's
    /// own request. `ProcessEngine::run` still refuses an unrunnable row.
    pub async fn start(
        &self,
        request: lash_core::ProcessStartRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessRecord> {
        let env_ref = match request.env_spec.as_ref() {
            Some(env_spec) => Some(
                lash_core::runtime::persist_process_execution_env(
                    self.core.env.core.durability.process_env_store.as_ref(),
                    env_spec,
                )
                .await?,
            ),
            None => None,
        };
        let observers = request.observers.clone();
        let registration = request.into_registration(env_ref);
        let command = lash_core::ProcessCommand::Start {
            registration,
            observers,
            env_spec: None,
            execution_context: Box::new(lash_core::ProcessExecutionContext::default()),
        };
        let outcome = self
            .run_command(command, scoped_effect_controller.clone())
            .await?;
        let lash_core::ProcessEffectOutcome::Start { record } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process start returned the wrong outcome".to_string(),
            )));
        };
        if let Some(driver) = self.core.work_driver.drivers().await.process {
            let _ = driver.claim_and_run_pending("admin_process_start").await?;
        }
        Ok(*record)
    }

    pub async fn list(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?.list(filter).await.map_err(Into::into)
    }

    /// List processes a session may address — the **observer** filter.
    /// This is the visibility lens (what a session may see), distinct
    /// from [`list_originated_by`](Self::list_originated_by). `session.processes()`
    /// is thin sugar over this method pre-scoped to the session's observer edge.
    pub async fn list_observed_by(
        &self,
        session_scope: &lash_core::SessionScope,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?
            .list_observed_by(session_scope, filter)
            .await
            .map_err(Into::into)
    }

    /// List processes a session originated — the **provenance** filter (ADR
    /// 0019). This is the lineage lens (what a session created), distinct from
    /// [`list_observed_by`](Self::list_observed_by): a process a session started
    /// then transferred away still matches here, and one merely observed by it
    /// does not.
    pub async fn list_originated_by(
        &self,
        session_scope: &lash_core::SessionScope,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?
            .list_originated_by(session_scope, filter)
            .await
            .map_err(Into::into)
    }

    pub async fn get(
        &self,
        process_id: &str,
    ) -> Result<Option<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?
            .process(process_id)
            .await
            .map_err(Into::into)
    }

    pub async fn events(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcessEvent>> {
        self.make_observer()?
            .events_after(process_id, after_sequence)
            .await
            .map_err(Into::into)
    }

    pub async fn await_output(&self, process_id: &str) -> Result<lash_core::ProcessAwaitOutput> {
        if let Some(driver) = self.core.env.process_work_driver.as_ref() {
            return driver.await_terminal(process_id).await.map_err(Into::into);
        }
        lash_core::facade_support::ProcessAwaiter::polling(self.registry()?)
            .await_terminal(process_id)
            .await
            .map_err(Into::into)
    }

    pub async fn cancel(
        &self,
        process_id: &str,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessCancelReceipt> {
        let command = lash_core::ProcessCommand::Cancel {
            process_id: process_id.to_string(),
            reason: Some("requested by host".to_string()),
            replay: None,
        };
        let outcome = self
            .run_command(command, scoped_effect_controller.clone())
            .await?;
        let lash_core::ProcessEffectOutcome::Cancel { record } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process cancel returned the wrong outcome".to_string(),
            )));
        };
        Ok(lash_core::ProcessCancelReceipt::from_record(*record))
    }

    pub async fn signal(
        &self,
        process_id: &str,
        signal_name: impl Into<String>,
        signal_id: impl Into<String>,
        request: lash_core::ProcessEventAppendRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEvent> {
        let command = lash_core::ProcessCommand::Signal {
            process_id: process_id.to_string(),
            signal_name: signal_name.into(),
            signal_id: signal_id.into(),
            request,
        };
        let outcome = self
            .run_command(command, scoped_effect_controller.clone())
            .await?;
        let lash_core::ProcessEffectOutcome::Signal { event } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process signal returned the wrong outcome".to_string(),
            )));
        };
        Ok(*event)
    }

    pub async fn session_snapshot(
        &self,
        session_id: impl Into<String>,
    ) -> Result<lash_core::facade_support::ProcessWorkSnapshot> {
        self.make_observer()?
            .snapshot_for_session(session_id)
            .await
            .map_err(Into::into)
    }

    pub fn observer(&self) -> Result<lash_core::facade_support::ProcessWorkObserver> {
        self.make_observer()
    }

    /// Cancel every currently-running process. A host-wide lever; for a
    /// session-scoped stop use [`SessionProcessAdmin::cancel_all`](crate::admin::SessionProcessAdmin::cancel_all).
    pub async fn cancel_all(
        &self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<Vec<lash_core::ProcessCancelReceipt>> {
        let running = self
            .list(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Running,
                ..lash_core::ProcessListFilter::default()
            })
            .await?;
        let mut summaries = Vec::with_capacity(running.len());
        for process in running {
            summaries.push(
                self.cancel(&process.process_id, scoped_effect_controller.clone())
                    .await?,
            );
        }
        Ok(summaries)
    }

    /// Move observer membership for `process_ids` from one session to another.
    /// Processes are global; this re-homes only observer membership, never the
    /// process itself.
    pub async fn transfer(
        &self,
        from_scope: &lash_core::SessionScope,
        to_scope: &lash_core::SessionScope,
        process_ids: &[String],
    ) -> Result<()> {
        self.registry()?
            .transfer_observers(
                &from_scope.session_id,
                &to_scope.session_id,
                process_ids,
                lash_core::ProcessObserverBy::host("admin-transfer"),
            )
            .await
            .map_err(Into::into)
    }

    /// Host-scheduled retention lever (ADR 0017): physically delete retired
    /// process rows (and their events, observer edges, leases) older than
    /// `cutoff_epoch_ms`, returning what was reclaimed. Retired is the terminal
    /// outcomes plus
    /// [`ProcessStatus::CallerDeparted`](lash_core::ProcessStatus::CallerDeparted),
    /// which nothing may ever honestly terminalize. The configured trigger
    /// store then removes exact delivery reservations for processes now
    /// represented by tombstones. In the same trigger-store transaction it
    /// reclaims empty-fan-out occurrences and trigger rows whose session owner
    /// has crossed the ADR 0049 deletion frontier. Host and platform name
    /// fences remain permanent. Live process rows — running and waiting — are
    /// never touched. Lash exposes no finite maximum waiter
    /// lifetime: the host must retain rows beyond every still-replayable await,
    /// and a later await after pruning receives the typed
    /// `ProcessNoLongerRetained` outcome. Pass
    /// either the projector's acknowledged
    /// [`ProjectionWatermark::UpTo`](lash_core::ProjectionWatermark::UpTo)
    /// cursor or an explicit
    /// [`ProjectionWatermark::NoProjector`](lash_core::ProjectionWatermark::NoProjector).
    ///
    /// `filter` narrows *which* eligible retired rows this call reclaims (ADR
    /// 0023): retention is differentiated host policy, so a host expresses
    /// "reclaim the work this deleted session originated" and "reclaim terminal
    /// subagent debris after a day" as two scheduled calls over the same lever.
    /// `None` considers every retired row. Because retention only ever deletes
    /// retired rows, a filter that selects
    /// [`ProcessStatusFilter::Running`](lash_core::ProcessStatusFilter::Running)
    /// or [`Waiting`](lash_core::ProcessStatusFilter::Waiting) — including the
    /// `Running` default a `..Default::default()` filter carries — can never
    /// match, so it is refused instead of silently reclaiming nothing.
    pub async fn prune(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<&lash_core::ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<lash_core::ProcessPruneReport> {
        let registry = self.registry()?;
        let candidates = registry
            .list_processes(&Self::prune_selection(filter)?)
            .await?;
        for process in candidates
            .into_iter()
            // Survey exactly the rows the prune SQL deletes: the retired
            // partition, which includes `CallerDeparted` alongside the
            // terminal outcomes. Surveying only terminal rows would leak the
            // effect journal of every caller-departed row the SQL reclaims.
            .filter(|process| {
                process.status.is_retired() && process.updated_at_ms < cutoff_epoch_ms
            })
        {
            if let Err(err) = self
                .core
                .env
                .core
                .control
                .effect_host
                .retire_effect_journal(lash_core::EffectJournalRetirement::process(
                    process.id.clone(),
                ))
                .await
            {
                tracing::warn!(
                    failure_stage = "retire_process_effect_journal",
                    cutoff_epoch_ms,
                    process_id = %process.id,
                    error = %err,
                    "process retention failed"
                );
                return Err(err.into());
            }
        }
        let mut report = match registry
            .prune_terminal_processes(cutoff_epoch_ms, filter.cloned(), watermark)
            .await
        {
            Ok(report) => report,
            Err(err) => {
                tracing::warn!(
                    failure_stage = "prune_process_registry",
                    cutoff_epoch_ms,
                    error = %err,
                    "process retention failed"
                );
                return Err(err.into());
            }
        };
        if let Some(trigger_store) = self.core.env.trigger_store.as_ref() {
            let retention = match lash_core::facade_support::reconcile_pruned_trigger_deliveries(
                registry.as_ref(),
                trigger_store.as_ref(),
                self.core.store_factory.as_deref(),
            )
            .await
            {
                Ok(retention) => retention,
                Err(err) => {
                    tracing::warn!(
                        failure_stage = "reconcile_trigger_deliveries_after_process_prune",
                        cutoff_epoch_ms,
                        pruned_processes = report.pruned_processes,
                        pruned_events = report.pruned_events,
                        error = %err,
                        "process retention partially completed"
                    );
                    return Err(err.into());
                }
            };
            report.pruned_trigger_deliveries = retention.reclaimed_delivery_count;
            tracing::info!(
                reclaimed_trigger_deliveries = retention.reclaimed_delivery_count,
                reclaimed_trigger_occurrences = retention.reclaimed_occurrence_count,
                reclaimed_trigger_subscriptions = retention.reclaimed_subscription_count,
                reclaimed_trigger_mutation_receipts = retention.reclaimed_mutation_receipt_count,
                "completed trigger retention after process prune"
            );
        }
        Ok(report)
    }

    /// Compact payload-free process tombstones while structurally excluding
    /// every process id referenced by an outstanding trigger delivery.
    /// Reconciliation runs first, then the raw registry compaction lever surveys
    /// the configured trigger store itself and refuses every matching tombstone.
    /// A configured trigger store that cannot be surveyed or reconciled blocks
    /// compaction, so tombstones accumulate until it recovers rather than
    /// allowing recovery evidence to become orphaned. The caller supplies the
    /// same explicit projection watermark required by the registry retention
    /// contract.
    pub async fn compact_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<usize> {
        let registry = self.registry()?;
        let surveyed_trigger_store = if let Some(trigger_store) =
            self.core.env.trigger_store.as_ref()
        {
            let retention_candidates =
                match trigger_store.list_delivery_retention_candidates().await {
                    Ok(candidates) => candidates,
                    Err(err) => {
                        tracing::warn!(
                            failure_stage = "survey_outstanding_trigger_deliveries",
                            cutoff_epoch_ms,
                            error = %err,
                            "process tombstone compaction blocked"
                        );
                        return Err(err.into());
                    }
                };
            let surveyed_trigger_store =
                SurveyedTriggerStore::new(trigger_store.as_ref(), retention_candidates);
            let reconciled_trigger_deliveries =
                match lash_core::facade_support::reconcile_pruned_trigger_deliveries(
                    registry.as_ref(),
                    &surveyed_trigger_store,
                    self.core.store_factory.as_deref(),
                )
                .await
                {
                    Ok(retention) => retention.reclaimed_delivery_count,
                    Err(err) => {
                        tracing::warn!(
                            failure_stage = "reconcile_trigger_deliveries_before_compaction",
                            cutoff_epoch_ms,
                            protected_process_count = surveyed_trigger_store.protected_process_count(),
                            error = %err,
                            "process tombstone compaction blocked"
                        );
                        return Err(err.into());
                    }
                };
            tracing::debug!(
                protected_process_count = surveyed_trigger_store.protected_process_count(),
                reconciled_trigger_deliveries,
                "prepared delivery-aware process tombstone compaction"
            );
            Some(surveyed_trigger_store)
        } else {
            None
        };
        match registry
            .compact_process_tombstones(
                cutoff_epoch_ms,
                watermark,
                surveyed_trigger_store
                    .as_ref()
                    .map(|store| store as &dyn lash_core::TriggerStore),
            )
            .await
        {
            Ok(compacted) => Ok(compacted),
            Err(err) => {
                tracing::warn!(
                    failure_stage = "compact_process_tombstones",
                    cutoff_epoch_ms,
                    protected_process_count = surveyed_trigger_store
                        .as_ref()
                        .map_or(0, SurveyedTriggerStore::protected_process_count),
                    error = %err,
                    "process tombstone compaction failed"
                );
                Err(err.into())
            }
        }
    }

    /// List durable process-wake delivery rows, optionally filtered by state.
    pub async fn wake_deliveries(
        &self,
        state: Option<lash_core::WakeDeliveryState>,
    ) -> Result<Vec<lash_core::WakeDelivery>> {
        self.registry()?
            .list_wake_deliveries(state)
            .await
            .map_err(Into::into)
    }

    /// Summarize delivery states and name blocked groups with their redrive ids.
    pub async fn wake_delivery_report(&self) -> Result<lash_core::WakeDeliveryReport> {
        self.registry()?
            .wake_delivery_report()
            .await
            .map_err(Into::into)
    }

    /// Explicitly return a discarded delivery to the pending lane.
    pub async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<()> {
        self.registry()?
            .redrive_wake_delivery(delivery_id)
            .await
            .map_err(Into::into)
    }

    /// Run one bounded wake-delivery pass immediately.
    pub async fn drive_wake_deliveries(
        &self,
    ) -> Result<lash_core::facade_support::WakeDeliveryDriveReport> {
        let drivers = self.core.work_driver.drivers().await;
        let Some(driver) = drivers._wake else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "wake delivery driver is unavailable in this runtime".to_string(),
            )));
        };
        driver.drive_pending().await.map_err(Into::into)
    }

    /// Record a durable, non-terminal **Abandon Request** on a process (ADR
    /// 0019): a third party's authorization to accept uncertainty about an
    /// owner. This never terminalizes anything itself — the recovery sweep
    /// reconciles it into `Abandoned` only once the owner's lease has lapsed;
    /// the marker stays visible to observers while pending. Returns the process
    /// as observed after the marker is written.
    pub async fn request_abandon(
        &self,
        process_id: &str,
        requested_by: impl Into<String>,
        reason: Option<String>,
    ) -> Result<lash_core::facade_support::ObservedProcess> {
        let request = lash_core::AbandonRequest {
            requested_by: requested_by.into(),
            requested_at_ms: now_epoch_ms(),
            reason,
        };
        self.registry()?
            .request_process_abandon(process_id, request)
            .await?;
        self.get(process_id).await?.ok_or_else(|| {
            EmbedError::Plugin(lash_core::PluginError::Session(format!(
                "process `{process_id}` vanished after recording its abandon request"
            )))
        })
    }
}

/// Host wall-clock epoch milliseconds for facade-issued markers (e.g. the
/// Abandon Request timestamp). The registry stays state-only, so the facade
/// stamps the request time itself. Shared with the session-scoped abandon lever
/// in [`crate::admin`].
pub(crate) fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
