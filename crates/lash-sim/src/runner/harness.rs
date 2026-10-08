use super::*;

/// The retention of the live replay a generated world's cores observe
/// through: bounded by count, never by age.
///
/// The world's host reads a turn's activity from the replay once the engine
/// has settled the turn, and a suspended turn's only after the whole workload
/// has drained. An age bound would make a seed's evidence depend on how long
/// the host took to run the workload: a loaded run drops activity an idle
/// run keeps, and a turn's oracle sees no activity at all (FIG-5148).
pub(super) fn world_live_replay_config() -> lash::observe::InMemoryLiveReplayStoreConfig {
    lash::observe::InMemoryLiveReplayStoreConfig {
        max_age: std::time::Duration::MAX,
        ..lash::observe::InMemoryLiveReplayStoreConfig::standard()
    }
}

/// How a generated world core drains queued input: every pending input that
/// may share a run joins the run that takes the head, so a queued input the
/// model admits with its next provider turn runs in that turn (ADR 0101
/// §5.2).
pub(super) fn world_queued_work_batching() -> lash::QueuedWorkBatchingConfig {
    lash::QueuedWorkBatchingConfig::new(1024).with_drain_mode(lash::DrainMode::All)
}

/// A generated world core's live replay ([`world_live_replay_config`]).
pub(super) fn world_live_replay_store() -> Arc<dyn lash::observe::LiveReplayStore> {
    Arc::new(lash::observe::InMemoryLiveReplayStore::new(
        world_live_replay_config(),
    ))
}

pub(super) fn runtime_core_for_scripts(
    scripts: Vec<ProviderWireScript>,
    backend: lash::Backend,
    provider_schedule: Option<ScriptedTransportSchedule>,
) -> Result<
    (
        lash::LashCore,
        Arc<ScriptedLlmHttpTransport>,
        String,
        lash::LlmProfileKey,
    ),
    FixedScriptRunnerError,
> {
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
    let mut transport = ScriptedLlmHttpTransport::from_scripts(scripts)?;
    if let Some(schedule) = provider_schedule {
        transport = transport.with_event_schedule(schedule);
    }
    let transport = Arc::new(transport);
    let (provider_handle, model, provider_kind) =
        runtime_provider_components(&provider_kind, &transport)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(world_queued_work_batching())
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .live_replay_store(world_live_replay_store())
        .serve_test_llm_profile(provider_handle, model.clone())
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    Ok((
        core,
        transport,
        provider_kind,
        lash::LlmProfileKey::new(model.wire_model),
    ))
}
