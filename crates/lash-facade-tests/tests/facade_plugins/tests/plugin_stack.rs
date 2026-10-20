use super::*;

use lash::{TurnEvent, TurnInput};
use lash_execution_contracts::backend::Backend;

/// DUPLICATE-PLUGIN-ID-REFUSED: distinct provider and callback keys do not
/// permit two factories to own one namespace.
#[tokio::test]
async fn duplicate_plugin_id_refused() -> Result<()> {
    use lash::plugins::{PluginComposition, PluginDeclaration, PluginDeclarationError, PluginSpec};
    let factory = |key: lash::plugins::ToolProviderKey| {
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial("duplicate-owner"),
            PluginSpec::new()
                .with_tool_provider(key, Arc::new(AppTools))
                .with_before_turn(
                    lash::plugins::HookKey::new(key.as_str()).unwrap(),
                    Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
                ),
        )) as Arc<dyn PluginFactory>
    };
    let first = factory(lash::tool_provider_key!("first"));
    let second = factory(lash::tool_provider_key!("second"));
    let result = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .plugin(first.clone())
    .plugin(second.clone())
    .build(lash_facade_tests::runtime_lease_owner());
    assert!(
        matches!(result, Err(EmbedError::PluginDeclaration(PluginDeclarationError::DuplicatePluginId { id })) if id == "duplicate-owner"),
        "duplicate plugin factories must refuse before registration"
    );
    let factories = vec![first, second.clone()];
    let duplicate = PluginDeclarationError::DuplicatePluginId {
        id: "duplicate-owner".into(),
    };
    assert_eq!(
        PluginComposition::new(factories.iter().map(|factory| factory.plugin_declaration()))
            .unwrap_err(),
        duplicate
    );
    assert_eq!(
        PluginComposition::with_builtins(
            factories.iter().map(|factory| factory.plugin_declaration())
        )
        .unwrap_err(),
        duplicate
    );
    assert_eq!(
        match lash::plugins::ConfigRegistry::build(&factories).unwrap_err() {
            lash::plugins::PluginError::ConfigRegistration(error) => error,
            other => panic!("typed config refusal: {other}"),
        },
        lash::plugins::ConfigRegistrationError::DuplicatePluginId {
            id: "duplicate-owner".into()
        }
    );
    let host = lash::plugins::PluginHost::new(
        factories.clone(),
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(Arc::new(lash::runtime::SystemClock)),
    );
    assert!(
        matches!(host.build_session(lash_core::plugin::PluginSessionRequest::creation(
        "duplicate-owner", lash_core::plugin::SessionAuthorityContext::fixture(),
    )), Err(lash_execution_contracts::plugin_error::PluginError::Declaration(error)) if error == duplicate)
    );
    let record = host.transition_plugins(
        lash::plugins::PluginTransitionRequest {
            id: lash::plugins::PluginTransitionId(
                lash_core::EffectAddress::new(
                    lash_core::ExecutionScope::turn("duplicate-owner", "run"),
                    "transition",
                )
                .unwrap(),
            ),
            owner: lash_core::RuntimeOwner::Session(SessionId::fixture("duplicate-owner")),
            base: lash::plugins::PluginTransitionBase::Session {
                head: lash_core::store::SessionHeadRef {
                    generation: 0,
                    revision: 0,
                    leaf: None,
                    checkpoint: None,
                },
            },
            target: Default::default(),
        },
        &Default::default(),
        &Default::default(),
    );
    assert!(
        matches!(record.candidate(), Err(lash_execution_contracts::plugin_error::PluginError::Declaration(error)) if error == duplicate)
    );
    let mut stack = lash::PluginStack::from_factories([factories[0].clone()]);
    stack.replace(second);
    assert_eq!(
        PluginComposition::new(
            stack
                .factories()
                .iter()
                .map(|factory| factory.plugin_declaration())
        )
        .unwrap()
        .declarations()
        .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn plugin_surface_streams_as_semantic_turn_event() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .plugin(Arc::new(SurfacePluginFactory))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("plugin-surface").expect("nonblank host identity"),
    )
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

