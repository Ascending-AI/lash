//! Compiled sources for the Rust snippets on `docs/embedding-advanced.html`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use lash::persistence::{AttachmentStore, LashlangArtifactStore, SessionStoreFactory};
use lash::process::ProcessRegistry;
use lash::provider::ProviderHandle;
use lash::{LashCore, LashSession, ModelSpec};
use lash_plugin_mcp::McpPluginFactory;

async fn inmemory_core(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:inmemory-core
    use std::sync::Arc;

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .build()?;
    // docs:end:inmemory-core
    Ok(())
}

async fn sqlite_core(
    provider: ProviderHandle,
    model: ModelSpec,
    data_dir: PathBuf,
) -> anyhow::Result<()> {
    // docs:start:sqlite-core
    use std::sync::Arc;

    use lash::persistence::FileAttachmentStore;
    use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

    let store_factory = Arc::new(SqliteSessionStoreFactory::new(data_dir.join("sessions")));
    let artifact_store = Arc::new(Store::open(&data_dir.join("artifacts.db")).await?);

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        artifact_store,
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .build()?;
    // docs:end:sqlite-core
    Ok(())
}

async fn refresh_background_graph(session: &LashSession) -> anyhow::Result<()> {
    // docs:start:refresh-background-graph
    session.refresh_background_graph().await?;
    // docs:end:refresh-background-graph
    Ok(())
}

async fn lashlang_module_facade(
    artifact_store: Arc<dyn LashlangArtifactStore>,
) -> anyhow::Result<()> {
    // docs:start:lashlang-module-facade
    use std::collections::BTreeMap;

    let mut resources = lashlang::LashlangHostCatalog::new();
    resources.add_trigger_source_constructor(
        ["app", "button"],
        lashlang::TypeExpr::Object(Vec::new()),
        lashlang::NamedDataType::object(
            "app.ButtonPressed",
            vec![lashlang::TypeField {
                name: "color".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            }],
        )?,
    )?;

    let environment = lashlang::LashlangHostEnvironment {
        resources,
        abilities: lashlang::LashlangAbilities::default().with_processes(),
        ..lashlang::LashlangHostEnvironment::default()
    };

    let compiled = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"
process on_button(event: app.ButtonPressed) {
  finish event.color
}
source = app.button({})
finish source
"#,
        environment: &environment,
        artifact_store: Some(artifact_store.as_ref()),
    })
    .await?;

    let process = compiled
        .introspection
        .exported_processes
        .iter()
        .find(|process| process.definition.process_name == "on_button")
        .expect("compiled module exports on_button");

    let inputs = lashlang::TriggerInputTemplate::new(BTreeMap::from([(
        "event".to_string(),
        lashlang::TriggerInputBinding::Event,
    )]));
    let compatibility =
        lashlang::check_trigger_compatibility(lashlang::TriggerCompatibilityRequest {
            artifact: &compiled.artifact,
            definition: &process.definition,
            source_type: "app.button",
            inputs: &inputs,
        })?;

    println!(
        "compiled {} and trigger emits {}",
        compiled.module_ref,
        compatibility.event_type.name()
    );
    // docs:end:lashlang-module-facade
    Ok(())
}

async fn process_registry_core(
    provider: ProviderHandle,
    model: ModelSpec,
    store_factory: Arc<dyn SessionStoreFactory>,
    process_registry: Arc<dyn ProcessRegistry>,
    artifact_store: Arc<dyn LashlangArtifactStore>,
    attachment_store: Arc<dyn AttachmentStore>,
) -> anyhow::Result<()> {
    // docs:start:process-registry-core
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        artifact_store,
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .process_registry(process_registry)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(attachment_store)
        .build()?;
    // docs:end:process-registry-core
    Ok(())
}

async fn subagents_core(
    provider: ProviderHandle,
    model: String,
    tier_models: BTreeMap<String, ModelSpec>,
) -> anyhow::Result<()> {
    // docs:start:subagents-core
    use std::sync::Arc;

    use lash::{SessionSpec, plugins::PluginFactory};
    use lash_subagents::{SubagentsPluginFactory, default_registry};

    let registry = Arc::new(default_registry(&tier_models));

    let child_spec = SessionSpec::inherit().turn_budget(lash::TurnBudget::bounded(8));
    let _configured_turn_budget = child_spec.turn_budget;
    let subagents = SubagentsPluginFactory::new(registry).with_session_spec(child_spec);

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model.clone())
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .plugin(Arc::new(subagents) as Arc<dyn PluginFactory>)
        .build()?;
    // docs:end:subagents-core
    Ok(())
}

async fn mcp_core(provider: ProviderHandle, model: String) -> anyhow::Result<()> {
    // docs:start:mcp-core
    use std::collections::BTreeMap;

    use lash_plugin_mcp::{McpPluginFactory, McpServerConfig};

    let mut servers = BTreeMap::new();
    servers.insert(
        "docs".to_string(),
        McpServerConfig::stdio("uvx", vec!["mcp-server-docs".into()]),
    );
    servers.insert(
        "web".to_string(),
        McpServerConfig::streamable_http("https://mcp.example.com/rpc"),
    );

    let mcp = McpPluginFactory::new(servers).await?;

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        std::sync::Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model.clone())
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default(),
        ))
        .attachment_store(std::sync::Arc::new(
            lash::persistence::InMemoryAttachmentStore::new(),
        ))
        .plugin(std::sync::Arc::new(mcp))
        .build()?;
    // docs:end:mcp-core
    Ok(())
}

async fn mcp_hot_swap(mcp: &McpPluginFactory) -> anyhow::Result<()> {
    use lash_plugin_mcp::McpServerConfig;
    // docs:start:mcp-hot-swap
    // Hot-swap a server at runtime.
    mcp.attach_server(
        "new-tool".to_string(),
        McpServerConfig::stdio("uvx", vec!["mcp-server-new".into()]),
    )
    .await?;

    mcp.detach_server("old-tool").await?;
    // docs:end:mcp-hot-swap
    Ok(())
}

async fn durable_stores_core(
    provider: ProviderHandle,
    data_dir: PathBuf,
    store_factory: Arc<dyn SessionStoreFactory>,
) -> anyhow::Result<()> {
    // docs:start:durable-stores-core
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        std::sync::Arc::new(lash_sqlite_store::Store::open(&data_dir.join("artifacts.db")).await?),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("anthropic/claude-sonnet-4.6")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .store_factory(store_factory)
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default(),
        ))
        .attachment_store(std::sync::Arc::new(
            lash::persistence::FileAttachmentStore::new(data_dir.join("attachments")),
        ))
        .turn_budget(lash::TurnBudget::bounded(50))
        .build()?;
    // docs:end:durable-stores-core
    Ok(())
}
