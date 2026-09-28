use super::*;

/// Session-scoped view of the global process surface
/// ([`Processes`](crate::process::Processes)).
///
/// This is thin sugar, not a parallel surface (ADR 0019 grill): every read is
/// the global observer pre-filtered by this session's observer scope (what the
/// session may address), and every mutation delegates to the same runtime
/// process path the global surface uses. It speaks the same
/// [`ObservedProcess`](lash_core::facade_support::ObservedProcess) vocabulary as
/// [`Processes`](crate::process::Processes); [`start`](Self::start) returns the
/// model-facing handle summary ([`lash_core::ProcessHandleView`]), the one row
/// type retained for the model/handle contract.
impl SessionProcessAdmin {
    /// Observer-scoped read: the global observer filtered to this session.
    /// One home for the session's read logic — it calls the observer, never
    /// reimplements it.
    async fn list_observed(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        // A registry-less runtime has no processes to address; observe empty
        // rather than erroring, matching the pre-unification session read.
        let Some(observer) = self.control.process_observer_opt() else {
            return Ok(Vec::new());
        };
        observer
            .list_observed_by(&self.control.process_observer_scope(), filter)
            .await
            .map_err(Into::into)
    }

    pub async fn start(
        &self,
        request: lash_core::ProcessStartRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessHandleView> {
        self.control
            .start_process(request, scoped_effect_controller)
            .await
    }

    /// Running processes this session may address.
    pub async fn list(&self) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.list_observed(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::any_of([lash_core::ProcessStatus::Running]),
            ..lash_core::ProcessListFilter::default()
        })
        .await
    }

    /// Every process (any status) this session may address.
    pub async fn list_all(&self) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.list_observed(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
    }

    /// One process this session may address, if present.
    pub async fn get(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_core::facade_support::ObservedProcess>> {
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .find(|process| process.process_id == process_id))
    }

    /// Read one durable event page from a process cursor, or from the start
    /// of the lifetime a process id currently names. This session routes no
    /// live observation, so a cursor it mints carries the unrouted epoch.
    pub async fn events(
        &self,
        from: crate::process_observation::ProcessEventsFrom,
        limit: std::num::NonZeroUsize,
        mode: lash_core::ProcessEventQueryMode,
    ) -> Result<crate::process_observation::ProcessEventsRead> {
        let Some(registry) = self.control.runtime.observe().process_registry.clone() else {
            let events = match mode {
                lash_core::ProcessEventQueryMode::Full => {
                    lash_core::ProcessEventPageEvents::Full(Vec::new())
                }
                lash_core::ProcessEventQueryMode::Lite => {
                    lash_core::ProcessEventPageEvents::Lite(Vec::new())
                }
            };
            return Ok(crate::process_observation::ProcessEventsRead {
                outcome: lash_core::ProcessEventReadOutcome::Retained(
                    lash_core::ProcessEventPage {
                        events,
                        more: lash_core::ProcessEventPageMore::Complete,
                    },
                ),
                cursor: None,
            });
        };
        Ok(crate::process_observation::read_events(&registry, None, from, limit, mode).await?)
    }

    pub async fn await_output(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core::ProcessAwaitOutput> {
        self.control.await_process_output(process_id).await
    }

    /// Delivers a signal to a process.
    pub async fn signal(
        &self,
        process_id: &ProcessId,
        signal_name: impl Into<String>,
        signal_id: impl Into<String>,
        payload: serde_json::Value,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEvent> {
        self.control
            .signal_process(
                process_id,
                signal_name.into(),
                signal_id.into(),
                payload,
                scoped_effect_controller,
            )
            .await
    }

    /// Requests cancellation of a process.
    pub async fn cancel(
        &self,
        process_id: &ProcessId,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessCancelReceipt> {
        self.control
            .cancel_process(process_id, scoped_effect_controller)
            .await
    }

    /// Requests cancellation of every process in the session.
    pub async fn cancel_all(
        &self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<Vec<lash_core::ProcessCancelReceipt>> {
        self.control
            .cancel_visible_processes(scoped_effect_controller)
            .await
    }

    /// Re-homes addressability only; the process itself is global.
    pub async fn transfer(
        &self,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<()> {
        self.control
            .transfer_process_handles(to_session_id, process_ids, scoped_effect_controller)
            .await
    }
}
