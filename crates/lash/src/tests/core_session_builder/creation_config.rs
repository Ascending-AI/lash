//! FIG-4099: a session's config is baked in when it is created. A reopen runs
//! with what the session recorded and writes nothing, and every later change
//! is `update(SessionConfigPatch)`.

use super::*;
use lash_sansio::SessionId;

fn snapshot(revision: &str) -> Arc<lash_core::AttachmentCapabilitySnapshot> {
    Arc::new(lash_core::AttachmentCapabilitySnapshot {
        revision: revision.to_string(),
        acceptors: Vec::new(),
    })
}

/// `id` at the mock model's shape, carrying the attachment snapshot
/// `attachments`.
fn model_with_attachments(id: &str, attachments: &str) -> lash_core::ModelSpec {
    let mut model = model_spec(id, None, 64_000);
    model.capability.attachment_acceptance = snapshot(attachments);
    model
}

fn guidance(text: &str) -> lash_core::PromptLayer {
    lash_core::PromptLayer::new()
        .with_contribution(lash_core::PromptContribution::guidance(text, text))
}

fn creation_spec() -> crate::SessionSpec {
    crate::SessionSpec::new()
        .model(model_with_attachments(
            "created-model",
            "created-attachments",
        ))
        .prompt_layer(guidance("CREATED PROMPT"))
        .generation(lash_core::GenerationOptions {
            seed: Some(7),
            ..Default::default()
        })
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(capturing_provider(captures))
    .model(mock_model_spec())
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
    assert_eq!(policy.model.id, "created-model");
    assert_eq!(
        policy.model.capability.attachment_acceptance,
        snapshot("created-attachments")
    );
    assert_eq!(policy.prompt, guidance("CREATED PROMPT"));
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

/// A reopen whose builder states a different model, prompt, generation and
/// attachment acceptance opens with the recorded config unchanged, and the
/// open makes no store write at all.
#[tokio::test]
async fn a_reopen_stating_other_config_runs_the_recorded_config_and_writes_nothing() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, writes) = counting_core(Arc::clone(&captures)).await?;
    let session = core
        .session("reopen-writes-nothing")
        .session_spec(creation_spec())
        .open()
        .await?;
    session.send(TurnInput::text("commit")).output().await?;
    Box::pin(session.close()).await?;
    let before = recorded_config(&backend, "reopen-writes-nothing").await;

    writes.lock_recover().clear();
    let reopened = core
        .session("reopen-writes-nothing")
        .session_spec(
            crate::SessionSpec::new()
                .model(model_with_attachments("other-model", "other-attachments"))
                .prompt_layer(guidance("OTHER PROMPT"))
                .replace_generation(lash_core::GenerationOptions {
                    seed: Some(9),
                    ..Default::default()
                }),
        )
        .open()
        .await?;
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

/// Every config field changes durably through `update(SessionConfigPatch)`,
/// and a cold reopen reads the patched values back.
#[tokio::test]
async fn update_changes_each_config_field_durably() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(Arc::clone(&captures)).await?;
    let session = core
        .session("patch-each-field")
        .session_spec(creation_spec())
        .open()
        .await?;
    session
        .admin()
        .config()
        .update(crate::SessionConfigPatch {
            model: Some(model_with_attachments(
                "patched-model",
                "ignored-attachments",
            )),
            attachment_acceptance: Some(snapshot("patched-attachments")),
            prompt: Some(guidance("PATCHED PROMPT")),
            generation: Some(crate::GenerationOverlay::Merge(
                lash_core::GenerationOptions {
                    output_token_cap: std::num::NonZeroUsize::new(41),
                    ..Default::default()
                },
            )),
            ..crate::SessionConfigPatch::default()
        })
        .await?;
    Box::pin(session.close()).await?;

    let (_, config) = recorded_config(&backend, "patch-each-field").await;
    assert_eq!(config.model.id, "patched-model");
    assert_eq!(
        config.model.capability.attachment_acceptance,
        snapshot("patched-attachments")
    );
    assert_eq!(config.prompt, Some(guidance("PATCHED PROMPT")));
    assert_eq!(config.generation.seed, Some(7), "a merge keeps the seed");
    assert_eq!(
        config.generation.output_token_cap,
        std::num::NonZeroUsize::new(41)
    );

    let reopened = core.session("patch-each-field").open().await?;
    let policy = reopened.policy_snapshot();
    assert_eq!(policy.model.id, "patched-model");
    assert_eq!(
        policy.model.capability.attachment_acceptance,
        snapshot("patched-attachments")
    );
    assert_eq!(policy.prompt, guidance("PATCHED PROMPT"));
    assert_eq!(policy.generation, config.generation);
    Ok(())
}

