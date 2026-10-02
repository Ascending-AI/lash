use std::path::Path;

async fn durable_core_without_advanced(
    provider: lash::provider::ProviderHandle,
    data_dir: &Path,
) -> lash::Result<lash::LashCore> {
    let model = lash::LlmProfileMetadata::builder("compile-only")
        .context_window_tokens(4096)
        .build()
        .expect("valid model metadata");

    // A store set supplies durable ports to a test effect host.
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(data_dir)
            .await
            .expect("sqlite store set"),
    );
    let backend: lash::Backend = lash_conformance::recording_backend_over(stores);
    // The RLM factory keeps its Lashlang artifacts in that same backend.
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

fn inspect_send(outcome: lash::SendOutcome) {
    let _ = outcome.status();
    match outcome {
        lash::SendOutcome::Settled { root, output, gaps } => {
            let _ = (root, output, gaps);
        }
        lash::SendOutcome::Parked { parked, gaps } => {
            let _ = (parked, gaps);
        }
        lash::SendOutcome::Stalled { stalled, gaps } => {
            let _ = (stalled, gaps);
        }
        lash::SendOutcome::Withdrawn { gaps } => {
            let _ = gaps;
        }
    }
}
