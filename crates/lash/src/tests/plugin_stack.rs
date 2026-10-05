use super::*;

const SEED: u64 = 0x5c_f109;
use lash_core::Backend;
use lash_sansio::SessionId;

fn persisted_tool_state_at_generation(
    state: lash_core::ToolState,
    generation: u64,
) -> lash_core::ToolState {
    let mut value = serde_json::to_value(state).expect("serialize persisted tool state");
    value["generation"] = serde_json::json!(generation);
    serde_json::from_value(value).expect("deserialize persisted tool state")
}

#[tokio::test]
async fn plugin_surface_streams_as_semantic_turn_event() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
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

#[tokio::test]
async fn persisted_session_restores_tool_state() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("persisted-tools").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session
        .admin()
        .tools()
        .set_membership("tool:app_lookup", false)
        .await?;
    let persisted_tool_state =
        persisted_tool_state_at_generation(session.admin().tools().state().await?, 9);
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("persisted-tools"),
        policy: lash_core::SessionPolicy {
            model: Some(recorded_llm_profile(mock_llm_profile_spec())),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.set_tool_state_snapshot(Some(persisted_tool_state));
    let (backend, _) = backend_seeded(state).await;
    let reopened_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;

    let reopened = reopened_core
        .session(crate::SessionId::parse("persisted-tools").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let state = reopened.admin().tools().state().await?;
    assert_eq!(state.generation(), 9);

    assert!(
        !state
            .get(&lash_core::ToolId::from("tool:app_lookup"))
            .expect("app tool")
            .is_member(),
        "the host-removed tool is restored as a non-member"
    );
    Ok(())
}

#[test]
fn tool_completed_activity_is_canonical_while_model_observation_is_projected() -> Result<()> {
    run_async_test_on_stack_budget("tool-projection-stack-test", || async {
        let mut standard_config = crate::plugins::StandardProtocolConfig::default();
        standard_config.render.defaults.value.max_chars = Some(32);
        let observed_tool_results = Arc::new(TokioMutex::new(Vec::<String>::new()));
        let observed_tool_results_provider = Arc::clone(&observed_tool_results);
        let responses = Arc::new(TokioMutex::new(VecDeque::from([
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
        let double = restate_double(SEED).await;
        let standard_core = explicit_ephemeral_facets(
            LashCore::builder(double.lash_backend()).protocol_plugin(Arc::new(
                crate::plugins::StandardProtocolPluginFactory::with_config(standard_config),
            )),
        )
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
            let rlm_core = explicit_ephemeral_facets(rlm_core_builder().await)
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
    })
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
    let responses = Arc::new(TokioMutex::new(VecDeque::from_iter((0..3).flat_map(
        |turn| {
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
        },
    ))));
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
        settle_session_shift(&core, id).await;
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
    builder_configured_tools_and_hooks_are_never_discarded(double_backend().await).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the gate's database URL is host configuration.
async fn builder_configured_tools_and_hooks_are_never_discarded_on_postgres() -> Result<()> {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL gate URL");
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url()).await?;
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    )) as Arc<dyn lash_core::StoreSet>;
    let backend =
        double_backend_over(lash_restate_test::ServerConfig::default(), move |_| stores).await;
    builder_configured_tools_and_hooks_are_never_discarded(backend).await
}
