use super::*;
use lash::SessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CronSessionDisposition {
    Live,
    Retired,
    Unknown,
}

impl CronSessionDisposition {
    pub(super) fn journal_value(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Retired => "retired",
            Self::Unknown => "unknown",
        }
    }

    pub(super) fn from_journal_value(value: &str) -> HandlerResult<Self> {
        match value {
            "live" => Ok(Self::Live),
            "retired" => Ok(Self::Retired),
            "unknown" => Ok(Self::Unknown),
            _ => Err(TerminalError::new(format!(
                "invalid journaled cron session disposition `{value}`"
            ))
            .into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CronRegistrationDisposition {
    Enabled,
    Disabled,
    Absent,
}

impl CronRegistrationDisposition {
    pub(super) fn journal_value(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Absent => "absent",
        }
    }

    pub(super) fn from_journal_value(value: &str) -> HandlerResult<Self> {
        match value {
            "enabled" => Ok(Self::Enabled),
            "disabled" => Ok(Self::Disabled),
            "absent" => Ok(Self::Absent),
            _ => Err(TerminalError::new(format!(
                "invalid journaled cron registration disposition `{value}`"
            ))
            .into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CronTickBasis {
    pub(super) session: CronSessionDisposition,
    pub(super) registration: CronRegistrationDisposition,
}

impl CronTickBasis {
    pub(super) fn journal_value(self) -> String {
        format!(
            "{}:{}",
            self.session.journal_value(),
            self.registration.journal_value()
        )
    }

    pub(super) fn from_journal_value(value: &str) -> HandlerResult<Self> {
        match value.split_once(':') {
            Some((session, registration)) => Ok(Self {
                session: CronSessionDisposition::from_journal_value(session)?,
                registration: CronRegistrationDisposition::from_journal_value(registration)?,
            }),
            // Pre-FIG-1071 invocations journaled the session axis alone; only the
            // session arm could cancel, so a live legacy value keeps ticking.
            None => Ok(Self {
                session: CronSessionDisposition::from_journal_value(value)?,
                registration: CronRegistrationDisposition::Enabled,
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum CronTick {
    Cancel { reason: &'static str, trace: Value },
    Run,
}

pub(super) fn cron_tick_decision(
    basis: CronTickBasis,
    state: &WorkbenchCronState,
    job_key: &str,
) -> CronTick {
    let (decision_basis, session_state, registration_state, reason) = match basis.session {
        CronSessionDisposition::Live => match basis.registration {
            CronRegistrationDisposition::Enabled => return CronTick::Run,
            CronRegistrationDisposition::Disabled => (
                "registration_record_disabled",
                "live",
                Some("disabled"),
                "registration_disabled",
            ),
            CronRegistrationDisposition::Absent => (
                "registration_record_absent",
                "live",
                Some("absent"),
                "registration_absent",
            ),
        },
        CronSessionDisposition::Retired => (
            "deleted_session_tombstone",
            "retired",
            None,
            "session_retired",
        ),
        CronSessionDisposition::Unknown => (
            "session_store_meta_absent",
            "unknown",
            None,
            "session_absent",
        ),
    };
    let mut trace = json!({
        "job_key": job_key,
        "job_session_id": state.request.session_id,
        "decision_basis": decision_basis,
        "session_state": session_state,
        "reason": reason,
    });
    if let Some(registration_state) = registration_state {
        trace["registration_state"] = json!(registration_state);
    }
    CronTick::Cancel { reason, trace }
}

pub(super) async fn cron_session_disposition(
    core: &lash::LashCore,
    session_id: &SessionId,
) -> Result<CronSessionDisposition, HandlerError> {
    if core
        .session_was_deleted(session_id)
        .await
        .map_err(classified_embed_handler_error)?
    {
        return Ok(CronSessionDisposition::Retired);
    }
    if core
        .session_exists(session_id)
        .await
        .map_err(classified_embed_handler_error)?
    {
        Ok(CronSessionDisposition::Live)
    } else {
        Ok(CronSessionDisposition::Unknown)
    }
}

pub(super) async fn cron_registration_disposition(
    state: &AppState,
    session_id: &SessionId,
    source_key: &str,
) -> Result<CronRegistrationDisposition, HandlerError> {
    let mut filter = lash::triggers::TriggerSubscriptionFilter::for_session(session_id);
    filter.source_type = Some(CRON_SCHEDULE_SOURCE_TYPE.to_string());
    filter.source_key = Some(source_key.to_string());
    let registrations = state
        .trigger_store
        .list_subscriptions(filter)
        .await
        .map_err(classified_plugin_handler_error)?;
    if registrations
        .iter()
        .any(|registration| registration.enabled)
    {
        Ok(CronRegistrationDisposition::Enabled)
    } else if registrations.is_empty() {
        Ok(CronRegistrationDisposition::Absent)
    } else {
        Ok(CronRegistrationDisposition::Disabled)
    }
}

fn cron_tick_outcome_key(job_key: &str, scheduled_for: &str) -> String {
    format!("workbench-cron-outcome:{job_key}:{scheduled_for}")
}

async fn record_cron_tick_outcome(
    state: AppState,
    request: WorkbenchCronRequest,
    scheduled_for: String,
    outcome: lash::triggers::TriggerOccurrenceOutcome,
    controller: &lash_restate::RestateRuntimeEffectController<'_, ObjectContext<'_>>,
) -> HandlerResult<String> {
    let scoped_effect_controller = controller
        .scoped_effect_controller(lash::runtime::ExecutionScope::runtime_operation(format!(
            "cron-outcome:{}:{scheduled_for}",
            controller.context().key()
        )))
        .map_err(|err| HandlerError::from(TerminalError::new(err.to_string())))?;
    record_cron_tick_outcome_with_effect_controller(
        state,
        request,
        scheduled_for,
        controller.context().key(),
        outcome,
        scoped_effect_controller,
    )
    .await
}

#[async_trait::async_trait]
pub(super) trait CronTickCancelSurface: Sync {
    async fn record_trace(&self, session_id: SessionId, trace: Value) -> HandlerResult<()>;

    async fn record_outcome(
        &self,
        request: WorkbenchCronRequest,
        scheduled_for: String,
        outcome: lash::triggers::TriggerOccurrenceOutcome,
    ) -> HandlerResult<String>;

    fn clear_cron_state(&self);
}

pub(super) struct RestateCronTickCancelSurface<'run, 'ctx> {
    app_state: AppState,
    controller: &'run lash_restate::RestateRuntimeEffectController<'ctx, ObjectContext<'ctx>>,
}

impl<'run, 'ctx> RestateCronTickCancelSurface<'run, 'ctx> {
    pub(crate) fn new(
        app_state: AppState,
        controller: &'run lash_restate::RestateRuntimeEffectController<'ctx, ObjectContext<'ctx>>,
    ) -> Self {
        Self {
            app_state,
            controller,
        }
    }
}

#[async_trait::async_trait]
impl CronTickCancelSurface for RestateCronTickCancelSurface<'_, '_> {
    async fn record_trace(&self, session_id: SessionId, trace: Value) -> HandlerResult<()> {
        journaled_workbench_trace(
            self.controller.context(),
            self.app_state.clone(),
            session_id,
            "cron.restate.zombie_cancelled",
            trace,
            "workbench-cron:trace-cancelled",
        )
        .await
    }

    async fn record_outcome(
        &self,
        request: WorkbenchCronRequest,
        scheduled_for: String,
        outcome: lash::triggers::TriggerOccurrenceOutcome,
    ) -> HandlerResult<String> {
        record_cron_tick_outcome(
            self.app_state.clone(),
            request,
            scheduled_for,
            outcome,
            self.controller,
        )
        .await
    }

    fn clear_cron_state(&self) {
        self.controller.context().clear(CRON_STATE_KEY);
    }
}

async fn cancel_observed_cron_tick(
    surface: &impl CronTickCancelSurface,
    cron_state: &WorkbenchCronState,
    reason: &'static str,
    trace: Value,
) -> HandlerResult<()> {
    surface
        .record_trace(cron_state.request.session_id.clone(), trace)
        .await?;
    surface
        .record_outcome(
            cron_state.request.clone(),
            cron_state.next_execution_time.clone(),
            lash::triggers::TriggerOccurrenceOutcome::Dropped {
                reason: reason.to_string(),
            },
        )
        .await?;
    surface.clear_cron_state();
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CronTickHandling {
    Cancelled,
    Run,
}

pub(super) async fn handle_observed_cron_tick(
    surface: &impl CronTickCancelSurface,
    cron_state: &WorkbenchCronState,
    decision: CronTick,
) -> HandlerResult<CronTickHandling> {
    match decision {
        CronTick::Cancel { reason, trace } => {
            cancel_observed_cron_tick(surface, cron_state, reason, trace).await?;
            Ok(CronTickHandling::Cancelled)
        }
        CronTick::Run => Ok(CronTickHandling::Run),
    }
}

pub(super) async fn record_cron_tick_outcome_with_effect_controller(
    state: AppState,
    request: WorkbenchCronRequest,
    scheduled_for: String,
    job_key: &str,
    outcome: lash::triggers::TriggerOccurrenceOutcome,
    scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
) -> HandlerResult<String> {
    let idempotency_key = cron_tick_outcome_key(job_key, &scheduled_for);
    let report = state
        .core
        .triggers()
        .emit(
            lash::triggers::TriggerOccurrenceRequest::new(
                CRON_SCHEDULE_SOURCE_TYPE,
                request.source_key,
                json!({ "scheduled_for": scheduled_for }),
                idempotency_key,
            )
            .with_source(json!({
                "expr": request.expr,
                "tz": request.tz,
            }))
            .for_session(request.session_id)
            .with_outcome(outcome),
            scoped_effect_controller,
        )
        .await
        .map_err(classified_embed_handler_error)?;
    debug_assert!(report.deliveries.is_empty());
    Ok(report.occurrence_id)
}