/// Runs on the test thread's default 2 MiB stack, the budget a host's
/// runtime thread gets.
#[tokio::test]
async fn tool_completed_activity_is_canonical_while_model_observation_is_projected() -> Result<()> {
    {
        let mut standard_config = lash::plugins::StandardProtocolConfig::standard();
        standard_config.render.defaults.value.max_chars = Some(32);
        let observed_tool_results = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let observed_tool_results_provider = Arc::clone(&observed_tool_results);
        let responses = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from([
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "call-1".to_string(),
                    tool_name: "tools__app_lookup".to_string(),
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
                                    lash_llm_transport::tool_result_text(content).into_owned(),
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
                lash::plugins::StandardProtocolPluginFactory::with_config(standard_config),
            )))
            .serve_test_llm_profile(standard_provider, mock_llm_profile_spec())
            .tools(lash::tool_provider_key!("tools"), Arc::new(LongTextTools))
            .build(lash_facade_tests::runtime_lease_owner())?;
        let standard_session = created_session(
            &standard_core,
            lash::SessionId::parse("standard-projection").expect("nonblank host identity"),
        )
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
            let rlm_factory = rlm_factory();
            let rlm_core =
                explicit_ephemeral_facets(LashCore::rlm_builder(rlm_backend, rlm_factory))
                    .serve_test_llm_profile(
                        queued_text_provider(vec![typescript_block(
                            r#"const value = await tools.appLookup({});
await control.finish("done");"#,
                        )]),
                        mock_llm_profile_spec(),
                    )
                    .tools(lash::tool_provider_key!("tools"), Arc::new(LongTextTools))
                    .build(lash_facade_tests::runtime_lease_owner())?;
            let rlm_session = created_session(
                &rlm_core,
                lash::SessionId::parse("rlm-projection").expect("nonblank host identity"),
            )
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
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        AppTools.tool_manifests(config)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        AppTools.resolve_contract(name, config)
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
                        tool_name: "tools__app_lookup".to_string(),
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
    let provider = lash::testing::TestProvider::builder()
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
        .tools(
            lash::tool_provider_key!("tools"),
            Arc::new(BuilderSentinelTools {
                calls: Arc::clone(&calls),
            }),
        )
        .plugin(Arc::new(SurfacePluginFactory))
        .build(lash_facade_tests::runtime_lease_owner())?;
    let id = "builder-sentinels";
    let mut session = created_session(
        &core,
        lash::SessionId::parse(id).expect("nonblank host identity"),
    )
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
                .session(lash::SessionId::parse(id).expect("nonblank host identity"))
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

/// FIG-6490: every shipped mode documents schema contracts without an
/// authored-example row, and refuses removed example settings at admission
/// and when reading a recorded config.
#[test]
fn shipped_tool_presenters_have_no_authored_examples() {
    use lash::tools::{
        ToolDisclosure, ToolPresentationEntry, ToolPresentationRequest, ToolPresenter,
    };
    let definition = lash::tools::ToolDefinition::raw(
        "tool:lookup", "tools.lookup", "Lookup a record",
        serde_json::json!({"type":"object", "properties":{"query":{"type":"string", "description":"Search text"}}, "required":["query"]}),
        serde_json::json!({"type":"string"}),
    ).expect("schemas").with_execution(std::time::Duration::from_secs(30));
    let manifest = definition.manifest();
    let contract = definition.contract();
    let entries = [ToolPresentationEntry {
        manifest: &manifest,
        contract: &contract,
        call_path: "tools.lookup",
        pinned: false,
    }];
    let standard = lash::standard::StandardToolPresenter;
    #[cfg_attr(
        not(feature = "rlm"),
        expect(unused_mut, reason = "RLM builds add both dialect presenters")
    )]
    let mut presenters: Vec<&dyn ToolPresenter> = vec![&standard];
    #[cfg(feature = "rlm")]
    let (typescript, python) = (
        lash::rlm::typescript_tool_presenter::TypescriptToolPresenter,
        lash::rlm::python_tool_presenter::PythonToolPresenter,
    );
    #[cfg(feature = "rlm")]
    presenters.extend([
        &typescript as &dyn ToolPresenter,
        &python as &dyn ToolPresenter,
    ]);
    for presenter in presenters {
        let config = presenter
            .admit_config(&serde_json::json!({}))
            .expect("default config");
        let section = presenter
            .present(ToolPresentationRequest {
                tools: &entries,
                config: &config,
                disclosure: ToolDisclosure::Complete,
                reserved_names: &[],
                max_section_bytes: None,
            })
            .expect("tool docs");
        let discovered = presenter
            .tool_doc(&entries[0], &config)
            .expect("discovery doc");
        assert!(section.text.contains("lookup"));
        for doc in [&section.text, &discovered] {
            assert!(
                !doc.lines().any(|row| row.starts_with("Examples:")),
                "{doc}"
            );
        }
        assert!(
            serde_json::to_value(&contract)
                .expect("contract serializes")
                .get("examples")
                .is_none()
        );
        for field in ["example_limit", "example_chars", "examples", "schema_depth"] {
            let mut legacy = config.clone();
            legacy[field] = serde_json::json!(0);
            let refused = presenter
                .admit_config(&legacy)
                .expect_err("removed fields are refused");
            assert!(
                refused
                    .to_string()
                    .contains(&format!("unknown field `{field}`")),
                "{refused}"
            );
            assert!(presenter.tool_doc(&entries[0], &legacy).is_err());
        }
    }
}

