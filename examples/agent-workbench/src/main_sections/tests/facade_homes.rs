// Facade-home coverage for the surface FIG-863 wave B added: each item below is
// exercised through this host example and asserted on an outcome the runtime
// produced.
    #[test]
    fn host_model_capability_validates_reasoning_effort_selections() {
        use lash::provider::{
            ModelEffortValidationCategory, ModelEffortValidationError, ReasoningCapability,
            ReasoningEncoding, ReasoningSelection,
        };

        let capability = workbench_model_capability();
        let unsupported: ModelEffortValidationError = capability
            .validate_selection(
                "workbench-model",
                "workbench-provider",
                &ReasoningSelection::Effort("ultra".to_string()),
            )
            .expect_err("host capability must reject an unadvertised effort");
        assert_eq!(
            unsupported.category,
            ModelEffortValidationCategory::UnsupportedEffort
        );
        assert!(unsupported.message.contains("Unsupported effort `ultra`"));
        assert_eq!(
            capability
                .validate_selection(
                    "workbench-model",
                    "workbench-provider",
                    &ReasoningSelection::Effort(" HIGH ".to_string()),
                )
                .expect("host capability accepts and normalizes an advertised effort"),
            ReasoningSelection::Effort("high".to_string())
        );

        let not_configurable = lash::provider::ModelCapability::default()
            .validate_selection(
                "plain-model",
                "workbench-provider",
                &ReasoningSelection::Effort("low".to_string()),
            )
            .expect_err("plain model must reject configurable effort");
        assert_eq!(
            not_configurable.category,
            ModelEffortValidationCategory::EffortNotConfigurable
        );
        assert!(not_configurable.message.contains("does not expose configurable effort"));

        let mut required_capability = capability.clone();
        required_capability
            .reasoning
            .as_mut()
            .expect("workbench reasoning capability")
            .mandatory = true;
        let required = required_capability
            .validate_selection(
                "required-model",
                "workbench-provider",
                &ReasoningSelection::ProviderDefault,
            )
            .expect_err("mandatory reasoning must require an explicit effort");
        assert_eq!(required.category, ModelEffortValidationCategory::EffortRequired);
        assert!(required.message.contains("requires an explicit effort"));

        let malformed_capability = lash::provider::ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: vec!["low".to_string(), "high".to_string()],
                default_effort: None,
                aliases: BTreeMap::new(),
                encoding: ReasoningEncoding::Budget(BTreeMap::from([(
                    "low".to_string(),
                    1_024,
                )])),
                disable: None,
                mandatory: false,
            }),
            ..Default::default()
        };
        let malformed = malformed_capability
            .validate_selection(
                "malformed-model",
                "workbench-provider",
                &ReasoningSelection::Effort("low".to_string()),
            )
            .expect_err("budget map must cover every advertised effort");
        assert_eq!(
            malformed.category,
            ModelEffortValidationCategory::MalformedCapability
        );
        assert!(malformed.message.contains("missing advertised effort `high`"));
    }

    #[test]
    fn durable_effect_boundary_rejects_live_protocol_turn_input() {
        use lash::rlm::RlmTurnInputExt as _;

        let plain = lash::TurnInput::text("durable plain input");
        lash::durability::ensure_durable_effect_input(&plain)
            .expect("plain turn input is replayable");

        let live = lash::TurnInput::text("durable projected input")
            .rlm_project(
                lash::rlm::RlmProjectedBindings::new()
                    .bind_json("live_value", json!({"answer": 42}))
                    .expect("bind live projected input"),
            )
            .expect("attach live RLM projection");
        let rejection = lash::durability::ensure_durable_effect_input(&live)
            .expect_err("live protocol extensions cannot cross a durable effect boundary");
        assert_eq!(
            rejection.code,
            lash::runtime::RuntimeErrorCode::DurableEffectLiveProtocolExtension
        );
        assert!(rejection.message.contains("live protocol_extension inputs"));
    }

    #[test]
    fn workbench_plugin_observes_session_config_policy_transition() {
        run_async_test_on_stack_budget("workbench-config-change-context-test", || async {
            let data_dir = std::env::temp_dir().join(format!(
                "agent-workbench-config-change-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&data_dir).expect("create config change data dir");
            let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                data_dir.join("lash-sessions"),
            )) as Arc<dyn lash::persistence::SessionStoreFactory>;
            let process_registry = Arc::new(
                lash_sqlite_store::SqliteProcessRegistry::open(
                    &data_dir.join("processes.db"),
                    data_dir.join("lash-sessions"),
                )
                .await
                .expect("open config change process registry"),
            ) as Arc<dyn lash::process::ProcessRegistry>;
            let plugin = Arc::new(WorkbenchPluginFactory::new(""));
            let config_changes = plugin.config_changes();
            let provider = lash::testing::TestProvider::builder()
                .kind("workbench-config-change-provider")
                .complete_error("config patch test should not call the provider")
                .build()
                .into_handle();
            let initial_model = lash::ModelSpec::from_token_limits(
                "workbench-model-before",
                Default::default(),
                4_096,
                None,
            )
            .expect("initial config change model");
            let core = explicit_durable_test_facets(&data_dir)
                .provider(provider)
                .model(initial_model)
                .plugin(plugin)
                .store_factory(store_factory)
                .process_registry(Arc::clone(&process_registry))
                .disable_queued_work_driver()
                .build()
                .expect("build config change workbench core");
            let session = core
                .session("workbench-config-change-session")
                .open()
                .await
                .expect("open config change session");
            let patched_model = lash::ModelSpec::from_token_limits(
                "workbench-model-after",
                Default::default(),
                8_192,
                None,
            )
            .expect("patched config change model");
            session
                .configure(lash::SessionConfigPatch {
                    model: Some(patched_model),
                    ..Default::default()
                })
                .await
                .expect("patch workbench session model");

            assert_eq!(
                config_changes.latest(),
                Some(WorkbenchConfigChange {
                    session_id: "workbench-config-change-session".to_string(),
                    previous_model_id: "workbench-model-before".to_string(),
                    current_model_id: "workbench-model-after".to_string(),
                    service_model_id: "workbench-model-after".to_string(),
                })
            );
            assert_eq!(
                session.policy_snapshot().model_id(),
                "workbench-model-after"
            );
            session.close().await.expect("close config change session");
            drop(core);
            drop(process_registry);
            std::fs::remove_dir_all(&data_dir).expect("remove config change data dir");
        });
    }

    #[test]
    fn workbench_context_transform_shapes_the_prompt_the_provider_receives() {
        run_async_test_on_stack_budget("workbench-context-transform-test", || async {
            let data_dir = std::env::temp_dir().join(format!(
                "agent-workbench-context-transform-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&data_dir).expect("create context transform data dir");
            let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                data_dir.join("lash-sessions"),
            )) as Arc<dyn lash::persistence::SessionStoreFactory>;
            let process_registry = Arc::new(
                lash_sqlite_store::SqliteProcessRegistry::open(
                    &data_dir.join("processes.db"),
                    data_dir.join("lash-sessions"),
                )
                .await
                .expect("open context transform process registry"),
            ) as Arc<dyn lash::process::ProcessRegistry>;
            let plugin = Arc::new(WorkbenchPluginFactory::new(""));
            let context_budget = plugin.context_budget();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_for_provider = Arc::clone(&requests);
            let provider = lash::testing::TestProvider::builder()
                .kind("workbench-context-transform-provider")
                .complete(move |request| {
                    let requests = Arc::clone(&requests_for_provider);
                    async move {
                        requests
                            .lock()
                            .expect("context transform request lock")
                            .push(request);
                        Ok(text_response(
                            "<lashlang>\nfinish \"context shaped\"\n</lashlang>",
                        ))
                    }
                })
                .build()
                .into_handle();
            let core = explicit_durable_test_facets(&data_dir)
                .provider(provider)
                .model(
                    lash::ModelSpec::from_token_limits(
                        "workbench-context-transform-model",
                        Default::default(),
                        4_096,
                        None,
                    )
                    .expect("context transform model"),
                )
                .plugin(plugin)
                .store_factory(store_factory)
                .process_registry(Arc::clone(&process_registry))
                .disable_queued_work_driver()
                .build()
                .expect("build context transform workbench core");
            let session = core
                .session("workbench-context-transform-session")
                .open()
                .await
                .expect("open context transform session");
            session
                .turn(lash::TurnInput::text("shape my context"))
                .require_finish()
                .expect("require finish")
                .run()
                .await
                .expect("run the context transform turn");

            // The transform ran against the context the runtime actually
            // assembled: one prepared message for a first turn, base tools on,
            // and nothing committed yet when the prompt was built.
            let observation = context_budget
                .observation()
                .expect("the registered transform must have run for this turn");
            assert_eq!(
                observation.session_id,
                "workbench-context-transform-session"
            );
            assert_eq!(observation.message_count, 1);
            assert_eq!(observation.committed_message_count, 0);
            assert!(observation.include_base_tools);
            // `tool_providers` is the transform's own contribution channel, not a
            // view of the plugin-registered catalog: the runtime hands it empty
            // and a transform pushes turn-scoped providers into it.
            assert_eq!(observation.tool_provider_count, 0);
            // No prior render on a first turn, so there is no prompt usage to
            // budget against yet.
            assert_eq!(observation.last_prompt_context_tokens, None);
            assert_eq!(observation.max_context_tokens, Some(4_096));

            // And its contribution is in the prompt the provider was handed —
            // the transform's output is not merely recorded, it is rendered.
            let rendered = {
                let captured = requests.lock().expect("context transform request lock");
                assert_eq!(captured.len(), 1, "one turn must make one provider call");
                serde_json::to_string(&*captured)
                    .expect("serialize the provider request the runtime issued")
            };
            assert!(
                rendered.contains("prepared 1 message(s) from 0 committed; base tools on"),
                "the transform's contribution must reach the prompt the provider received"
            );
            assert!(
                rendered.contains("Context budget"),
                "the transform's contribution title must survive prompt rendering"
            );

            session.close().await.expect("close context transform session");
            drop(core);
            drop(process_registry);
            std::fs::remove_dir_all(&data_dir).expect("remove context transform data dir");
        });
    }
