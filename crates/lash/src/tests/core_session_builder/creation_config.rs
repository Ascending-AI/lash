//! FIG-4099, FIG-4112: a session's config is baked in when it is created,
//! and only `create(SessionCreation)` states it. An open runs with what the
//! session recorded and writes nothing — it cannot state config at all (the
//! `session_open_takes_no_config` UI fixture) — and every later change is a
//! config transaction (FIG-4379).

use super::*;
use lash_sansio::SessionId;

fn snapshot(revision: &str) -> Arc<lash_core::AttachmentCapabilitySnapshot> {
    Arc::new(lash_core::AttachmentCapabilitySnapshot {
        revision: revision.to_string(),
        acceptors: Vec::new(),
    })
}

/// Every model key these laws select, each at the mock model's shape.
const SERVED_MODELS: [&str; 4] = [
    "mock-model",
    "created-model",
    "patched-model",
    "upgraded-model",
];

/// A standard prompt whose one instruction is `text`.
fn guidance(text: &str) -> crate::standard::StandardPrompt {
    crate::standard::StandardPrompt {
        instructions: vec![text.to_string()],
        ..Default::default()
    }
}

/// The standard prompt `config` records.
fn recorded_prompt(config: &lash_core::PersistedSessionConfig) -> crate::standard::StandardPrompt {
    config
        .plugin_config
        .decode::<crate::standard::StandardRecordedConfig>(
            crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
        )
        .expect("the standard namespace decodes")
        .expect("the session records its standard namespace")
        .prompt
}

fn creation_spec() -> crate::SessionSpec {
    mock_session_spec()
        .model("created-model")
        .attachment_acceptance(snapshot("created-attachments"))
        .plugin(
            crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
            crate::standard::StandardTurnOptions {
                prompt: Some(guidance("CREATED PROMPT")),
                render: None,
            },
        )
        .expect("the standard options encode")
        .generation(lash_core::GenerationOptions {
            seed: Some(7),
            ..Default::default()
        })
}

/// Create `id` with [`creation_spec`], and nothing else.
async fn create_with_creation_spec(core: &LashCore, id: &str) -> Result<crate::DurableSession> {
    core.session(id)
        .create(crate::SessionCreation {
            spec: creation_spec(),
            parent: None,
        })
        .await
}

fn capturing_provider(
    captures: Arc<std::sync::Mutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let captures = Arc::clone(&captures);
            async move {
                captures.lock_recover().push(request);
                Ok(text_response("captured"))
            }
        })
        .build()
        .into_handle()
}

/// A core over a catalog that counts its writes, and the ledger.
async fn counting_core(
    captures: Arc<std::sync::Mutex<Vec<lash_core::LlmRequest>>>,
) -> Result<(
    LashCore,
    lash_core::Backend,
    Arc<std::sync::Mutex<Vec<&'static str>>>,
)> {
    let mut ledger = None;
    let backend: lash_core::Backend = backend_with_catalog(|inner| {
        let (layer, writes) = CountingWrites::over(inner);
        ledger = Some(writes);
        layer
    })
    .await
    .into();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .llm_profiles(test_catalog(
            capturing_provider(captures),
            SERVED_MODELS.map(|key| llm_profile_spec(key, None, 64_000)),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    Ok((core, backend, ledger.expect("the catalog is decorated")))
}

fn session_view(backend: &lash_core::Backend, id: &str) -> lash_core::store::SessionStore {
    let runtime_store: Arc<dyn lash_core::RuntimeStore> = backend.session_store_factory();
    lash_core::store::SessionStore::new(runtime_store, SessionId::from(id)).expect("session view")
}

async fn recorded_config(
    backend: &lash_core::Backend,
    id: &str,
) -> (u64, lash_core::PersistedSessionConfig) {
    let head = session_view(backend, id)
        .load_session_head_meta()
        .await
        .expect("load the head")
        .expect("the session has a head");
    (head.head_revision, head.config)
}

fn assert_runs_creation_config(policy: &lash_core::SessionPolicy) {
    assert_eq!(policy.wire_model(), Some("created-model"));
    assert_eq!(
        policy.attachment_acceptance,
        snapshot("created-attachments")
    );
    assert_eq!(policy.generation.seed, Some(7));
}

fn assert_request_uses_creation_config(request: &lash_core::LlmRequest) {
    assert_eq!(request.model, "created-model");
    assert!(
        request
            .instructions
            .as_deref()
            .unwrap_or_default()
            .contains("CREATED PROMPT"),
        "the turn renders the creation prompt"
    );
    assert_eq!(request.generation.seed, Some(7));
}

#[tokio::test]
async fn identical_session_configs_share_a_process_execution_environment() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(captures).await?;
    create_with_creation_spec(&core, "env-owner-a").await?;
    create_with_creation_spec(&core, "env-owner-b").await?;
    let first = core.session("env-owner-a").open().await?;
    let second = core.session("env-owner-b").open().await?;
    assert_ne!(first.session_id(), second.session_id());
    let (_, first_config) = recorded_config(&backend, "env-owner-a").await;
    let (_, second_config) = recorded_config(&backend, "env-owner-b").await;
    assert_eq!(first_config, second_config);
    let first_env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::new(first_config.plugin_config, 0),
        first.policy_snapshot(),
    );
    let second_env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::new(second_config.plugin_config, 0),
        second.policy_snapshot(),
    );
    assert_eq!(first_env.stable_ref()?, second_env.stable_ref()?);
    Ok(())
}

