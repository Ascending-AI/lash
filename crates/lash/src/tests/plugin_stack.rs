use super::*;

use crate::{TurnEvent, TurnInput};
use lash_core::Backend;

#[tokio::test]
async fn plugin_surface_streams_as_semantic_turn_event() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .plugin(Arc::new(SurfacePluginFactory))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("plugin-surface").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();

    session
        .send(TurnInput::text("hello"))
        .output_into(&events)
        .await?;

    let surface = events
        .snapshot()
        .await
        .into_iter()
        .find(|event| matches!(&event.event, TurnEvent::PluginRuntime { .. }))
        .expect("plugin surface event");
    let TurnEvent::PluginRuntime { plugin_id, event } = surface.event else {
        unreachable!();
    };
    assert_eq!(plugin_id, "surface_test");
    assert!(matches!(
        event,
        lash_core::PluginRuntimeEvent::Status { key, label, .. }
        if key == "surface" && label == "working"
    ));
    Ok(())
}

/// Host tool curation is durable session state: it needs no built session,
/// it applies in a run, and a session reopened on a new deployment over the
/// stores restores it, the host's removal included (FIG-5134).
#[tokio::test]
async fn persisted_session_restores_tool_state() -> Result<()> {
    let app_lookup = lash_core::ToolId::from("tool:app_lookup");
    let stores = sqlite_memory_store_set().await;
    let deploy = || {
        explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
            stores.clone(),
        )))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())
    };
    let core = deploy()?;
    let session = core
        .session(crate::SessionId::parse("persisted-tools").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert!(
        session.admin().tools().state().await?.recorded().is_none(),
        "an open builds no capabilities, so nothing has recorded tool state yet"
    );
    session
        .admin()
        .tools()
        .set_membership(
            app_lookup.clone(),
            false,
            "host:plugin_stack:set_membership:74",
        )
        .await?
        .settle_with(
            &session.admin().commands(),
            crate::testing::admin_fixture_outcome,
        )
        .await?;
    let curated = session.admin().tools().state().await?;
    assert!(
        curated.pending().is_empty(),
        "the settled change is applied, not pending"
    );
    let curated = curated
        .recorded()
        .expect("the command run recorded the session's tool state")
        .clone();
    assert!(
        !curated
            .entries()
            .get(&app_lookup)
            .expect("app tool")
            .is_member(),
        "the command run applied the host's removal"
    );
    drop(session);
    core.shutdown().await?;

    let reopened_core = deploy()?;
    let reopened = reopened_core
        .session(crate::SessionId::parse("persisted-tools").expect("nonblank host identity"))
        .open()
        .await?;
    reopened
        .send(TurnInput::text("restore the persisted tool state"))
        .output()
        .await?;
    let state = reopened.admin().tools().state().await?;
    let restored = state
        .recorded()
        .expect("the run recorded the restored tool state");
    assert!(
        restored.generation >= curated.generation,
        "the restored state continues the curated one: {} < {}",
        restored.generation,
        curated.generation
    );
    assert!(
        !restored
            .entries()
            .get(&app_lookup)
            .expect("app tool")
            .is_member(),
        "the host-removed tool is restored as a non-member"
    );
    reopened_core.shutdown().await?;
    Ok(())
}