/// A host's catalogue vocabulary and zero disclosure limits are honored.
#[cfg(feature = "rlm")]
#[test]
fn facade_catalogue_preview_honors_host_vocabulary_and_limits() {
    let preview = lash::tools::catalogue_preview(
        [lash::tools::CataloguePreviewEntry::new(
            lash::tools::IntoOperation::into_operation("tools.hidden_module.hidden_call")
                .expect("a canonical operation"),
        )],
        &lash::tools::CataloguePreviewOptions {
            title: "Host catalogue".into(),
            search_call_path: "tools.find".into(),
            module_limit: 0,
            call_name_limit: 0,
            ..lash::tools::CataloguePreviewOptions::default()
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
    let mut config = lash::rlm::RlmProtocolPluginConfig::standard()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
    config.presentation.prompt_max_tool_calls = 0;
    config.presentation.max_inline_keys = 0;
    config.prompt_features.decomposition = false;
    config.continue_as_soft_warn_tokens = None;
    config.render.print.max_chars = Some(9);
    config.render.preview.max_chars = Some(9);
    config.max_output_chars = 500;

    let recorded = config
        .recorded_behaviour(&lash::rlm::CellDialect::typescript())
        .expect("the standard tool presentation admits");
    assert_eq!(recorded.presentation, config.presentation);
    let factory =
        lash::rlm::RlmProtocolPluginFactory::new(config, lash::rlm::CellDialect::typescript())
            .with_worker_service(untimed_fixture_workers());
    let (provider, requests) =
        super::standard_compaction_persistence::standard_compaction_provider_recorded(vec![
            text_response(&typescript_block(
                r#"const mapped = new Map([["a", 1], ["b", 2], ["c", 3]]);
const object = { first: 1, second: 2 };
await tools.appLookup({});
console.log("abcdefghijklmnopqrstuvwxyz");"#,
            )),
            text_response(&typescript_block(
                r#"await control.finish("complete-value");"#,
            )),
        ]);
    let cuts = lash::RuntimeOutputCuts {
        value_reply_max_chars: 3,
        raw_error_max_chars: 2,
    };
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .output_cuts(cuts)
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(&core, SessionId::from("host-rlm-presentation"))
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("use the host presentation"))
        .output()
        .await?;
    assert_eq!(
        output.finished().map(|(_, value)| value),
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
            .find_map(|line| line.strip_prefix("- `mapped`: "))
            .expect("map binding in the next real prompt");
        assert!(
            // A kernel map with text keys has a JSON record view. The
            // configured preview and key cuts apply to it like other data.
            summary.contains("keys=3 (see Schema)")
                && summary
                    .split_once(" ≈ ")
                    .expect("the map has a bounded preview")
                    .1
                    .chars()
                    .count()
                    == 9
                && !summary.contains("keys=3 (a, b, c)"),
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
    let admitted: lash::rlm::RlmRecordedConfig = serde_json::from_value(
        snapshot
            .plugin_config
            .get(lash::rlm::RLM_PROTOCOL_PLUGIN_ID)
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
        serde_json::from_value::<lash::rlm::RlmRecordedBehaviour>(omission).is_err(),
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
    assert!(protocol.contains("tools.app_lookup"), "{protocol}");
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
    let recorded = lash::plugins::StandardProtocolConfig::standard()
        .recorded_behaviour()
        .expect("the standard tool presentation admits");
    let mut omission = serde_json::to_value(&recorded).expect("behaviour");
    omission["render"]["defaults"]["value"]
        .as_object_mut()
        .expect("patch")
        .remove("max_chars");
    assert!(serde_json::from_value::<lash::standard::StandardRecordedBehaviour>(omission).is_err());
    let mut omission = serde_json::to_value(&recorded).expect("behaviour");
    omission
        .as_object_mut()
        .expect("behaviour")
        .remove("discovery_operation");
    assert!(serde_json::from_value::<lash::standard::StandardRecordedBehaviour>(omission).is_err());
    assert!(
        lash::render::StandardRenderConfig::default()
            .defaults
            .value
            .is_empty()
    );
    #[cfg(feature = "rlm")]
    {
        let recorded = lash::rlm::RlmProtocolPluginConfig::standard()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::unbounded())
            .memory_limit(lash::rlm::MemoryBound::unbounded())
            .build()
            .recorded_behaviour(&lash::rlm::CellDialect::typescript())
            .expect("the standard tool presentation admits");
        let mut omission = serde_json::to_value(&recorded).expect("behaviour");
        omission["render"]["print"]
            .as_object_mut()
            .expect("patch")
            .remove("max_chars");
        assert!(serde_json::from_value::<lash::rlm::RlmRecordedBehaviour>(omission).is_err());
        for field in ["continue_as_soft_warn_tokens", "discovery"] {
            let mut omission = serde_json::to_value(&recorded).expect("behaviour");
            omission.as_object_mut().expect("behaviour").remove(field);
            assert!(serde_json::from_value::<lash::rlm::RlmRecordedBehaviour>(omission).is_err());
        }
    }
}

/// Recording the base must preserve a per-tool patch's inheritance from run defaults.
#[test]
fn facade_recording_preserves_per_tool_inheritance() {
    let id = lash::tools::ToolId::new("host-tool");
    let mut host = lash::plugins::StandardProtocolConfig::standard();
    host.render.per_tool.insert(
        id.clone(),
        lash::render::ToolRenderPatch {
            max_lines: Some(2),
            ..Default::default()
        },
    );
    let recorded = host
        .recorded_behaviour()
        .expect("the standard tool presentation admits");
    let mut run = lash::render::StandardRenderConfig::default();
    run.defaults.value.max_chars = Some(11);
    let resolved = lash::render::resolve(
        &lash::render::StandardRenderConfig::standard(),
        &recorded.render,
        &run,
    )
    .expect("valid run preference");
    assert_eq!(resolved.for_tool(&id).value.max_chars, 11);
    assert_eq!(resolved.for_tool(&id).max_lines, 2);
}
