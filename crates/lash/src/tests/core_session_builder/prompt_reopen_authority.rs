// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_sansio::SessionId;

const LEGACY_PROMPTLESS_HEAD_JSON: &str = r#"{
  "schema_version": 3,
  "session_id": "legacy-promptless",
  "config": {
    "provider_id": "embed-test",
    "model": {
      "id": "",
      "variant": "provider_default",
      "limits": { "context_window_tokens": 1 }
    },
    "turn_budget": "unbounded",
    "tool_access": { "mode": "ambient" },
    "config_revision": 0
  }
}"#;

fn prompt_probe_state(
    session_id: &SessionId,
    prompt: lash_core::PromptLayer,
) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        policy: lash_core::SessionPolicy {
            provider_id: "embed-test".to_string(),
            model: mock_model_spec(),
            prompt,
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    }
}
/// A backend whose one session's head records the config of the literal
/// historical head `literal_head_json`.
async fn backend_from_literal_head(literal_head_json: &str) -> lash_core::Backend {
    let payload: lash_core::store::SessionHeadPayload =
        serde_json::from_str(literal_head_json).expect("literal historical session head");
    backend_seeded_with_config(
        prompt_probe_state(&payload.session_id, lash_core::PromptLayer::new()),
        payload.config,
    )
    .await
    .0
}

fn prompt_capture_provider(
    captures: Arc<std::sync::Mutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let captures = Arc::clone(&captures);
            async move {
                captures.lock_recover().push(request);
                Ok(text_response("prompt captured"))
            }
        })
        .build()
        .into_handle()
}

fn rendered_system_prompt(request: &lash_core::LlmRequest) -> String {
    request
        .instructions
        .as_deref()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn core_prompt_redeploy_reaches_persisted_session_without_session_prompt() -> Result<()> {
    use crate::PromptLayerSink as _;

    let backend = double_backend().await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core_v1 = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .instructions("CORE PROMPT V1")
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core_v1
        .session("core-prompt-redeploy")
        .created()
        .await
        .open()
        .await?;
    session.send(TurnInput::text("commit V1")).output().await?;
    drop(session);
    // V1's drive outlives the answer while it closes the root's scope
    // (FIG-3979), and an input sent to the session meanwhile is admitted by
    // that drive, on V1's driver: V2's own driver (FIG-4017) serves only the
    // drives that start after it is installed.
    settle_session_drive(&core_v1, "core-prompt-redeploy").await;
    drop(core_v1);

    let core_v2 = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .instructions("CORE PROMPT V2")
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    core_v2
        .session("core-prompt-redeploy")
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("render V2"))
        .output()
        .await?;

    let requests = captures.lock_recover();
    assert_eq!(requests.len(), 2);
    let rendered = rendered_system_prompt(&requests[1]);
    assert!(rendered.contains("CORE PROMPT V2"));
    assert!(!rendered.contains("CORE PROMPT V1"));
    Ok(())
}

#[tokio::test]
async fn open_with_state_without_builder_prompt_renders_supplied_snapshot_prompt() -> Result<()> {
    let supplied =
        lash_core::PromptLayer::new().with_contribution(lash_core::PromptContribution::guidance(
            "Supplied snapshot",
            "OPEN WITH STATE SUPPLIED PROMPT",
        ));
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    core.session("open-with-state-supplied-prompt")
        .created()
        .await
        .open_with_state(prompt_probe_state(
            &SessionId::from("open-with-state-supplied-prompt"),
            supplied,
        ))
        .await?
        .send(TurnInput::text("probe"))
        .output()
        .await?;

    let requests = captures.lock_recover();
    assert_eq!(requests.len(), 1);
    assert!(rendered_system_prompt(&requests[0]).contains("OPEN WITH STATE SUPPLIED PROMPT"));
    Ok(())
}

