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
    "turn_budget": "unbounded"
  }
}"#;

fn prompt_probe_state(
    session_id: &str,
    prompt: lash_core::PromptLayer,
) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: session_id.to_string(),
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
fn snapshot_store_from_literal_head(
    literal_head_json: &str,
) -> Arc<SnapshotStore> {
    let payload: lash_core::store::SessionHeadPayload =
        serde_json::from_str(literal_head_json).expect("literal historical session head");
    Arc::new(SnapshotStore::with_state_and_config(
        prompt_probe_state(&payload.session_id, lash_core::PromptLayer::new()),
        payload.config,
    ))
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
        .messages
        .first()
        .filter(|message| {
            matches!(message.role, lash_sansio::llm::types::LlmRole::System)
        })
        .into_iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            lash_sansio::llm::types::LlmContentBlock::Text { text, .. } => {
                Some(text.as_ref())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn legacy_promptless_head_with_host_prompt_renders_host_prompt_in_memory() -> Result<()> {
    use crate::PromptLayerSink as _;

    let store = snapshot_store_from_literal_head(LEGACY_PROMPTLESS_HEAD_JSON);
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("legacy-promptless")
        .store(store)
        .instructions("HOST SUPPLIED AT REOPEN")
        .open()
        .await?;
    session.turn(TurnInput::text("probe")).run().await?;

    let requests = captures.lock_recover();
    assert_eq!(requests.len(), 1);
    assert!(
        rendered_system_prompt(&requests[0]).contains("HOST SUPPLIED AT REOPEN"),
        "literal pre-FIG-1376 bytes must retain main's host-prompt behavior"
    );
    Ok(())
}

#[tokio::test]
async fn legacy_promptless_head_without_host_prompt_matches_fresh_render_in_memory() -> Result<()> {
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let fresh = core.session("fresh-prompt-baseline").open().await?;
    fresh.turn(TurnInput::text("fresh probe")).run().await?;
    let legacy = core
        .session("legacy-promptless")
        .store(snapshot_store_from_literal_head(LEGACY_PROMPTLESS_HEAD_JSON))
        .open()
        .await?;
    legacy.turn(TurnInput::text("legacy probe")).run().await?;

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
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(
        prompt_probe_state("committed-prompt", committed),
    ));
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("committed-prompt").store(store).open().await?;
    session.turn(TurnInput::text("probe")).run().await?;

    let requests = captures.lock_recover();
    assert!(rendered_system_prompt(&requests[0]).contains("COMMITTED PROMPT"));
    Ok(())
}

#[tokio::test]
async fn explicit_empty_committed_prompt_beats_inherited_host_default_in_memory() -> Result<()> {
    use crate::PromptLayerSink as _;

    let state = prompt_probe_state("explicit-empty-prompt", lash_core::PromptLayer::new());
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(SnapshotStore::with_state(state));
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .instructions("INHERITED CORE DEFAULT")
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("explicit-empty-prompt")
        .store(store)
        .open()
        .await?;
    session.turn(TurnInput::text("probe")).run().await?;

    let requests = captures.lock_recover();
    assert!(
        !rendered_system_prompt(&requests[0]).contains("INHERITED CORE DEFAULT"),
        "present empty is authority, not legacy absence"
    );
    Ok(())
}

#[tokio::test]
async fn new_host_prompt_overrides_and_recommits_old_prompt_in_memory() -> Result<()> {
    use crate::PromptLayerSink as _;
    use lash_core::SessionCommitStore as _;

    let old = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Old", "OLD STORED PROMPT"),
    );
    let store = Arc::new(SnapshotStore::with_state(prompt_probe_state(
        "host-reprompt",
        old,
    )));
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trace = tempfile::NamedTempFile::new().expect("composition trace");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .instructions("CORE DEFAULT MUST NOT WIN")
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .trace_jsonl_path(trace.path())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("host-reprompt")
        .store(store.clone() as Arc<dyn lash_core::RuntimePersistence>)
        .instructions("NEW HOST PROMPT")
        .open()
        .await?;
    session.turn(TurnInput::text("probe")).run().await?;
    core.flush_trace_sink()?;

    {
        let requests = captures.lock_recover();
        let rendered = rendered_system_prompt(&requests[0]);
        assert!(rendered.contains("NEW HOST PROMPT"));
        assert!(!rendered.contains("OLD STORED PROMPT"));
    }
    let read = store.load_session().await?.expect("recommitted session head");
    let committed = read.config.prompt.expect("new host prompt is present");
    assert!(format!("{committed:?}").contains("NEW HOST PROMPT"));
    let composition_events = std::fs::read_to_string(trace.path())
        .expect("read composition trace")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|record| record["type"] == "composition_changed")
        .count();
    assert_eq!(composition_events, 1, "the changed composition is emitted");
    Ok(())
}

async fn sqlite_prompt_probe_store(
    session_id: &str,
    prompt: lash_core::PromptLayer,
) -> (
    tempfile::TempDir,
    lash_sqlite_store::SqliteSessionStoreFactory,
    Arc<dyn lash_core::RuntimePersistence>,
) {
    use lash_core::SessionStoreFactory as _;

    let dir = tempfile::tempdir().expect("SQLite prompt probe directory");
    let factory = lash_sqlite_store::SqliteSessionStoreFactory::new(dir.path());
    let mut policy = lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded);
    policy.provider_id = "embed-test".to_string();
    policy.model = mock_model_spec();
    policy.prompt = prompt;
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("create SQLite prompt probe store");
    let state = lash_core::RuntimeSessionState {
        session_id: session_id.to_string(),
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
    (dir, factory, store)
}

