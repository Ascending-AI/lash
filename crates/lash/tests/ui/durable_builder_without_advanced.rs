use std::path::Path;

async fn durable_core_without_advanced(
    provider: lash::provider::ProviderHandle,
    data_dir: &Path,
) -> lash::Result<lash::LashCore> {
    let model = lash::LlmProfileMetadata::builder("compile-only")
        .cache_retention(lash::provider::CacheRetention::Short)
        .context_window_tokens(4096)
        .build()
        .expect("valid model metadata");

    // A store set supplies durable ports to a test effect host.
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            data_dir.join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("sqlite store set"),
    );
    let backend: lash::Backend = lash_conformance::backend_over(stores);
    // The RLM factory keeps its Lash VM artifacts in that same backend.
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    lash::LashCore::rlm_builder(backend, factory)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "compile-only",
                    lash::RegisteredLlmProfile::new(model, provider),
                )
                .expect("one key registers"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "durable-builder-test-worker",
            "durable-builder-test-boot",
        ))
}

fn main() {
    let _ = durable_core_without_advanced;
}

fn inspect_send(outcome: lash::SendOutcome) {
    let _ = outcome.status();
    match outcome {
        lash::SendOutcome::OperationSettled { run, outcome, gaps } => {
            let _ = (run, outcome, gaps);
        }
        lash::SendOutcome::Settled { run, output, gaps } => {
            let _ = (run, output, gaps);
        }
        lash::SendOutcome::Parked { parked, gaps } => {
            let _ = (parked, gaps);
        }
        lash::SendOutcome::Stalled { stalled, gaps } => {
            let _ = (stalled, gaps);
        }
        lash::SendOutcome::Refused { run, refusal, gaps } => {
            let _ = (run, refusal, gaps);
        }
        lash::SendOutcome::Withdrawn { gaps } | lash::SendOutcome::NotAccepted { gaps } => {
            let _ = gaps;
        }
    }
}

async fn operation_run(session: &lash::LashSession) -> lash::Result<()> {
    let run = session
        .plugin_operations()
        .start_task_raw("task", serde_json::Value::Null, "host-key")
        .await?;
    let _ = run.events();
    let _ = run.cancel().await?;
    let _ = session.durable().run(run.run().clone()).result().await?;
    Ok(())
}
