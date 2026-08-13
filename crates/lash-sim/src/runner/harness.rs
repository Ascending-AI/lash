use super::*;

pub(super) fn runtime_core_for_scripts(
    scripts: Vec<ProviderWireScript>,
    store_factory: Arc<dyn SessionStoreFactory>,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
    provider_schedule: Option<ScriptedTransportSchedule>,
    disable_inline_queued_work_driver: bool,
    clock: Arc<SimClock>,
) -> Result<(lash::LashCore, Arc<ScriptedLlmHttpTransport>, String), FixedScriptRunnerError> {
    let provider_kind = scripts
        .first()
        .ok_or_else(|| {
            FixedScriptRunnerError::Assertion(
                "runtime core requires at least one script".to_string(),
            )
        })?
        .provider_kind
        .clone();
    if scripts
        .iter()
        .any(|script| script.provider_kind != provider_kind)
    {
        return Err(FixedScriptRunnerError::Assertion(
            "runtime provider scripts for a session must use one provider kind".to_string(),
        ));
    }
    let mut transport = ScriptedLlmHttpTransport::from_scripts(scripts);
    if let Some(schedule) = provider_schedule {
        transport = transport.with_event_schedule(schedule);
    }
    let transport = Arc::new(transport);
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(&provider_kind, &transport)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let mut builder = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .effect_host(Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(attachment_store)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(process_env_store)
        .store_factory(store_factory)
        .clock(clock)
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .provider(provider_handle)
        .model(model);
    if disable_inline_queued_work_driver {
        builder = builder.disable_queued_work_driver();
    }
    let core = builder
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    Ok((core, transport, provider_kind))
}