/// Runs on the test thread's default 2 MiB stack, the budget a host's
/// runtime thread gets.
#[tokio::test]
async fn tool_completed_activity_is_canonical_while_model_observation_is_projected() -> Result<()> {
    {
        let mut standard_config = crate::plugins::StandardProtocolConfig::default();
        standard_config.render.defaults.value.max_chars = Some(32);
        let observed_tool_results = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let observed_tool_results_provider = Arc::clone(&observed_tool_results);
        let responses = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from([
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "call-1".to_string(),
                    tool_name: "app_lookup".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            },
            LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            },
        ])));
        let standard_provider = lash_core::testing::TestProvider::builder()
            .kind("embed-test")
            .complete(move |request| {
                let observed_tool_results = Arc::clone(&observed_tool_results_provider);
                let responses = Arc::clone(&responses);
                async move {
                    for message in &request.messages {
                        for block in message.blocks.iter() {
                            if let LlmContentBlock::ToolResult { content, .. } = block {
                                observed_tool_results.lock().await.push(
                                    lash_core::facade_support::tool_result_text(content)
                                        .into_owned(),
                                );
                            }
                        }
                    }
                    Ok(responses.lock().await.pop_front().expect("queued response"))
                }
            })
            .build()
            .into_handle();
        let backend = sqlite_memory_store_backend().await;
        let standard_core =
            explicit_ephemeral_facets(LashCore::builder(backend).protocol_plugin(Arc::new(
                crate::plugins::StandardProtocolPluginFactory::with_config(standard_config),
            )))
            .serve_test_llm_profile(standard_provider, mock_llm_profile_spec())
            .tools(Arc::new(LongTextTools))
            .build(crate::testing::runtime_lease_owner())?;
        let standard_session = standard_core
            .session(
                crate::SessionId::parse("standard-projection").expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        let standard_events = RecordingEvents::default();
        let _ = standard_session
            .send(TurnInput::text("use tool"))
            .output_into(&standard_events)
            .await?;
        let standard_view = standard_events
            .snapshot()
            .await
            .into_iter()
            .find_map(|event| match event.event {
                TurnEvent::ToolCallCompleted { output, .. } => Some(output.value_for_projection()),
                _ => None,
            })
            .expect("standard tool completion");
        assert_eq!(
            standard_view,
            serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789")
        );
        let observed = observed_tool_results.lock().await;
        let model_observation = observed
            .iter()
            .find(|content| content.contains("[output cut:"))
            .expect("projected model observation");
        assert!(model_observation.chars().count() <= 32);

        #[cfg(feature = "rlm")]
        {
            let rlm_backend = sqlite_memory_store_backend().await;
            let rlm_factory = rlm_factory(&rlm_backend);
            let rlm_core =
                explicit_ephemeral_facets(LashCore::rlm_builder(rlm_backend, rlm_factory))
                    .serve_test_llm_profile(
                        queued_text_provider(vec![typescript_block(
                            r#"const value = await tools.app_lookup({});
finish("done");"#,
                        )]),
                        mock_llm_profile_spec(),
                    )
                    .tools(Arc::new(LongTextTools))
                    .build(crate::testing::runtime_lease_owner())?;
            let rlm_session = rlm_core
                .session(crate::SessionId::parse("rlm-projection").expect("nonblank host identity"))
                .created()
                .await
                .open()
                .await?;
            let rlm_events = RecordingEvents::default();
            let _ = rlm_session
                .send(TurnInput::text("use tool"))
                .output_into(&rlm_events)
                .await?;
            let rlm_view = rlm_events
                .snapshot()
                .await
                .into_iter()
                .find_map(|event| match event.event {
                    TurnEvent::ToolCallCompleted { output, .. } => {
                        Some(output.value_for_projection())
                    }
                    _ => None,
                })
                .expect("rlm tool completion");

            assert_eq!(rlm_view, standard_view);
        }
        Ok(())
    }
}

struct BuilderSentinelTools {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl ToolProvider for BuilderSentinelTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        AppTools.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        AppTools.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        AppTools.execute(call).await
    }
}

async fn builder_configured_tools_and_hooks_are_never_discarded(backend: Backend) -> Result<()> {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responses = Arc::new(tokio::sync::Mutex::new(
        std::collections::VecDeque::from_iter((0..3).flat_map(|turn| {
            [
                LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("sentinel-{turn}"),
                        tool_name: "app_lookup".to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                },
                LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "done".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                },
            ]
        })),
    ));
    let provider = crate::testing::TestProvider::builder()
        .complete(move |_| {
            let responses = Arc::clone(&responses);
            async move {
                Ok(responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("scripted response"))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(Arc::new(BuilderSentinelTools {
            calls: Arc::clone(&calls),
        }))
        .plugin(Arc::new(SurfacePluginFactory))
        .build(crate::testing::runtime_lease_owner())?;
    let id = "builder-sentinels";
    let mut session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    for turn in 0..3 {
        let events = RecordingEvents::default();
        session
            .send(TurnInput::text("probe"))
            .output_into(&events)
            .await?;
        let hook_invoked = events.snapshot().await.into_iter().any(|event| {
            matches!(
                event.event,
                TurnEvent::PluginRuntime { plugin_id, .. } if plugin_id == "surface_test"
            )
        });
        assert_eq!(
            (
                calls.load(std::sync::atomic::Ordering::SeqCst),
                hook_invoked
            ),
            (turn + 1, true),
            "configured tool and hook must run before and after session rematerialization"
        );
        if turn == 0 {
            session = core.resume(session.park().await?).await?;
        } else if turn == 1 {
            session.close().await?;
            session = core
                .session(crate::SessionId::parse(id).expect("nonblank host identity"))
                .open()
                .await?;
        }
    }
    session.close().await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn builder_configured_tools_and_hooks_are_never_discarded_on_sqlite() -> Result<()> {
    builder_configured_tools_and_hooks_are_never_discarded(sqlite_memory_store_backend().await)
        .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn builder_configured_tools_and_hooks_are_never_discarded_on_postgres() -> Result<()> {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    let backend = lash_conformance::backend_over(stores);
    builder_configured_tools_and_hooks_are_never_discarded(backend).await
}