/// A reopen opens with the recorded config unchanged, and the open makes no
/// store write at all.
#[tokio::test]
async fn a_reopen_runs_the_recorded_config_and_writes_nothing() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, writes) = counting_core(Arc::clone(&captures)).await?;
    create_with_creation_spec(&core, "reopen-writes-nothing").await?;
    let session = core.session("reopen-writes-nothing").open().await?;
    session.send(TurnInput::text("commit")).output().await?;
    Box::pin(session.close()).await?;
    let before = recorded_config(&backend, "reopen-writes-nothing").await;

    writes.lock_recover().clear();
    let reopened = core.session("reopen-writes-nothing").open().await?;
    assert_eq!(
        *writes.lock_recover(),
        Vec::<&str>::new(),
        "a reopen writes nothing"
    );
    assert_eq!(
        recorded_config(&backend, "reopen-writes-nothing").await,
        before
    );
    assert_runs_creation_config(&reopened.policy_snapshot());

    reopened.send(TurnInput::text("probe")).output().await?;
    assert_request_uses_creation_config(captures.lock_recover().last().expect("a request"));
    Ok(())
}

/// Every core config field changes durably through a config transaction, and
/// a cold reopen reads the changed values back.
#[tokio::test]
async fn update_changes_each_config_field_durably() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(Arc::clone(&captures)).await?;
    create_with_creation_spec(&core, "patch-each-field").await?;
    let session = core.session("patch-each-field").open().await?;
    session
        .admin()
        .config()
        .configure(
            crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("patched-model"),
            })
            .then(crate::config::SetAttachmentAcceptance {
                acceptance: (*snapshot("patched-attachments")).clone(),
            })
            .then(crate::standard::SetStandardPrompt {
                prompt: guidance("PATCHED PROMPT"),
            })
            .then(crate::config::SetGeneration {
                generation: crate::GenerationOverlay::Merge(lash_core::GenerationOptions {
                    output_token_cap: std::num::NonZeroUsize::new(41),
                    ..Default::default()
                }),
            }),
        )
        .await?;
    Box::pin(session.close()).await?;

    let (_, config) = recorded_config(&backend, "patch-each-field").await;
    assert_eq!(config.wire_model(), Some("patched-model"));
    assert_eq!(
        config.attachment_acceptance,
        snapshot("patched-attachments")
    );
    assert_eq!(recorded_prompt(&config), guidance("PATCHED PROMPT"));
    assert_eq!(config.generation.seed, Some(7), "a merge keeps the seed");
    assert_eq!(
        config.generation.output_token_cap,
        std::num::NonZeroUsize::new(41)
    );

    let reopened = core.session("patch-each-field").open().await?;
    let policy = reopened.policy_snapshot();
    assert_eq!(policy.wire_model(), Some("patched-model"));
    assert_eq!(
        policy.attachment_acceptance,
        snapshot("patched-attachments")
    );
    assert_eq!(policy.generation, config.generation);
    Ok(())
}