/// FIG-4099, FIG-4112: the supplied snapshot is the session's config; the
/// prompt the session was created with is not reconciled over it.
#[tokio::test]
async fn open_with_state_runs_the_supplied_snapshot_prompt_not_the_created_one() -> Result<()> {
    use crate::PromptLayerSink as _;

    let supplied = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Supplied snapshot", "OPEN WITH STATE OLD PROMPT"),
    );
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    core.session("open-with-state-created-prompt")
        .create(crate::SessionCreation::default().instructions("OPEN WITH STATE NEW PROMPT"))
        .await?;
    core.session("open-with-state-created-prompt")
        .open_with_state(prompt_probe_state(
            &SessionId::from("open-with-state-created-prompt"),
            supplied,
        ))
        .await?
        .send(TurnInput::text("probe"))
        .output()
        .await?;

    let requests = captures.lock_recover();
    assert_eq!(requests.len(), 1);
    let rendered = rendered_system_prompt(&requests[0]);
    assert!(rendered.contains("OPEN WITH STATE OLD PROMPT"));
    assert!(!rendered.contains("OPEN WITH STATE NEW PROMPT"));
    Ok(())
}

#[tokio::test]
async fn legacy_promptless_head_without_host_prompt_matches_fresh_render_in_memory() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let fresh = core
        .session("fresh-prompt-baseline")
        .created()
        .await
        .open()
        .await?;
    fresh.send(TurnInput::text("fresh probe")).output().await?;
    let legacy_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend_from_literal_head(LEGACY_PROMPTLESS_HEAD_JSON).await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let legacy = legacy_core
        .session("legacy-promptless")
        .created()
        .await
        .open()
        .await?;
    legacy
        .send(TurnInput::text("legacy probe"))
        .output()
        .await?;

    let requests = captures.lock_recover();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        rendered_system_prompt(&requests[1]),
        rendered_system_prompt(&requests[0]),
        "legacy absence must keep main's ordinary prompt reconstruction"
    );
    Ok(())
}

#[tokio::test]
async fn committed_prompt_without_host_prompt_renders_committed_prompt_in_memory() -> Result<()> {
    let committed = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Committed", "COMMITTED PROMPT"),
    );
    let (backend, _) = backend_seeded(prompt_probe_state(
        &SessionId::from("committed-prompt"),
        committed,
    ))
    .await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("committed-prompt")
        .created()
        .await
        .open()
        .await?;
    session.send(TurnInput::text("probe")).output().await?;

    let requests = captures.lock_recover();
    assert!(rendered_system_prompt(&requests[0]).contains("COMMITTED PROMPT"));
    Ok(())
}

#[tokio::test]
async fn explicit_empty_committed_session_prompt_preserves_live_core_prompt_in_memory() -> Result<()>
{
    use crate::PromptLayerSink as _;

    let state = prompt_probe_state(
        &SessionId::from("explicit-empty-prompt"),
        lash_core::PromptLayer::new(),
    );
    let (backend, _) = backend_seeded(state).await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .instructions("INHERITED CORE DEFAULT")
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("explicit-empty-prompt")
        .created()
        .await
        .open()
        .await?;
    session.send(TurnInput::text("probe")).output().await?;

    let requests = captures.lock_recover();
    assert!(
        rendered_system_prompt(&requests[0]).contains("INHERITED CORE DEFAULT"),
        "durable session state must not erase the live core prompt"
    );
    Ok(())
}

