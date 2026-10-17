use super::AdminContext;
use crate::support::{Arc, EmbedError, Result};
use lash_core::ActorContext;
use lash_protocol_types::{ProcessId, SessionId};

#[derive(Clone)]
/// Facade handle for session process administration.
pub struct SessionProcessAdmin {
    pub(crate) context: AdminContext,
    pub(crate) observation_work_limits: lash_trace::ObservationWorkLimits,
    pub(crate) process_work: Arc<dyn lash_core::ProcessWorkSubstrate>,
}
/// Session-scoped view of the global process surface
/// ([`Processes`](crate::process::Processes)).
///
/// This is thin sugar, not a parallel surface (ADR 0014 grill): every read is
/// the global observer pre-filtered by this session's observer scope (what the
/// session may address), and every mutation delegates to the same runtime
/// process path the global surface uses. It speaks the same
/// [`ObservedProcess`](lash_execution_contracts::process::ObservedProcess) vocabulary as
/// [`Processes`](crate::process::Processes); [`start`](Self::start) returns the
/// model-facing handle summary ([`lash_execution_contracts::process::ProcessHandleView`]), the one row
/// type retained for the model/handle contract.
impl SessionProcessAdmin {
    /// Observer-scoped read: the global observer filtered to this session.
    /// One home for the session's read logic — it calls the observer, never
    /// reimplements it.
    async fn list_observed(
        &self,
        filter: &lash_execution_contracts::process::ProcessListFilter,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>> {
        // A registry-less runtime has no processes to address; observe empty
        // rather than erroring, matching the pre-unification session read.
        let Some(observer) = self.process_observer_opt() else {
            return Ok(Vec::new());
        };
        observer
            .list_observed_by(&self.process_observer_scope(), filter)
            .await
            .map_err(Into::into)
    }

    pub async fn start(
        &self,
        request: lash_execution_contracts::process::ProcessStartRequest,
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_execution_contracts::process::ProcessHandleView> {
        let (session_id, processes) = {
            let writer = self.context.runtime.writer();
            let runtime = writer.lock().await;
            (
                SessionId::from(runtime.session_id()),
                runtime.process_service()?,
            )
        };
        let request = scoped_effect_controller
            .key_host_start(request)
            .map_err(EmbedError::Plugin)?;
        let start_key = request.start_key().cloned();
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summary = processes
            .start_from_request(&session_id, request, scope)
            .await
            .map_err(|error| {
                EmbedError::Plugin(match error {
                    lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(error) => {
                        crate::process_admin::host_start_refusal(start_key.as_ref(), error)
                    }
                    lash_execution_contracts::plugin_error::PluginError::Runtime(error) => {
                        crate::process_admin::host_start_refusal(start_key.as_ref(), error.into())
                    }
                    error => error,
                })
            })?;
        Ok(summary)
    }

    /// Live processes this session may address: running, or waiting on a
    /// call, a key, a sleep or another process.
    pub async fn list(&self) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>> {
        self.list_observed(&lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::any_of(
                lash_execution_contracts::process::ProcessStatus::ALL
                    .iter()
                    .copied()
                    .filter(lash_execution_contracts::process::ProcessStatus::is_live),
            ),
            ..lash_execution_contracts::process::ProcessListFilter::default()
        })
        .await
    }

    /// Every process (any status) this session may address.
    pub async fn list_all(
        &self,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>> {
        self.list_observed(&lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            ..lash_execution_contracts::process::ProcessListFilter::default()
        })
        .await
    }

    /// One process this session may address, if present.
    pub async fn get(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_execution_contracts::process::ObservedProcess>> {
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .find(|process| process.process_id == process_id))
    }

    /// Read one durable event page after `from`; the returned continuation
    /// names where the next page starts.
    pub async fn events(
        &self,
        from: crate::process_history::ProcessHistoryContinuation,
        limit: std::num::NonZeroUsize,
        mode: lash_execution_contracts::process::ProcessEventQueryMode,
    ) -> Result<crate::process_history::ProcessEventsRead> {
        let Some(registry) = self.context.runtime.observe().process_registry.clone() else {
            let events = match mode {
                lash_execution_contracts::process::ProcessEventQueryMode::Full => {
                    lash_execution_contracts::process::ProcessEventPageEvents::Full(Vec::new())
                }
                lash_execution_contracts::process::ProcessEventQueryMode::Lite => {
                    lash_execution_contracts::process::ProcessEventPageEvents::Lite(Vec::new())
                }
            };
            return Ok(crate::process_history::ProcessEventsRead {
                outcome: lash_execution_contracts::process::ProcessEventReadOutcome::Retained(
                    lash_execution_contracts::process::ProcessEventPage {
                        events,
                        more: lash_execution_contracts::process::ProcessEventPageMore::Complete,
                    },
                ),
                next: from,
            });
        };
        Ok(crate::process_history::read_events(&registry, from, limit, mode).await?)
    }

    pub async fn await_output(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_execution_contracts::process::ProcessAwaitOutput> {
        let process_work = &self.process_work;
        let process_id = self
            .process_registry()?
            .require_process_id(process_id)
            .await?;
        loop {
            match process_work.await_process_terminal(&process_id).await? {
                lash_core::ProcessTerminalWait::Terminal(output) => return Ok(output),
                lash_core::ProcessTerminalWait::Reattach => {}
            }
        }
    }

    /// Requests cancellation of a process.
    pub async fn cancel(
        &self,
        process_id: &ProcessId,
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_execution_contracts::process::ProcessCancelReceipt> {
        let (owner, processes) = {
            let writer = self.context.runtime.writer();
            let runtime = writer.lock().await;
            (
                lash_core::RuntimeOwner::Session(SessionId::from(runtime.session_id())),
                runtime.process_service()?,
            )
        };
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        processes
            .validate_visible(&owner, std::slice::from_ref(process_id), scope.clone())
            .await
            .map_err(EmbedError::Plugin)?;
        let summary = processes
            .cancel(&owner, process_id, scope)
            .await
            .and_then(lash_execution_contracts::process::ProcessCancelReceipt::from_observed)
            .map_err(EmbedError::Plugin)?;
        Ok(summary)
    }

    /// Requests cancellation of every process in the session.
    pub async fn cancel_all(
        &self,
        scoped_effect_controller: ActorContext,
    ) -> Result<Vec<lash_execution_contracts::process::ProcessCancelReceipt>> {
        let (session_id, processes) = {
            let writer = self.context.runtime.writer();
            let runtime = writer.lock().await;
            (
                SessionId::from(runtime.session_id()),
                runtime.process_service()?,
            )
        };
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summaries = processes
            .cancel_all_visible(&session_id, scope)
            .await
            .map_err(EmbedError::Plugin)?;
        Ok(summaries)
    }

    /// Re-homes addressability only; the process itself is global.
    pub async fn transfer(
        &self,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scoped_effect_controller: ActorContext,
    ) -> Result<()> {
        let (session_id, processes) = {
            let writer = self.context.runtime.writer();
            let runtime = writer.lock().await;
            (
                SessionId::from(runtime.session_id()),
                runtime.process_service()?,
            )
        };
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        processes
            .transfer(&session_id, to_session_id, process_ids, scope)
            .await
            .map_err(EmbedError::Plugin)
    }

    fn process_registry(
        &self,
    ) -> Result<Arc<dyn lash_execution_contracts::process::ProcessRegistry>> {
        self.context
            .runtime
            .observe()
            .process_registry
            .clone()
            .ok_or_else(|| {
                EmbedError::Plugin(
                    lash_execution_contracts::plugin_error::PluginError::Session(
                        "process registry is unavailable in this runtime".to_string(),
                    ),
                )
            })
    }

    /// An observer over the session's process registry, or `None` when this
    /// runtime has no registry. Session-scoped reads use this so a registry-less
    /// runtime observes an empty process set rather than erroring, matching the
    /// pre-unification `list_process_handles` behavior.
    fn process_observer_opt(
        &self,
    ) -> Option<lash_execution_contracts::process::ProcessWorkObserver> {
        let observation = self.context.runtime.observe();
        observation.process_registry.clone().map(|registry| {
            lash_execution_contracts::process::ProcessWorkObserver::new(registry)
                .with_actor_parks(Arc::clone(observation.effect_host.backend().durable()))
                .with_read_attempts(self.context.target.observer_pacing().snapshot_read_attempts)
                .with_work_limits(self.observation_work_limits)
        })
    }

    /// Observer edges are session-scoped and deliberately frame-less.
    fn process_observer_scope(&self) -> lash_execution_contracts::process::SessionScope {
        self.context.runtime.observe().process_scope()
    }
}
