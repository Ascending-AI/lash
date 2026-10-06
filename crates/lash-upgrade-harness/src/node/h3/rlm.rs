//! H3's real RLM program fixture; the provider supplies code, the engine runs it.
use super::super::RestateArgs;
use anyhow::Result;
use std::sync::Arc;

fn core(backend: lash::Backend, code: &str) -> Result<lash::LashCore> {
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::rlm::TypescriptDialect),
        &backend,
    );
    let code = format!("<typescript>\n{code}\n</typescript>");
    let provider = lash_core::testing::TestProvider::builder()
        .kind("e2e-h3-rlm")
        .serialize_config(|| serde_json::json!({"fixture":"e2e-h3-rlm"}))
        .complete(move |_| {
            let code = code.clone();
            async move { Ok(super::super::scripted_reply(code)) }
        })
        .build()
        .into_handle();
    let models = lash::LlmProfileRegistry::new().register(
        super::super::PROFILE_KEY,
        lash::RegisteredLlmProfile::new(super::super::model()?, provider),
    )?;
    let materials = backend.tool_material_store();
    Ok(lash::LashCore::rlm_builder(backend, factory)
        .plugin(super::plugin("", materials))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .llm_profiles(Arc::new(models))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "e2e-h3",
            "rlm-double",
        ))?)
}

/// V7, real SQLite storage and the production RLM executor. Manual server time
/// makes firing an application timer an explicit action, never a readiness wait.
pub async fn fixture(
    seed: u64,
    code: &str,
) -> Result<(
    lash::LashCore,
    lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
)> {
    let stores: Arc<dyn lash::StoreSet> = Arc::new(lash::sqlite::SqliteStoreSet::memory().await?);
    let args = RestateArgs {
        ingress_url: "http://127.0.0.1:9".into(),
        admin_url: "http://127.0.0.1:9".into(),
        authority: format!("lash-restate-test-{seed}"),
        namespace: String::new(),
    };
    let bootstrap = super::super::engine(stores.clone(), &args)?;
    let generation = core(lash::Backend::new(bootstrap), code)?
        .build_generation()
        .clone();
    let double = lash_restate_test::backend_with_store_set(
        seed,
        lash_restate_test::ServerConfig {
            build_generation: generation,
            protocol: lash_restate_test::ProtocolVersion::V7,
            always_replay: true,
            time: lash_restate_test::TimeMode::Manual,
            ..Default::default()
        },
        Default::default(),
        |_| async move { Ok(stores) },
    )
    .await?;
    let core = core(double.lash_backend(), code)?;
    double.install_process_worker(lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()?,
    )?);
    Ok((core, double))
}