/// ADR 0026: a model change retains the session's attachment snapshot; only
/// the patch's own attachment field replaces it.
#[tokio::test]
async fn a_model_change_through_the_patch_keeps_the_attachment_snapshot() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(captures).await?;
    let session = core
        .session("patch-keeps-attachments")
        .session_spec(creation_spec())
        .open()
        .await?;
    let config = session.admin().config();
    config
        .update(crate::SessionConfigPatch {
            model: Some(model_with_attachments(
                "upgraded-model",
                "catalogue-attachments",
            )),
            ..crate::SessionConfigPatch::default()
        })
        .await?;
    let policy = session.policy_snapshot();
    assert_eq!(policy.model.id, "upgraded-model");
    assert_eq!(
        policy.model.capability.attachment_acceptance,
        snapshot("created-attachments"),
        "the model change retains the opening snapshot"
    );
    let (_, recorded) = recorded_config(&backend, "patch-keeps-attachments").await;
    assert_eq!(
        recorded.model.capability.attachment_acceptance,
        snapshot("created-attachments")
    );

    config
        .update(crate::SessionConfigPatch {
            attachment_acceptance: Some(snapshot("adopted-attachments")),
            ..crate::SessionConfigPatch::default()
        })
        .await?;
    let (_, recorded) = recorded_config(&backend, "patch-keeps-attachments").await;
    assert_eq!(recorded.model.id, "upgraded-model");
    assert_eq!(
        recorded.model.capability.attachment_acceptance,
        snapshot("adopted-attachments")
    );
    Ok(())
}

/// Plugin session config that no plugin of the session reads is refused
/// typed, and the refused patch writes nothing — not even its other fields.
#[tokio::test]
async fn plugin_options_no_plugin_reads_are_refused_typed_and_write_nothing() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, writes) = counting_core(captures).await?;
    let session = core
        .session("patch-unread-plugin-options")
        .session_spec(creation_spec())
        .open()
        .await?;
    session.send(TurnInput::text("commit")).output().await?;
    let before = recorded_config(&backend, "patch-unread-plugin-options").await;
    writes.lock_recover().clear();

    let error = session
        .admin()
        .config()
        .update(crate::SessionConfigPatch {
            prompt: Some(guidance("NEVER WRITTEN")),
            plugin_options: Some(
                lash_core::PluginOptions::typed("no-such-plugin", serde_json::json!({ "k": 1 }))
                    .expect("options encode"),
            ),
            ..crate::SessionConfigPatch::default()
        })
        .await
        .expect_err("options no plugin reads are refused");
    let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) = &error
    else {
        panic!("expected a typed session config refusal, got: {error:?}");
    };
    assert_eq!(
        refusal.downcast_ref::<lash_core::PluginOptionsUnaccepted>(),
        Some(&lash_core::PluginOptionsUnaccepted {
            plugin_ids: vec!["no-such-plugin".to_string()],
        })
    );
    assert_eq!(*writes.lock_recover(), Vec::<&str>::new());
    assert_eq!(
        recorded_config(&backend, "patch-unread-plugin-options").await,
        before
    );
    assert_eq!(session.policy_snapshot().prompt, guidance("CREATED PROMPT"));
    Ok(())
}

/// A session created by `open()` runs its first engine-driven turn with the
/// creation config.
#[tokio::test]
async fn a_session_created_by_open_runs_its_first_turn_with_the_creation_config() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, _backend, _writes) = counting_core(Arc::clone(&captures)).await?;
    let session = core
        .session("created-by-open")
        .session_spec(creation_spec())
        .open()
        .await?;
    session.send(TurnInput::text("first turn")).output().await?;
    assert_request_uses_creation_config(&captures.lock_recover()[0]);
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
    drop(
        core.session("created-by-create")
            .session_spec(creation_spec())
            .create()
            .await?,
    );
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
    let durable = core
        .session("created-then-engine-driven")
        .session_spec(creation_spec())
        .create()
        .await?;
    durable
        .send(TurnInput::text("the engine opens this session first"))
        .id("engine-driven-root")
        .output()
        .await?;
    assert_request_uses_creation_config(&captures.lock_recover()[0]);
    Ok(())
}