async fn sqlite_store_from_literal_legacy_head(
) -> (tempfile::TempDir, Arc<dyn lash_core::RuntimePersistence>) {
    let (dir, factory, store) = sqlite_prompt_probe_store(
        "legacy-promptless",
        lash_core::PromptLayer::new(),
    )
    .await;
    let raw = rusqlite::Connection::open(factory.catalog_path()).expect("open SQLite catalog");
    assert_eq!(
        raw.execute(
            "UPDATE session_head SET head_json = ?1 WHERE session_id = ?2",
            rusqlite::params![LEGACY_PROMPTLESS_HEAD_JSON, "legacy-promptless"],
        )
        .expect("install literal historical head"),
        1
    );
    drop(raw);
    (dir, store)
}

#[tokio::test]
async fn legacy_promptless_head_with_host_prompt_renders_host_prompt_sqlite() -> Result<()> {
    use crate::PromptLayerSink as _;

    let (_dir, store) = sqlite_store_from_literal_legacy_head().await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("legacy-promptless")
        .store(store)
        .instructions("SQLITE HOST PROMPT")
        .open()
        .await?;
    session.turn(TurnInput::text("probe")).run().await?;
    assert!(rendered_system_prompt(&captures.lock_recover()[0]).contains("SQLITE HOST PROMPT"));
    Ok(())
}

#[tokio::test]
async fn legacy_promptless_head_without_host_prompt_matches_fresh_render_sqlite() -> Result<()> {
    let (_dir, store) = sqlite_store_from_literal_legacy_head().await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    core.session("fresh-sqlite-baseline")
        .open()
        .await?
        .turn(TurnInput::text("fresh"))
        .run()
        .await?;
    core.session("legacy-promptless")
        .store(store)
        .open()
        .await?
        .turn(TurnInput::text("legacy"))
        .run()
        .await?;
    let requests = captures.lock_recover();
    assert_eq!(rendered_system_prompt(&requests[1]), rendered_system_prompt(&requests[0]));
    Ok(())
}

#[tokio::test]
async fn committed_prompt_without_host_prompt_renders_committed_prompt_sqlite() -> Result<()> {
    let committed = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Committed", "SQLITE COMMITTED PROMPT"),
    );
    let (_dir, _factory, store) = sqlite_prompt_probe_store("sqlite-committed", committed).await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    core.session("sqlite-committed")
        .store(store)
        .open()
        .await?
        .turn(TurnInput::text("probe"))
        .run()
        .await?;
    assert!(rendered_system_prompt(&captures.lock_recover()[0]).contains("SQLITE COMMITTED PROMPT"));
    Ok(())
}

#[tokio::test]
async fn explicit_empty_committed_prompt_beats_inherited_host_default_sqlite() -> Result<()> {
    use crate::PromptLayerSink as _;

    let (_dir, _factory, store) =
        sqlite_prompt_probe_store("sqlite-explicit-empty", lash_core::PromptLayer::new()).await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .instructions("SQLITE INHERITED DEFAULT")
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    core.session("sqlite-explicit-empty")
        .store(store)
        .open()
        .await?
        .turn(TurnInput::text("probe"))
        .run()
        .await?;
    assert!(!rendered_system_prompt(&captures.lock_recover()[0]).contains("SQLITE INHERITED DEFAULT"));
    Ok(())
}

#[tokio::test]
async fn new_host_prompt_overrides_and_recommits_old_prompt_sqlite() -> Result<()> {
    use crate::PromptLayerSink as _;

    let old = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Old", "SQLITE OLD PROMPT"),
    );
    let (_dir, _factory, store) = sqlite_prompt_probe_store("sqlite-host-reprompt", old).await;
    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trace = tempfile::NamedTempFile::new().expect("SQLite composition trace");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(prompt_capture_provider(Arc::clone(&captures)))
        .model(mock_model_spec())
        .trace_jsonl_path(trace.path())
        .build(crate::testing::runtime_lease_owner())?;
    core.session("sqlite-host-reprompt")
        .store(Arc::clone(&store))
        .instructions("SQLITE NEW HOST PROMPT")
        .open()
        .await?
        .turn(TurnInput::text("probe"))
        .run()
        .await?;
    core.flush_trace_sink()?;
    let rendered = rendered_system_prompt(&captures.lock_recover()[0]);
    assert!(rendered.contains("SQLITE NEW HOST PROMPT"));
    assert!(!rendered.contains("SQLITE OLD PROMPT"));
    let committed = store.load_session().await?.expect("recommitted SQLite head");
    assert!(
        format!("{:?}", committed.config.prompt.expect("present prompt"))
            .contains("SQLITE NEW HOST PROMPT")
    );
    let composition_events = std::fs::read_to_string(trace.path())
        .expect("read SQLite composition trace")
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|record| record["type"] == "composition_changed")
        .count();
    assert_eq!(composition_events, 1, "the changed composition is emitted");
    Ok(())
}
