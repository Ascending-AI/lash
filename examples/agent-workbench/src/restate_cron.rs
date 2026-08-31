#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CronSessionDisposition {
    Live,
    Retired,
    Unknown,
}

impl CronSessionDisposition {
    fn journal_value(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Retired => "retired",
            Self::Unknown => "unknown",
        }
    }

    fn from_journal_value(value: &str) -> HandlerResult<Self> {
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

#[derive(Clone, Debug, PartialEq)]
enum CronTick {
    Cancel { reason: &'static str, trace: Value },
    Run,
}

fn cron_tick_decision(
    disposition: CronSessionDisposition,
    state: &WorkbenchCronState,
    job_key: &str,
) -> CronTick {
    let (decision_basis, session_state, reason) = match disposition {
        CronSessionDisposition::Live => return CronTick::Run,
        CronSessionDisposition::Retired => {
            ("deleted_session_tombstone", "retired", "session_retired")
        }
        CronSessionDisposition::Unknown => {
            ("session_store_meta_absent", "unknown", "session_absent")
        }
    };
    CronTick::Cancel {
        reason,
        trace: json!({
            "job_key": job_key,
            "job_session_id": state.request.session_id,
            "decision_basis": decision_basis,
            "session_state": session_state,
            "reason": reason,
        }),
    }
}

async fn cron_session_disposition(
    core: &lash::LashCore,
    session_id: &str,
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
trait CronTickCancelSurface: Sync {
    async fn record_trace(
        &self,
        session_id: String,
        trace: Value,
    ) -> HandlerResult<()>;

    async fn record_outcome(
        &self,
        request: WorkbenchCronRequest,
        scheduled_for: String,
        outcome: lash::triggers::TriggerOccurrenceOutcome,
    ) -> HandlerResult<String>;

    fn clear_cron_state(&self);
}

struct RestateCronTickCancelSurface<'run, 'ctx> {
    app_state: AppState,
    controller: &'run lash_restate::RestateRuntimeEffectController<'ctx, ObjectContext<'ctx>>,
}

impl<'run, 'ctx> RestateCronTickCancelSurface<'run, 'ctx> {
    fn new(
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
    async fn record_trace(
        &self,
        session_id: String,
        trace: Value,
    ) -> HandlerResult<()> {
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
enum CronTickHandling {
    Cancelled,
    Run,
}

async fn handle_observed_cron_tick(
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

async fn record_cron_tick_outcome_with_effect_controller(
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
