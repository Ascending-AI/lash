use std::{path::Path, sync::Arc};

async fn durable_core_without_advanced(
    provider: lash::provider::ProviderHandle,
    data_dir: &Path,
) -> lash::Result<lash::LashCore> {
    let model = lash::ModelSpec::builder("compile-only")
        .context_window_tokens(4096)
        .build()
        .expect("valid model metadata");

    lash::LashCore::rlm_builder(
        lash::TurnBudget::Unbounded,
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::new(
                lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
                lash_protocol_rlm::ExecutionBound::secs(30),
            ),
        Arc::new(
            lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                .await
                .expect("sqlite artifact store"),
        ),
    ))
    .provider(provider)
    .model(model)
    .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("sessions"),
    )))
    .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
        data_dir.join("attachments"),
    )))
    .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
    .termination(lash::durability::TerminationPolicy::default())
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        "durable-builder-test-worker",
        "durable-builder-test-boot",
    ))
}

fn main() {
    let _ = durable_core_without_advanced;
}
