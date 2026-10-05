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
    core.session(SessionId::fixture(id.to_string()))
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
    lash_core::store::SessionStore::new(runtime_store, SessionId::fixture(id))
        .expect("session view")
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
    assert_eq!(request.model.wire_model(), "created-model");
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

/// A reopen opens with the recorded config unchanged, and the open makes no
/// store write at all.
#[tokio::test]
async fn a_reopen_runs_the_recorded_config_and_writes_nothing() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, writes) = counting_core(Arc::clone(&captures)).await?;
    create_with_creation_spec(&core, "reopen-writes-nothing").await?;
    let session = core
        .session(crate::SessionId::parse("reopen-writes-nothing").expect("nonblank host identity"))
        .open()
        .await?;
    session.send(TurnInput::text("commit")).output().await?;
    Box::pin(session.close()).await?;
    let before = recorded_config(&backend, "reopen-writes-nothing").await;

    writes.lock_recover().clear();
    let reopened = core
        .session(crate::SessionId::parse("reopen-writes-nothing").expect("nonblank host identity"))
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

/// ADR 0026: a model change retains the session's attachment snapshot; only
/// `SetAttachmentAcceptance` replaces it.
#[tokio::test]
async fn a_profile_change_keeps_the_attachment_snapshot() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (core, backend, _writes) = counting_core(captures).await?;
    create_with_creation_spec(&core, "patch-keeps-attachments").await?;
    let session = core
        .session(
            crate::SessionId::parse("patch-keeps-attachments").expect("nonblank host identity"),
        )
        .open()
        .await?;
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

/// A session created by `create()` and first executed by the engine — a send
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
        .id(crate::TurnId::parse("engine-driven-root").expect("nonblank host identity"))
        .output()
        .await?;
    assert_request_uses_creation_config(&captures.lock_recover()[0]);
    Ok(())
}