/// ADR 0026: a model change retains the session's attachment snapshot; only
/// `SetAttachmentAcceptance` replaces it.
#[tokio::test]
async fn a_profile_change_keeps_the_attachment_snapshot() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(captures).await?;
    create_with_creation_spec(&core, "patch-keeps-attachments").await?;
    let session = core.session("patch-keeps-attachments").open().await?;
    let config = session.admin().config();
    config
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("upgraded-model"),
            },
        ))
        .await?;
    let policy = session.policy_snapshot();
    assert_eq!(policy.wire_model(), Some("upgraded-model"));
    assert_eq!(
        policy.attachment_acceptance,
        snapshot("created-attachments"),
        "the model change retains the opening snapshot"
    );
    let (_, recorded) = recorded_config(&backend, "patch-keeps-attachments").await;
    assert_eq!(
        recorded.attachment_acceptance,
        snapshot("created-attachments")
    );

    config
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetAttachmentAcceptance {
                acceptance: (*snapshot("adopted-attachments")).clone(),
            },
        ))
        .await?;
    let (_, recorded) = recorded_config(&backend, "patch-keeps-attachments").await;
    assert_eq!(recorded.wire_model(), Some("upgraded-model"));
    assert_eq!(
        recorded.attachment_acceptance,
        snapshot("adopted-attachments")
    );
    Ok(())
}

/// A config command no installed plugin owns is refused typed at
/// submission, and the refused transaction writes nothing — not even its
/// other commands.
#[tokio::test]
async fn a_command_no_plugin_owns_is_refused_typed_and_writes_nothing() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, writes) = counting_core(captures).await?;
    create_with_creation_spec(&core, "patch-unread-plugin-options").await?;
    let session = core.session("patch-unread-plugin-options").open().await?;
    session.send(TurnInput::text("commit")).output().await?;
    let before = recorded_config(&backend, "patch-unread-plugin-options").await;
    writes.lock_recover().clear();

    let config = session.admin().config();
    let revision = config.revision().await?;
    let error = config
        .apply(
            crate::config::ConfigWrite::new("unowned-plugin-command", revision),
            crate::config::ConfigTransaction::of(crate::standard::SetStandardPrompt {
                prompt: guidance("NEVER WRITTEN"),
            })
            .then_entry(crate::config::ConfigCommandEntry {
                owner: "no-such-plugin".to_string(),
                command: "set_k".to_string(),
                args: serde_json::json!({ "k": 1 }),
            }),
        )
        .await
        .expect_err("a command no installed plugin owns is refused");
    assert!(
        matches!(
            &error,
            crate::EmbedError::ConfigSubmit(crate::config::ConfigSubmitError::UnknownOwner { owner })
                if owner == "no-such-plugin"
        ),
        "expected a typed config submission refusal, got: {error:?}"
    );
    assert_eq!(*writes.lock_recover(), Vec::<&str>::new());
    assert_eq!(
        recorded_config(&backend, "patch-unread-plugin-options").await,
        before
    );
    assert_eq!(
        recorded_prompt(&before.1),
        guidance("CREATED PROMPT"),
        "the refused transaction's prompt command wrote nothing"
    );
    Ok(())
}

/// A session created by `create()` records its creation config with its
/// catalog row, so the first turn of a later `open()` that states nothing
/// runs with it.
#[tokio::test]
async fn a_session_created_by_create_runs_its_first_opened_turn_with_the_creation_config()
-> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(Arc::clone(&captures)).await?;
    drop(create_with_creation_spec(&core, "created-by-create").await?);
    let (revision, config) = recorded_config(&backend, "created-by-create").await;
    assert_eq!(revision, 0, "creation writes the config head, no frame");
    assert_runs_creation_config(&config.session_policy());

    let session = core.session("created-by-create").open().await?;
    assert_runs_creation_config(&session.policy_snapshot());
    session.send(TurnInput::text("first turn")).output().await?;
    assert_request_uses_creation_config(&captures.lock_recover()[0]);
    Ok(())
}

/// A session created by `create()` and first driven by the engine — a send
/// through its durable handle, with no host open — runs with the creation
/// config, not the core's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_created_by_create_runs_its_first_engine_driven_turn_with_the_creation_config()
-> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, _backend, _writes) = counting_core(Arc::clone(&captures)).await?;
    let durable = create_with_creation_spec(&core, "created-then-engine-driven").await?;
    durable
        .send(TurnInput::text("the engine opens this session first"))
        .id("engine-driven-root")
        .output()
        .await?;
    assert_request_uses_creation_config(&captures.lock_recover()[0]);
    Ok(())
}
