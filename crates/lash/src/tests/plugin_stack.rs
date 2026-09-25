use super::*;

const SEED: u64 = 0x5c_f109;
use lash_sansio::SessionId;

struct ShutdownRecordingPluginFactory {
    id: &'static str,
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    failure: Option<&'static str>,
    standard_protocol: bool,
}

struct EmptySessionPlugin {
    id: &'static str,
}

impl lash_core::facade_support::SessionPlugin for EmptySessionPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register(
        &self,
        _reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::PluginFactory for ShutdownRecordingPluginFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn shutdown(&self) -> std::result::Result<(), lash_core::PluginError> {
        self.calls.lock_recover().push(self.id);
        match self.failure {
            Some(message) => Err(lash_core::PluginError::Invoke(message.to_string())),
            None => Ok(()),
        }
    }

    fn build(
        &self,
        ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        if self.standard_protocol {
            return lash_core::facade_support::PluginFactory::build(
                &lash_protocol_standard::StandardProtocolPluginFactory::new(),
                ctx,
            );
        }
        Ok(Arc::new(EmptySessionPlugin { id: self.id }))
    }
}

#[tokio::test]
async fn core_shutdown_visits_protocol_then_common_factories_and_continues_after_error()
-> Result<()> {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        lash_core::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .protocol_plugin(Arc::new(ShutdownRecordingPluginFactory {
        id: "protocol",
        calls: Arc::clone(&calls),
        failure: None,
        standard_protocol: true,
    }))
    .plugin(Arc::new(ShutdownRecordingPluginFactory {
        id: "first",
        calls: Arc::clone(&calls),
        failure: Some("first failed"),
        standard_protocol: false,
    }))
    .plugin(Arc::new(ShutdownRecordingPluginFactory {
        id: "second",
        calls: Arc::clone(&calls),
        failure: None,
        standard_protocol: false,
    }))
    .build(crate::testing::runtime_lease_owner())?;

    let error = core
        .shutdown()
        .await
        .expect_err("first factory failure surfaces");

    assert_eq!(
        *calls.lock_recover(),
        vec!["protocol", "first", "second"],
        "protocol factory is first and a failure does not stop the common-factory walk"
    );
    assert!(error.to_string().contains("first failed"), "{error}");
    Ok(())
}

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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(SurfacePluginFactory))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("plugin-surface").open().await?;
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
async fn embedded_sessions_always_expose_tool_state() -> Result<()> {
    let core = standard_core().await;
    let session = core.session("dynamic-default").open().await?;

    let state = session.admin().tools().state().await?;

    assert!(state.generation() > 0);
    Ok(())
}

#[tokio::test]
async fn registered_static_tools_appear_in_tool_state() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("static-tools").open().await?;

    let state = session.admin().tools().state().await?;

    assert!(state.contains(&lash_core::ToolId::from("tool:app_lookup")));
    Ok(())
}

#[tokio::test]
async fn apply_tool_state_and_membership_update_live_catalog() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("tool-state").open().await?;
    let app_tool = lash_core::ToolId::from("tool:app_lookup");

    // Members by default.
    let initial = session.admin().tools().state().await?;
    assert!(initial.get(&app_tool).expect("app tool").is_member());

    let generation = session
        .admin()
        .tools()
        .set_membership_many(&[(app_tool.clone(), false)])
        .await?;
    let removed = session.admin().tools().state().await?;
    assert_eq!(removed.generation(), generation);
    assert!(!removed.get(&app_tool).expect("app tool").is_member());

    // Re-add as a member.
    let generation = session
        .admin()
        .tools()
        .set_membership("tool:app_lookup", true)
        .await?;
    let restored = session.admin().tools().state().await?;
    assert_eq!(restored.generation(), generation);
    assert!(restored.get(&app_tool).expect("app tool").is_member());

    // Advanced apply_state round-trips membership.
    let mut removed_again = restored;
    removed_again
        .set_membership(&app_tool, false)
        .expect("app tool");
    let generation = session
        .admin()
        .tools()
        .advanced()
        .apply_state(removed_again)
        .await?;
    let applied = session.admin().tools().state().await?;
    assert_eq!(applied.generation(), generation);
    assert!(!applied.get(&app_tool).expect("app tool").is_member());
    Ok(())
}

#[tokio::test]
async fn persisted_session_restores_tool_state() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("persisted-tools").open().await?;
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
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    state.set_tool_state_snapshot(Some(persisted_tool_state));
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let reopened_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend_serving(store).await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;

    let reopened = reopened_core.session("persisted-tools").open().await?;
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
        let projection = Arc::new(
            crate::plugins::ToolOutputBudgetPluginFactory::new(
                crate::plugins::ToolOutputBudgetConfig {
                    mode: crate::plugins::ToolOutputBudgetMode::Bytes,
                    limit: 12,
                    max_lines: 4,
                    head_share_percent: 50,
                    retain_full_output: false,
                },
            )
            .expect("valid tool output budget config"),
        );
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
        let standard_core = explicit_ephemeral_facets(LashCore::standard_builder(
            double.lash_backend(),
            crate::TurnBudget::Unbounded,
        ))
        .provider(standard_provider)
        .model(mock_model_spec())
        .tools(Arc::new(LongTextTools))
        .configure_plugins(|plugins| {
            plugins.replace(projection.clone());
        })
        .build(crate::testing::runtime_lease_owner())?;
        let standard_session = standard_core.session("standard-projection").open().await?;
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
            .find(|content| content.contains("bytes truncated"))
            .expect("projected model observation");
        assert!(model_observation.contains("Re-run the tool with narrower arguments"));
        assert!(!model_observation.contains("Full output saved to:"));

        #[cfg(feature = "rlm")]
        {
            let rlm_core = explicit_ephemeral_facets(rlm_core_builder().await)
                .provider(queued_text_provider(vec![typescript_block(
                    r#"const value = await tools.app_lookup({});
finish("done");"#,
                )]))
                .model(mock_model_spec())
                .tools(Arc::new(LongTextTools))
                .configure_plugins(|plugins| {
                    plugins.replace(projection);
                })
                .build(crate::testing::runtime_lease_owner())?;
            let rlm_session = rlm_core.session("rlm-projection").open().await?;
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