/// FIG-4099, FIG-4112: a reopen, which cannot state a prompt, writes
/// nothing; the prompt changes through `update(SessionConfigPatch)`, which
/// recommits it.
#[tokio::test]
async fn a_reopen_writes_nothing_and_update_recommits_the_prompt_in_memory() -> Result<()> {
    let old = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Old", "OLD STORED PROMPT"),
    );
    let (backend, store) = backend_seeded(prompt_probe_state(
        &SessionId::from("host-reprompt"),
        old.clone(),
    ))
    .await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trace = tempfile::NamedTempFile::new().expect("composition trace");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .trace_jsonl_path(trace.path())
    .build(crate::testing::runtime_lease_owner())?;

    let before = store
        .load_session_head_meta()
        .await?
        .expect("seeded session head");
    let session = core.session("host-reprompt").open().await?;
    let after_open = store
        .load_session_head_meta()
        .await?
        .expect("seeded session head");
    assert_eq!(
        after_open.head_revision, before.head_revision,
        "the reopen wrote nothing"
    );
    assert_eq!(after_open.config, before.config, "the reopen wrote nothing");
    session.send(TurnInput::text("probe")).output().await?;
    core.flush_trace_sink()?;
    {
        let requests = captures.lock_recover();
        let rendered = rendered_system_prompt(&requests[0]);
        assert!(rendered.contains("OLD STORED PROMPT"));
        assert!(!rendered.contains("NEW HOST PROMPT"));
    }
    let composition_events = || {
        lash_trace::parse_jsonl_records::<serde_json::Value>(
            &std::fs::read_to_string(trace.path()).expect("read composition trace"),
        )
        .expect("composition trace records")
        .into_iter()
        .filter(|record| record["type"] == "composition_changed")
        .count()
    };
    let events_before_update = composition_events();

    let new = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("New", "NEW HOST PROMPT"),
    );
    session
        .admin()
        .config()
        .update(crate::SessionConfigPatch::with_prompt(new.clone()))
        .await?;
    session
        .send(TurnInput::text("probe again"))
        .output()
        .await?;
    core.flush_trace_sink()?;
    {
        let requests = captures.lock_recover();
        let rendered = rendered_system_prompt(&requests[1]);
        assert!(rendered.contains("NEW HOST PROMPT"));
        assert!(!rendered.contains("OLD STORED PROMPT"));
    }
    let read = store
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await?
        .expect("recommitted session head");
    assert_eq!(
        read.config.prompt,
        Some(new),
        "the update recommitted the prompt"
    );
    assert_eq!(
        composition_events() - events_before_update,
        1,
        "the changed composition is emitted once"
    );
    Ok(())
}

/// A session committed straight into the SQLite session catalog of a fresh
/// Restate double, before any core opens it: the double's store set, its
/// backend, and the session's view.
async fn sqlite_prompt_probe_store(
    session_id: &SessionId,
    prompt: lash_core::PromptLayer,
) -> (
    Arc<lash_sqlite_store::SqliteStoreSet>,
    lash_core::Backend,
    lash_core::store::SessionStore,
) {
    let backend = double_backend().await;
    let stores = Arc::clone(
        latest_double()
            .expect("the backend runs on its held double")
            .stores(),
    );
    let catalog: Arc<dyn lash_core::DeploymentStore> = stores.session_store_factory();
    let mut policy = lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded);
    policy.provider_id = "embed-test".to_string();
    policy.model = mock_model_spec();
    policy.prompt = prompt;
    let store = lash_core::runtime::admit_session_view(
        &catalog,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(session_id.to_string()),
            relation: lash_core::SessionRelation::Root,
            config: policy.clone().into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .expect("create SQLite prompt probe store");
    let state = lash_core::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &state,
            &[],
        ))
        .await
        .expect("commit SQLite prompt probe head");
    (stores, backend, store)
}

