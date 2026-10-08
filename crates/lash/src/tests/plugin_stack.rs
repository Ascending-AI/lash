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
        let mut standard_config = crate::plugins::StandardProtocolConfig::standard();
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

/// D-DEFAULTS2: compact contracts and catalogue advertisements use the facade's
/// non-default presentation values, including their cached render identity.
#[test]
fn facade_compact_and_catalogue_policies_change_the_rendered_view() {
    let definition = crate::tools::ToolDefinition::raw(
        "display", "display", "display contract",
        serde_json::json!({"type":"object", "properties":{"nested":{"type":"object", "properties":{"deep":{"type":"string"}}}}}),
        serde_json::json!({"type":"string"}),
    ).expect("schemas").with_examples(vec!["abcdefghijklmnopqrstuvwxyz".into(), "second".into()]);
    let contract = definition.contract();
    let manifest = definition.manifest();
    let base = contract.compact_contract_with_presentation(
        &manifest,
        "display",
        &crate::tools::ToolPresentationConfig::standard(),
    );
    let selected = crate::tools::ToolPresentationConfig {
        example_limit: 1,
        example_chars: 8,
        schema_depth: 0,
    };
    let compact = contract.compact_contract_with_presentation(&manifest, "display", &selected);
    assert_eq!(compact.examples.len(), 1);
    assert!(compact.examples[0].chars().count() <= 8);
    assert_ne!(compact.signature, base.signature);
    assert_eq!(
        contract.compact_contract_with_presentation(&manifest, "display", &selected),
        compact
    );
    let preview = crate::tools::catalogue_preview(
        [crate::tools::CataloguePreviewEntry::new(
            ["tools", "hidden_module"],
            "hidden_call",
        )],
        &crate::tools::CataloguePreviewOptions {
            title: "Host catalogue".into(),
            search_call_path: "tools.find".into(),
            module_limit: 0,
            call_name_limit: 0,
        },
    )
    .expect("nonempty catalogue");
    assert!(preview.contains("Host catalogue") && preview.contains("tools.find"));
    assert!(!preview.contains("hidden_module") && !preview.contains("hidden_call"));
}

