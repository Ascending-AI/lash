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

async fn cancel_observed_cron_tick(
    app_state: AppState,
    cron_state: &WorkbenchCronState,
    reason: &'static str,
    trace: Value,
    controller: &lash_restate::RestateRuntimeEffectController<'_, ObjectContext<'_>>,
) -> HandlerResult<()> {
    journaled_workbench_trace(
        controller.context(),
        app_state.clone(),
        cron_state.request.session_id.clone(),
        "cron.restate.zombie_cancelled",
        trace,
        "workbench-cron:trace-cancelled",
    )
    .await?;
    record_cron_tick_outcome(
        app_state,
        cron_state.request.clone(),
        cron_state.next_execution_time.clone(),
        lash::triggers::TriggerOccurrenceOutcome::Dropped {
            reason: reason.to_string(),
        },
        controller,
    )
    .await?;
    controller.context().clear(CRON_STATE_KEY);
    Ok(())
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
