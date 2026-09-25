use std::{path::Path, sync::Arc};

async fn durable_core_without_advanced(
    provider: lash::provider::ProviderHandle,
    data_dir: &Path,
) -> lash::Result<lash::LashCore> {
    let model = lash::ModelSpec::builder("compile-only")
        .context_window_tokens(4096)
        .build()
        .expect("valid model metadata");

    // One file backend supplies every port and the effect host.
    let backend = lash_sqlite_store::SqliteBackend::open(data_dir)
        .await
        .expect("sqlite backend");
    // The RLM factory keeps its Lashlang artifacts in that same backend.
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    lash::LashCore::rlm_builder(Arc::new(backend), lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .termination(lash::durability::TerminationPolicy::default())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "durable-builder-test-worker",
            "durable-builder-test-boot",
        ))
}

fn main() {
    let _ = durable_core_without_advanced;
}