/// D-DEFAULTS2: the facade's RLM presentation reaches the running worker and
/// both its prompt and transcript projections; the value reply uses the
/// independently selected runtime output cut while the outcome stays whole.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn facade_rlm_presentation_and_runtime_cuts_reach_the_running_turn() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let mut config = crate::rlm::RlmProtocolPluginConfig::standard()
        .channel(crate::rlm::RlmChannel::Cell)
        .instruction_limit(crate::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(crate::rlm::MemoryBound::mebibytes(64))
        .build();
    config.presentation.binding_summary = crate::rlm::lang::BindingSummaryConfig {
        members: 1,
        depth: 1,
        max_chars: 18,
    };
    config.presentation.max_tool_call_records = 0;
    config.presentation.max_inline_scalar_bytes = 2;
    config.presentation.max_inline_keys = 0;
    config.prompt_features.decomposition = false;
    config.continue_as_soft_warn_tokens = None;
    config.render.print.max_chars = Some(9);
    config.max_output_chars = 500;

    let recorded = config.recorded_behaviour();
    assert_eq!(recorded.presentation, config.presentation);
    let factory = crate::rlm::RlmProtocolPluginFactory::new(
        config,
        Arc::new(crate::rlm::TypescriptDialect),
        &backend,
    )
    .with_worker_service(untimed_fixture_workers());
    let (provider, requests) =
        super::standard_compaction_persistence::standard_compaction_provider_recorded(vec![
            text_response(&typescript_block(
                r#"const opaque = new Map([["a", 1], ["b", 2], ["c", 3]]);
const object = { first: 1, second: 2 };
await tools.app_lookup({});
console.log("abcdefghijklmnopqrstuvwxyz");"#,
            )),
            text_response(&typescript_block(r#"finish("complete-value");"#)),
        ]);
    let cuts = crate::RuntimeOutputCuts {
        value_reply_max_chars: 3,
        raw_error_max_chars: 2,
    };
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .output_cuts(cuts)
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::from("host-rlm-presentation"))
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("use the host presentation"))
        .output()
        .await?;
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("complete-value"))
    );
    {
        let captured = requests.lock_recover();
        assert_eq!(captured.len(), 2);
        let messages: Vec<lash_core::llm::types::LlmMessage> = serde_json::from_str(&captured[1])?;
        let text = messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let summary = text
            .lines()
            .find_map(|line| line.strip_prefix("- `opaque`: "))
            .expect("opaque binding in the next real prompt");
        assert!(
            summary.contains("Map(3)") && summary.chars().count() <= 18,
            "{summary}"
        );
    }
    let read = committed(&session).await;
    assert!(
        read.messages()
            .iter()
            .any(|message| message.parts.iter().any(|part| part.content() == "com…"))
    );
    let snapshot = read.to_snapshot();
    let admitted: crate::rlm::RlmRecordedConfig = serde_json::from_value(
        snapshot
            .plugin_config
            .get(crate::rlm::RLM_PROTOCOL_PLUGIN_ID)
            .expect("recorded RLM namespace")
            .clone(),
    )?;
    assert_eq!(
        admitted.behaviour, recorded,
        "creation preserves the full presentation"
    );
    let mut omission = serde_json::to_value(&recorded)?;
    omission
        .as_object_mut()
        .expect("behaviour object")
        .remove("presentation");
    assert!(
        serde_json::from_value::<crate::rlm::RlmRecordedBehaviour>(omission).is_err(),
        "cold reopen cannot invent an omitted presentation choice"
    );
    let protocol = read
        .active_events()
        .iter()
        .filter_map(|event| match event {
            lash_core::SessionHistoryRecord::Protocol(event) => Some(event.payload.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(protocol.contains("\"calls_omitted\":1"), "{protocol}");
    assert!(
        protocol.contains("within 9;"),
        "the selected print cut rendered the running cell: {protocol}"
    );
    assert!(
        protocol.contains("\"value\":\"abcdefghijklmnopqrstuvwxyz\""),
        "the typed print stays whole"
    );
    let envelope = lash_core::session_model::make_error_envelope(
        lash_core::TurnFailureKind::RuntimeEffectController,
        None,
        None,
        "host error",
        Some("abcdef".into()),
        cuts,
    );
    assert!(envelope.raw.expect("raw").contains("4 chars omitted"));
    core.shutdown().await?;
    Ok(())
}

/// Replay requires resolved bases; an empty authored patch remains inheritance.
#[test]
fn facade_recorded_render_refuses_omitted_base_choices() {
    let recorded = crate::plugins::StandardProtocolConfig::standard().recorded_behaviour();
    let mut omission = serde_json::to_value(&recorded).expect("behaviour");
    omission["render"]["defaults"]["value"]
        .as_object_mut()
        .expect("patch")
        .remove("max_chars");
    assert!(
        serde_json::from_value::<crate::standard::StandardRecordedBehaviour>(omission).is_err()
    );
    let mut omission = serde_json::to_value(&recorded).expect("behaviour");
    omission
        .as_object_mut()
        .expect("behaviour")
        .remove("discovery_operation");
    assert!(
        serde_json::from_value::<crate::standard::StandardRecordedBehaviour>(omission).is_err()
    );
    assert!(
        crate::render::StandardRenderConfig::default()
            .defaults
            .value
            .is_empty()
    );
    #[cfg(feature = "rlm")]
    {
        let recorded = crate::rlm::RlmProtocolPluginConfig::standard()
            .channel(crate::rlm::RlmChannel::Cell)
            .instruction_limit(crate::rlm::InstructionBound::unbounded())
            .memory_limit(crate::rlm::MemoryBound::unbounded())
            .build()
            .recorded_behaviour();
        let mut omission = serde_json::to_value(&recorded).expect("behaviour");
        omission["render"]["print"]
            .as_object_mut()
            .expect("patch")
            .remove("max_chars");
        assert!(serde_json::from_value::<crate::rlm::RlmRecordedBehaviour>(omission).is_err());
        for field in ["continue_as_soft_warn_tokens", "discovery_operation"] {
            let mut omission = serde_json::to_value(&recorded).expect("behaviour");
            omission.as_object_mut().expect("behaviour").remove(field);
            assert!(serde_json::from_value::<crate::rlm::RlmRecordedBehaviour>(omission).is_err());
        }
    }
}

/// Recording the base must preserve a per-tool patch's inheritance from run defaults.
#[test]
fn facade_recording_preserves_per_tool_inheritance() {
    let id = crate::tools::ToolId::new("host-tool");
    let mut host = crate::plugins::StandardProtocolConfig::standard();
    host.render.per_tool.insert(
        id.clone(),
        crate::render::ToolRenderPatch {
            max_lines: Some(2),
            ..Default::default()
        },
    );
    let recorded = host.recorded_behaviour();
    let mut run = crate::render::StandardRenderConfig::default();
    run.defaults.value.max_chars = Some(11);
    let resolved = crate::render::resolve(
        &crate::render::StandardRenderConfig::standard(),
        &recorded.render,
        &run,
    )
    .expect("valid run preference");
    assert_eq!(resolved.for_tool(&id).value.max_chars, 11);
    assert_eq!(resolved.for_tool(&id).max_lines, 2);
}
