async fn sync_cron_jobs_with_context(
    state: &AppState,
    ctx: &WorkflowContext<'_>,
    session_id: &str,
    reason: &str,
) -> HandlerResult<()> {
    sync_cron_jobs(
        state,
        &WorkflowCronJobSyncSurface { ctx },
        session_id,
        reason,
        classified_embed_handler_error,
    )
    .await
}

#[async_trait::async_trait]
trait CronJobSyncSurface {
    type Error;

    async fn upsert(
        &self,
        job_key: &str,
        request: WorkbenchCronRequest,
    ) -> Result<WorkbenchCronInfo, Self::Error>;

    async fn cancel(&self, job_key: &str) -> Result<(), Self::Error>;
}

struct WorkflowCronJobSyncSurface<'a, 'ctx> {
    ctx: &'a WorkflowContext<'ctx>,
}

#[async_trait::async_trait]
impl CronJobSyncSurface for WorkflowCronJobSyncSurface<'_, '_> {
    type Error = HandlerError;

    async fn upsert(
        &self,
        job_key: &str,
        request: WorkbenchCronRequest,
    ) -> Result<WorkbenchCronInfo, Self::Error> {
        let Json(info) = self
            .ctx
            .object_client::<WorkbenchCronJobClient>(job_key.to_string())
            .upsert(Json(request))
            .call()
            .await?;
        Ok(info)
    }

    async fn cancel(&self, job_key: &str) -> Result<(), Self::Error> {
        self.ctx
            .object_client::<WorkbenchCronJobClient>(job_key.to_string())
            .cancel()
            .call()
            .await?;
        Ok(())
    }
}

struct IngressCronJobSyncSurface {
    client: lash_restate::RestateIngressClient,
}

#[async_trait::async_trait]
impl CronJobSyncSurface for IngressCronJobSyncSurface {
    type Error = AppError;

    async fn upsert(
        &self,
        job_key: &str,
        request: WorkbenchCronRequest,
    ) -> Result<WorkbenchCronInfo, Self::Error> {
        self.client
            .call_object_json("WorkbenchCronJob", job_key, "upsert", &request)
            .await
            .map_err(|err| AppError::internal(format!("Restate cron sync failed: {err}")))
    }

    async fn cancel(&self, job_key: &str) -> Result<(), Self::Error> {
        self.client
            .call_object_empty("WorkbenchCronJob", job_key, "cancel")
            .await
            .map_err(|err| AppError::internal(format!("Restate cron sync failed: {err}")))
    }
}

/// Completes the cron half of one committed trigger enable/disable mutation
/// before its HTTP route may return success. The pre-mutation record keeps a
/// disabled job cancellable even when this process did not originally arm it.
pub(crate) async fn sync_cron_jobs_after_trigger_mutation(
    state: &AppState,
    session_id: &str,
    reason: &str,
    affected_registration: &lash::triggers::TriggerSubscriptionRecord,
) -> Result<(), AppError> {
    if affected_registration.source_type != CRON_SCHEDULE_SOURCE_TYPE {
        return Ok(());
    }
    state
        .restate_cron_job_keys
        .lock_recover()
        .entry(session_id.to_string())
        .or_default()
        .insert(cron_job_key(session_id, &affected_registration.source_key));
    let surface = IngressCronJobSyncSurface {
        client: lash_restate::RestateIngressClient::new(
            lash_restate::RestateConnection::with_client(
                &state.restate_ingress_url,
                state.restate_http.clone(),
            ),
        ),
    };
    sync_cron_jobs(state, &surface, session_id, reason, AppError::runtime).await
}

