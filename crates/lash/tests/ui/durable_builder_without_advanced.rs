use std::{path::Path, sync::Arc};

async fn durable_core_without_advanced(
    provider: lash::provider::ProviderHandle,
    data_dir: &Path,
) -> lash::Result<lash::LashCore> {
    let model = lash::ModelSpec::from_token_limits("compile-only", Default::default(), 4096, None)
        .expect("valid model metadata");

    lash::LashCore::rlm_builder(lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
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
    .build()
}

fn main() {
    let _ = durable_core_without_advanced;
}
