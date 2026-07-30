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

    fn make_observer(&self) -> Result<lash_core::ProcessWorkObserver> {
        Ok(lash_core::ProcessWorkObserver::new(self.registry()?))
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
            .controller()
            .execute_effect(
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
            driver.claim_and_run_pending("admin_process_start").await?;
        }
        Ok(*record)
    }

    pub async fn list(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::ObservedProcess>> {
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
    ) -> Result<Vec<lash_core::ObservedProcess>> {
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
    ) -> Result<Vec<lash_core::ObservedProcess>> {
        self.make_observer()?
            .list_originated_by(session_scope, filter)
            .await
            .map_err(Into::into)
    }

    pub async fn get(&self, process_id: &str) -> Result<Option<lash_core::ObservedProcess>> {
        self.make_observer()?
            .process(process_id)
            .await
            .map_err(Into::into)
    }

    pub async fn events(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<lash_core::ObservedProcessEvent>> {
        self.make_observer()?
            .events_after(process_id, after_sequence)
            .await
            .map_err(Into::into)
    }

    pub async fn await_output(&self, process_id: &str) -> Result<lash_core::ProcessAwaitOutput> {
        if let Some(driver) = self.core.env.process_work_driver.as_ref() {
            return driver.await_terminal(process_id).await.map_err(Into::into);
        }
        lash_core::ProcessAwaiter::polling(self.registry()?)
            .await_terminal(process_id)
            .await
            .map_err(Into::into)
    }

    pub async fn cancel(
        &self,
        process_id: &str,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessCancelSummary> {
        let command = lash_core::ProcessCommand::Cancel {
            process_id: process_id.to_string(),
            reason: Some("requested by host".to_string()),
        };
        let outcome = self
            .run_command(command, scoped_effect_controller.clone())
            .await?;
        let lash_core::ProcessEffectOutcome::Cancel { record } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process cancel returned the wrong outcome".to_string(),
            )));
        };
        Ok(lash_core::ProcessCancelSummary::from_record(*record))
    }

    pub async fn signal(
        &self,
        process_id: &str,
        signal_name: impl Into<String>,
        signal_id: impl Into<String>,
        request: lash_core::ProcessEventAppendRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEvent> {
        let signal_name = signal_name.into();
        let event_type = request.event_type.clone();
        let payload = request.payload.clone();
        let command = lash_core::ProcessCommand::Signal {
            process_id: process_id.to_string(),
            signal_name: signal_name.clone(),
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
        let registry = self.registry()?;
        let waiting_ordinal =
            registry
                .get_process(process_id)
                .await?
                .and_then(|record| match record.wait {
                    Some(lash_core::WaitState {
                        kind:
                            lash_core::WaitKind::Signal {
                                name,
                                event_type: wait_event_type,
                                ordinal,
                                ..
                            },
                        ..
                    }) if name == signal_name && wait_event_type == event_type => Some(ordinal),
                    _ => None,
                });
        let ordinal = match waiting_ordinal {
            Some(ordinal) => ordinal,
            None => {
                registry
                    .count_events_through(process_id, &event_type, event.sequence)
                    .await?
            }
        };
        if ordinal > 0 {
            let key = scoped_effect_controller
                .controller()
                .await_event_key(
                    &lash_core::ExecutionScope::process(process_id),
                    lash_core::AwaitEventWaitIdentity::process_signal(
                        process_id,
                        &signal_name,
                        ordinal,
                    ),
                )
                .await
                .map_err(|err| {
                    EmbedError::Plugin(lash_core::PluginError::Session(err.to_string()))
                })?;
            let _ = scoped_effect_controller
                .controller()
                .resolve_await_event(&key, lash_core::Resolution::Ok(payload))
                .await
                .map_err(|err| {
                    EmbedError::Plugin(lash_core::PluginError::Session(err.to_string()))
                })?;
        }
        Ok(*event)
    }

    pub async fn session_snapshot(
        &self,
        session_id: impl Into<String>,
    ) -> Result<lash_core::ProcessWorkSnapshot> {
        self.make_observer()?
            .snapshot_for_session(session_id)
            .await
            .map_err(Into::into)
    }

    pub fn observer(&self) -> Result<lash_core::ProcessWorkObserver> {
        self.make_observer()
    }

    /// Cancel every currently-running process. A host-wide lever; for a
    /// session-scoped stop use [`SessionProcessAdmin::cancel_all`](crate::admin::SessionProcessAdmin::cancel_all).
    pub async fn cancel_all(
        &self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<Vec<lash_core::ProcessCancelSummary>> {
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

    /// Host-scheduled retention lever (ADR 0017): physically delete terminal
    /// process rows (and their events, observer edges, leases) older than
    /// `cutoff_epoch_ms`, returning what was reclaimed. Non-terminal rows are
    /// never touched. Lash exposes no finite maximum waiter lifetime: the host
    /// must retain rows beyond every still-replayable await, and a later await
    /// after pruning receives the typed `ProcessNoLongerRetained` outcome.
    pub async fn prune(&self, cutoff_epoch_ms: u64) -> Result<lash_core::ProcessPruneReport> {
        let registry = self.registry()?;
        let candidates = registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await?;
        for process in candidates
            .into_iter()
            .filter(|process| process.is_terminal() && process.updated_at_ms < cutoff_epoch_ms)
        {
            self.core
                .env
                .core
                .control
                .effect_host
                .retire_effect_journal(lash_core::EffectJournalRetirement::process(
                    process.id.clone(),
                ))
                .await?;
        }
        registry
            .prune_terminal_processes(
                cutoff_epoch_ms,
                None,
                lash_core::ProjectionWatermark::NoProjector,
            )
            .await
            .map_err(Into::into)
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
    pub async fn drive_wake_deliveries(&self) -> Result<lash_core::WakeDeliveryDriveReport> {
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
    ) -> Result<lash_core::ObservedProcess> {
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