/// Cancels the cron job named by the registration before its durable trigger
/// delete. A failed cancellation therefore leaves the registration available
/// for a later retry instead of orphaning a live external job.
pub(crate) async fn cancel_cron_job_before_trigger_delete(
    state: &AppState,
    session_id: &str,
    affected_registration: &lash::triggers::TriggerSubscriptionRecord,
) -> Result<(), AppError> {
    if affected_registration.source_type != CRON_SCHEDULE_SOURCE_TYPE {
        return Ok(());
    }
    let job_key = cron_job_key(session_id, &affected_registration.source_key);
    let surface = IngressCronJobSyncSurface {
        client: lash_restate::RestateIngressClient::new(
            lash_restate::RestateConnection::with_client(
                &state.restate_ingress_url,
                state.restate_http.clone(),
            ),
        ),
    };
    surface.cancel(&job_key).await?;
    state.trace_for_session(
        session_id,
        "cron.restate.sync_cancelled",
        json!({
            "reason": "trigger_deleted",
            "job_key": job_key,
            "job_session_id": session_id,
        }),
    );
    let mut known = state.restate_cron_job_keys.lock_recover();
    let remove_session = known.get_mut(session_id).is_some_and(|keys| {
        keys.remove(&job_key);
        keys.is_empty()
    });
    if remove_session {
        known.remove(session_id);
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct CronSyncPlan {
    upserts: BTreeMap<String, WorkbenchCronRequest>,
    cancels: BTreeSet<String>,
}

fn cron_sync_plan(
    session_id: &str,
    registrations: &[lash::triggers::TriggerRegistration],
    mut known: BTreeSet<String>,
) -> CronSyncPlan {
    let mut upserts = BTreeMap::new();
    for registration in registrations {
        let job_key = cron_job_key(session_id, &registration.source_key);
        let request = match cron_request_from_registration(session_id, registration) {
            Ok((_job_key, request)) => request,
            Err(_) => {
                known.insert(job_key);
                continue;
            }
        };
        known.insert(job_key.clone());
        if registration.enabled {
            upserts.entry(job_key).or_insert(request);
        }
    }
    let active = upserts.keys().cloned().collect::<BTreeSet<_>>();
    let cancels = known.difference(&active).cloned().collect();
    CronSyncPlan { upserts, cancels }
}

async fn sync_cron_jobs<S, Classify>(
    state: &AppState,
    surface: &S,
    session_id: &str,
    reason: &str,
    classify_embed_error: Classify,
) -> Result<(), S::Error>
where
    S: CronJobSyncSurface + Sync,
    Classify: Fn(lash::EmbedError) -> S::Error,
{
    #[cfg(test)]
    crate::tests::arm_registered_session_open_admission_gate(session_id, reason);
    let session = state
        .open_session(session_id)
        .await
        .map_err(&classify_embed_error)?;
    let registrations = session
        .triggers()
        .by_source_type(CRON_SCHEDULE_SOURCE_TYPE)
        .await
        .map_err(&classify_embed_error)?;
    let known = state
        .restate_cron_job_keys
        .lock_recover()
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    for registration in &registrations {
        if let Err(err) = cron_request_from_registration(session_id, registration) {
            state.trace_for_session(
                session_id,
                "cron.restate.sync_invalid",
                json!({
                    "reason": reason,
                    "subscription_key": registration.subscription_key,
                    "error": err,
                }),
            );
        }
    }
    let CronSyncPlan { upserts, cancels } = cron_sync_plan(session_id, &registrations, known);
    let mut active = BTreeSet::new();
    for (job_key, request) in upserts {
        let info = surface.upsert(&job_key, request).await?;
        state.trace_for_session(
            session_id,
            "cron.restate.sync_upserted",
            json!({
                "reason": reason,
                "job_key": job_key,
                // The registration's own session, so every cron trace record can
                // be gated on the same payload field (the run/cancel records
                // carry it too) instead of splitting between payload and trace
                // context (FIG-1018 review).
                "job_session_id": session_id,
                "next_execution_time": info.next_execution_time,
                "next_execution_id": info.next_execution_id,
            }),
        );
        active.insert(job_key);
    }
    for stale in cancels {
        surface.cancel(&stale).await?;
        state.trace_for_session(
            session_id,
            "cron.restate.sync_cancelled",
            json!({
                "reason": reason,
                "job_key": stale,
                "job_session_id": session_id,
            }),
        );
    }
    state
        .restate_cron_job_keys
        .lock_recover()
        .insert(session_id.to_string(), active);
    Ok(())
}
/// Idempotency key for one cron tick's trigger occurrence.
fn cron_occurrence_key(job_key: &str, fired_at: &str) -> String {
    format!("workbench-cron:{job_key}:{fired_at}")
}

async fn emit_cron_occurrence(
    state: AppState,
    request: WorkbenchCronRequest,
    fired_at: String,
    controller: &lash_restate::RestateRuntimeEffectController<'_, ObjectContext<'_>>,
) -> HandlerResult<Json<CronEmitReport>> {
    let scoped_effect_controller = controller
        .scoped_effect_controller(lash::runtime::ExecutionScope::runtime_operation(format!(
            "cron:{}:{fired_at}",
            controller.context().key()
        )))
        .map_err(|err| HandlerError::from(TerminalError::new(err.to_string())))?;
    emit_cron_occurrence_with_effect_controller(
        state,
        request,
        fired_at,
        controller.context().key(),
        scoped_effect_controller,
    )
    .await
}

async fn emit_cron_occurrence_with_effect_controller(
    state: AppState,
    request: WorkbenchCronRequest,
    fired_at: String,
    job_key: &str,
    scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
) -> HandlerResult<Json<CronEmitReport>> {
    let report = state
        .core
        .triggers()
        .emit(
            lash::triggers::TriggerOccurrenceRequest::new(
                CRON_SCHEDULE_SOURCE_TYPE,
                request.source_key.clone(),
                json!({"fired_at": fired_at}),
                cron_occurrence_key(job_key, &fired_at),
            )
            .with_source(json!({"expr": request.expr, "tz": request.tz})),
            scoped_effect_controller,
        )
        .await
        .map_err(classified_embed_handler_error)?;
    Ok(Json(CronEmitReport {
        started_process_ids: report.started_process_ids(),
    }))
}

async fn schedule_next(
    ctx: &ObjectContext<'_>,
    request: WorkbenchCronRequest,
    now: DateTime<Utc>,
    last_fired_at: Option<String>,
) -> HandlerResult<WorkbenchCronState> {
    let next = next_cron_time(&request.expr, request.tz.as_deref(), now)
        .map_err(|err| HandlerError::from(TerminalError::new(err)))?;
    let delay = next
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(0));
    let handle = ctx.object_client::<WorkbenchCronJobClient>(ctx.key()).run().send_after(delay);
    let next_execution_id = handle.await?.invocation_id().to_owned();
    let state = WorkbenchCronState {
        request,
        next_execution_time: next.to_rfc3339(),
        next_execution_id,
        last_fired_at,
    };
    ctx.set(CRON_STATE_KEY, Json(state.clone()));
    Ok(state)
}

async fn cancel_stored_execution(ctx: &ObjectContext<'_>) -> HandlerResult<()> {
    if let Some(Json(existing)) = ctx.get::<Json<WorkbenchCronState>>(CRON_STATE_KEY).await? {
        ctx.invocation_handle(existing.next_execution_id).cancel();
    }
    Ok(())
}

async fn journaled_now(
    ctx: &ObjectContext<'_>,
    name: &'static str,
) -> HandlerResult<DateTime<Utc>> {
    let now = ctx
        .run(|| async { Ok::<_, HandlerError>(Utc::now().to_rfc3339()) })
        .name(name)
        .await?;
    DateTime::parse_from_rfc3339(&now)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|err| TerminalError::new(err.to_string()).into())
}

async fn journaled_workbench_trace(
    ctx: &ObjectContext<'_>,
    state: AppState,
    session_id: String,
    name: &'static str,
    payload: Value,
    effect_name: &'static str,
) -> HandlerResult<()> {
    ctx.run(move || {
        let state = state.clone();
        let session_id = session_id.clone();
        let payload = payload.clone();
        async move {
            state.trace_for_session(&session_id, name, payload);
            Ok::<(), HandlerError>(())
        }
    })
    .name(effect_name)
    .await?;
    Ok(())
}

fn next_cron_time(
    expr: &str,
    tz: Option<&str>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let timezone: Tz = tz
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("UTC")
        .parse()
        .map_err(|err| format!("invalid timezone: {err}"))?;
    let cron = CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expr)
        .map_err(|err| format!("invalid cron expression `{expr}`: {err}"))?;
    let zoned_now = now.with_timezone(&timezone);
    cron.find_next_occurrence(&zoned_now, false)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|err| format!("cron expression `{expr}` has no next occurrence: {err}"))
}

fn cron_job_key(session_id: &str, source_key: &str) -> String {
    format!("{session_id}:{source_key}")
}