async fn sqlite_store_from_literal_legacy_head() -> (
    Arc<lash_sqlite_store::SqliteStoreSet>,
    lash_core::Backend,
    lash_core::store::SessionStore,
) {
    let (stores, backend, store) = sqlite_prompt_probe_store(
        &SessionId::from("legacy-promptless"),
        lash_core::PromptLayer::new(),
    )
    .await;
    let raw = rusqlite::Connection::open(
        stores.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
    .expect("open SQLite catalog");
    // The literal keeps the pre-prompt config bytes for the field-defaulting
    // probe, while the real store decoder still requires this binary's exact
    // session-head envelope generation.
    let current_schema_head_json = LEGACY_PROMPTLESS_HEAD_JSON.replace(
        "\"schema_version\": 3",
        &format!(
            "\"schema_version\": {}",
            crate::formats::SESSION_HEAD_META_SCHEMA_VERSION
        ),
    );
    assert_eq!(
        raw.execute(
            "UPDATE session_head SET head_json = ?1 WHERE session_id = ?2",
            rusqlite::params![current_schema_head_json, "legacy-promptless"],
        )
        .expect("install literal historical head"),
        1
    );
    drop(raw);
    (stores, backend, store)
}

#[tokio::test]
async fn legacy_promptless_head_without_host_prompt_matches_fresh_render_sqlite() -> Result<()> {
    let (_stores, backend, _) = sqlite_store_from_literal_legacy_head().await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    core.session("fresh-sqlite-baseline")
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("fresh"))
        .output()
        .await?;
    core.session("legacy-promptless")
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("legacy"))
        .output()
        .await?;
    let requests = captures.lock_recover();
    assert_eq!(
        rendered_system_prompt(&requests[1]),
        rendered_system_prompt(&requests[0])
    );
    Ok(())
}

#[tokio::test]
async fn committed_prompt_without_host_prompt_renders_committed_prompt_sqlite() -> Result<()> {
    let committed = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Committed", "SQLITE COMMITTED PROMPT"),
    );
    let (_stores, backend, _) =
        sqlite_prompt_probe_store(&SessionId::from("sqlite-committed"), committed).await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    core.session("sqlite-committed")
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("probe"))
        .output()
        .await?;
    assert!(
        rendered_system_prompt(&captures.lock_recover()[0]).contains("SQLITE COMMITTED PROMPT")
    );
    Ok(())
}

#[tokio::test]
async fn explicit_empty_committed_session_prompt_preserves_live_core_prompt_sqlite() -> Result<()> {
    use crate::PromptLayerSink as _;

    let (_stores, backend, _) = sqlite_prompt_probe_store(
        &SessionId::from("sqlite-explicit-empty"),
        lash_core::PromptLayer::new(),
    )
    .await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .instructions("SQLITE INHERITED DEFAULT")
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    core.session("sqlite-explicit-empty")
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("probe"))
        .output()
        .await?;
    assert!(
        rendered_system_prompt(&captures.lock_recover()[0]).contains("SQLITE INHERITED DEFAULT")
    );
    Ok(())
}

#[tokio::test]
async fn a_reopen_writes_nothing_and_update_recommits_the_prompt_sqlite() -> Result<()> {
    let old = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Old", "SQLITE OLD PROMPT"),
    );
    let (_stores, backend, store) =
        sqlite_prompt_probe_store(&SessionId::from("sqlite-host-reprompt"), old).await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(prompt_capture_provider(Arc::clone(&captures)))
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let before = store
        .load_session_head_meta()
        .await?
        .expect("seeded SQLite head");
    let session = core.session("sqlite-host-reprompt").open().await?;
    let after_open = store
        .load_session_head_meta()
        .await?
        .expect("seeded SQLite head");
    assert_eq!(
        after_open.head_revision, before.head_revision,
        "the reopen wrote nothing"
    );
    assert_eq!(after_open.config, before.config, "the reopen wrote nothing");
    session.send(TurnInput::text("probe")).output().await?;
    let rendered = rendered_system_prompt(&captures.lock_recover()[0]);
    assert!(rendered.contains("SQLITE OLD PROMPT"));
    assert!(!rendered.contains("SQLITE NEW HOST PROMPT"));

    let new = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("New", "SQLITE NEW HOST PROMPT"),
    );
    session
        .admin()
        .config()
        .update(crate::SessionConfigPatch::with_prompt(new.clone()))
        .await?;
    drop(session);
    let committed = store
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await?
        .expect("recommitted SQLite head");
    assert_eq!(
        committed.config.prompt,
        Some(new),
        "the update recommitted the prompt"
    );
    Ok(())
}
