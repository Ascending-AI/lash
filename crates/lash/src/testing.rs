//! Test helpers for embedders. Enable with `lash = { ..., features = ["testing"] }`
//! to script model responses in integration tests without a live provider.

pub use lash_core::TestLocalProcessRegistry;
pub use lash_core::testing::{TestProvider, TestProviderBuilder};

/// The normalized behavior-transcript vocabulary. Render a scenario's real facts
/// into it and pin the result with an inline `insta` snapshot; see
/// `docs/adr/0050-behavior-transcripts-are-one-normalized-vocabulary.md`.
pub use lash_core::testing::behavior_transcript;

/// Store-factory decorator that observes accepted runtime-checkpoint commits, so
/// a scenario can render durable-write transcript lines from real facts.
pub use lash_core::testing::checkpoint_observer;

#[cfg(feature = "rlm")]
pub mod conformance {
    //! Backend-agnostic conformance suites: validate a custom backend
    //! implementation against a contract by running the same suite the in-tree
    //! backends run.
    //!
    //! Re-exports all lash-core trait suites, including process registry,
    //! runtime persistence, store-recovery, trace-derived real-turn crash,
    //! effect-host, trigger, attachment, artifact, and state-machine
    //! conformance, and adds
    //! [`runtime_rebuild_and_worker_recovery`] — a runtime-level suite
    //! that proves cold session rebuild and durable worker recovery use the
    //! same reconstructed runtime surface.

    pub use lash_core::testing::conformance::*;

    use std::sync::Arc;
    use std::time::Duration;

    use crate::LashCore;
    use crate::LashCoreBuilder;
    use crate::plugins::{
        PluginError, PluginExtensionContribution, PluginFactory, PluginRegistrar,
        PluginSessionContext, SessionPlugin,
    };
    use crate::rlm::{
        LASHLANG_SURFACE_EXTENSION_ID, LashlangLanguageFeatures, LashlangSurfaceContribution,
    };
    use crate::testing::TestProvider;
    use futures_util::StreamExt as _;
    use lash_lashlang_runtime::{LashlangArtifactStore, ToolDefinitionLashlangExt};

    /// Stores + registry for one run of the
    /// [`runtime_rebuild_and_worker_recovery`] suite.
    ///
    /// `build_core` receives a builder pre-loaded with the mode, provider, model,
    /// plugins, and `process_registry`, and must wire the stores (and, for a
    /// durable store factory, an effect controller) and `build()`. `process_registry`
    /// is the same registry the builder is given, retained so the suite can drive
    /// and await processes. `make` in
    /// [`runtime_rebuild_and_worker_recovery`] must return a fresh backend
    /// (fresh stores) on each call.
    pub struct RuntimeRebuildBackend {
        pub process_registry: Arc<dyn lash_core::ProcessRegistry>,
        /// The Lashlang artifact store, now a construction-time input to the RLM
        /// protocol factory. The backend supplies it here so `base_builder` can
        /// seed a tier-consistent factory.
        pub artifact_store: Arc<dyn LashlangArtifactStore>,
        pub build_core: Box<dyn Fn(LashCoreBuilder) -> LashCore + Send + Sync>,
    }

    /// Run the full cold-rebuild + worker-recovery conformance suite against
    /// the backend produced by `make`. `make` must return a fresh backend on
    /// each call.
    ///
    /// Each scenario registers a Lashlang trigger route through the runtime
    /// `triggers` module, drops and reopens the session (a restart from cold
    /// durable storage), then drives an out-of-turn process through the
    /// lease-protected worker. The worker reconstructs the session purely from
    /// persisted state; the suite asserts that trigger registry state and the
    /// process runtime surface survive rebuild across the
    /// [`ProcessInput`](lash_core::ProcessInput) variants the worker runs.
    pub async fn runtime_rebuild_and_worker_recovery<F>(make: F)
    where
        F: Fn() -> RuntimeRebuildBackend,
    {
        reopen_restores_trigger_registry_state(make()).await;
        worker_runs_trigger_started_lashlang_process_after_restart(make()).await;
        trigger_triggered_process_wake_provenance_survives_restart(make()).await;
        worker_recovers_tool_call_process_in_restarted_session(make()).await;
        worker_recovers_session_turn_process_in_restarted_session(make()).await;
    }

    const TRIGGER_SOURCE: &str = r#"
process remember(tick: clock.Tick) {
  wake { id: tick.id, scheduled_at: tick.scheduled_at }
  finish { id: tick.id, ok: true }
}

source = clock.Alarm({ at: "08:00" })
handle = await triggers.register({
  source: source,
  target: remember,
  inputs: { tick: trigger.event },
  name: "remembered"
})?
finish "registered"
"#;

    const BUTTON_TRIGGER_SOURCE: &str = r#"
process remember_button(event: ui.button.Pressed) {
  wake { button: event.button, message: event.message }
  finish { button: event.button, ok: true }
}

source = ui.button.pressed({})
handle = await triggers.register({
  source: source,
  target: remember_button,
  inputs: { event: trigger.event },
  name: "button remembered"
})?
finish "registered"
"#;

    const SESSION_ID: &str = "rebuild-conformance";

    fn clock_tick_event_type() -> crate::rlm::NamedDataType {
        crate::rlm::NamedDataType::object(
            "clock.Tick",
            vec![
                crate::rlm::TypeField {
                    name: "id".into(),
                    ty: crate::rlm::TypeExpr::Str,
                    optional: false,
                },
                crate::rlm::TypeField {
                    name: "scheduled_at".into(),
                    ty: crate::rlm::TypeExpr::Str,
                    optional: false,
                },
            ],
        )
        .expect("valid clock tick type")
    }

    fn button_pressed_event_type() -> crate::rlm::NamedDataType {
        crate::rlm::NamedDataType::object(
            "ui.button.Pressed",
            vec![
                crate::rlm::TypeField {
                    name: "button".into(),
                    ty: crate::rlm::TypeExpr::Str,
                    optional: false,
                },
                crate::rlm::TypeField {
                    name: "message".into(),
                    ty: crate::rlm::TypeExpr::Str,
                    optional: false,
                },
                crate::rlm::TypeField {
                    name: "pressed_at".into(),
                    ty: crate::rlm::TypeExpr::Str,
                    optional: false,
                },
            ],
        )
        .expect("valid button event type")
    }

    fn rebuild_abilities() -> crate::rlm::LashlangAbilities {
        crate::rlm::LashlangAbilities::default()
            .with_processes()
            .with_sleep()
            .with_process_signals()
            .with_triggers()
    }

    /// Installs the trigger abilities used by the rebuild conformance program.
    struct TriggerResourcePluginFactory;

    impl PluginFactory for TriggerResourcePluginFactory {
        fn id(&self) -> &'static str {
            "rebuild-conformance-trigger"
        }

        fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
            let mut resources = crate::rlm::LashlangHostCatalog::new();
            resources
                .add_trigger_source_constructor(
                    ["clock", "Alarm"],
                    crate::rlm::TypeExpr::Object(vec![crate::rlm::TypeField {
                        name: "at".into(),
                        ty: crate::rlm::TypeExpr::Str,
                        optional: false,
                    }]),
                    clock_tick_event_type(),
                )
                .expect("valid clock trigger source");
            resources
                .add_trigger_source_constructor(
                    ["ui", "button", "pressed"],
                    crate::rlm::TypeExpr::Object(Vec::new()),
                    button_pressed_event_type(),
                )
                .expect("valid button trigger source");
            let contribution = LashlangSurfaceContribution::new(
                rebuild_abilities(),
                LashlangLanguageFeatures::default(),
                resources,
            );
            vec![
                PluginExtensionContribution::new(LASHLANG_SURFACE_EXTENSION_ID, contribution)
                    .expect("lashlang surface contribution serializes"),
            ]
        }

        fn build(
            &self,
            _ctx: &PluginSessionContext,
        ) -> std::result::Result<Arc<dyn SessionPlugin>, PluginError> {
            Ok(Arc::new(TriggerResourceSessionPlugin))
        }
    }

    struct TriggerResourceSessionPlugin;

    impl SessionPlugin for TriggerResourceSessionPlugin {
        fn id(&self) -> &'static str {
            "rebuild-conformance-trigger"
        }

        fn register(&self, reg: &mut PluginRegistrar) -> std::result::Result<(), PluginError> {
            reg.triggers().declare(crate::triggers::TriggerEvent::new(
                "Button",
                "ui.button",
                "pressed",
                lash_core::LashSchema::any(),
            ))?;
            Ok(())
        }
    }

    /// Echo tool for the [`ProcessInput::ToolCall`](lash_core::ProcessInput) scenario.
    struct EchoToolProvider;

    fn echo_tool_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:rebuild_echo",
            "rebuild_echo",
            "Echo the input value.",
            serde_json::json!({ "type": "object", "additionalProperties": true }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
            ["tools"],
            "rebuild_echo",
        ))
    }

    #[async_trait::async_trait]
    impl lash_core::ToolProvider for EchoToolProvider {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            vec![echo_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
            (name == "rebuild_echo").then(|| Arc::new(echo_tool_definition().contract()))
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
            let value = call
                .args
                .get("value")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            lash_core::ToolResult::ok(serde_json::json!({ "echoed": value }))
        }
    }

    fn rebuild_model() -> crate::ModelSpec {
        crate::ModelSpec::builder("rebuild-conformance-model")
            .context_window_tokens(4096)
            .build()
            .expect("model spec")
    }

    fn lashlang_block(source: &str) -> String {
        format!("<lashlang>\n{}\n</lashlang>", source.trim())
    }

    /// Provider used both to register the trigger route through a normal RLM
    /// turn and to finish the SessionTurn child. The child inherits this
    /// provider, exercising provider re-supply after rebuild.
    fn rebuild_provider() -> crate::provider::ProviderHandle {
        TestProvider::builder()
            .kind("rebuild-conformance")
            .complete(|req| async move {
                let rendered_messages = format!("{:?}", req.messages);
                let text = if rendered_messages.contains("run child") {
                    lashlang_block("finish \"child done\"")
                } else if rendered_messages.contains("register rebuild button trigger") {
                    lashlang_block(BUTTON_TRIGGER_SOURCE)
                } else {
                    lashlang_block(TRIGGER_SOURCE)
                };
                Ok(crate::provider::LlmResponse {
                    full_text: text.clone(),
                    parts: vec![crate::direct::LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..crate::provider::LlmResponse::default()
                })
            })
            .build()
            .into_handle()
    }

    fn base_builder(
        registry: Arc<dyn lash_core::ProcessRegistry>,
        artifact_store: Arc<dyn LashlangArtifactStore>,
    ) -> LashCoreBuilder {
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            crate::rlm::RlmProtocolPluginConfig::new(
                crate::rlm::ExecutionBound::instructions(1_000_000),
                crate::rlm::ExecutionBound::secs(30),
            )
            .with_lashlang_abilities(rebuild_abilities()),
            artifact_store,
        );
        LashCore::rlm_builder(crate::TurnBudget::Unbounded, factory)
            .provider(rebuild_provider())
            .model(rebuild_model())
            .plugin(Arc::new(TriggerResourcePluginFactory))
            .tools(Arc::new(EchoToolProvider))
            .process_registry(registry)
    }

    fn worker_registration(
        input: lash_core::ProcessInput,
        id: &str,
    ) -> lash_core::ProcessRegistration {
        lash_core::ProcessRegistration::new(
            id,
            input,
            // Worker-rebuild recovery tests need the row to be re-executable.
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessProvenance::session(lash_core::SessionScope::new(SESSION_ID)),
        )
    }

    async fn attach_rebuild_process_env(
        core: &LashCore,
        registration: lash_core::ProcessRegistration,
    ) -> lash_core::ProcessRegistration {
        let env_ref = lash_core::runtime::persist_process_execution_env(
            core.env.core.durability.process_env_store.as_ref(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::PluginOptions::default(),
                lash_core::SessionPolicy {
                    model: rebuild_model(),
                    ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
            ),
        )
        .await
        .expect("persist rebuild process env");
        registration.with_execution_env_ref(Some(env_ref))
    }

    /// Open the session, register the trigger route through Lashlang, optionally
    /// register an out-of-turn process, then drop and reopen from cold durable
    /// storage.
    async fn open_mutate_and_restart(
        core: &LashCore,
        register: Option<lash_core::ProcessRegistration>,
        registry: &Arc<dyn lash_core::ProcessRegistry>,
    ) {
        open_mutate_and_restart_with_prompt(core, "register rebuild trigger", register, registry)
            .await;
    }

    async fn open_mutate_and_restart_with_prompt(
        core: &LashCore,
        prompt: &str,
        register: Option<lash_core::ProcessRegistration>,
        registry: &Arc<dyn lash_core::ProcessRegistry>,
    ) {
        let session = core.session(SESSION_ID).open().await.expect("open session");
        let output = session
            .turn(lash_core::TurnInput::text(prompt))
            .run()
            .await
            .expect("register trigger route");
        assert_eq!(output.final_value(), Some(&serde_json::json!("registered")));
        if let Some(registration) = register {
            registry
                .register_process(registration)
                .await
                .expect("register out-of-turn process");
        }
        drop(session);
        // Reopen from cold storage — spawns the default work runner and forces the
        // worker to reconstruct the trigger-mutated surface purely from persistence.
        core.session(SESSION_ID)
            .open()
            .await
            .expect("reopen session");
    }

    async fn await_success(registry: &Arc<dyn lash_core::ProcessRegistry>, process_id: &str) {
        let awaiter = lash_core::facade_support::ProcessAwaiter::polling(Arc::clone(registry));
        let outcome =
            tokio::time::timeout(Duration::from_secs(10), awaiter.await_terminal(process_id))
                .await
                .expect("worker runs the process to terminal promptly")
                .expect("await terminal output");
        assert!(
            matches!(outcome, lash_core::ProcessAwaitOutput::Success { .. }),
            "process `{process_id}` must reach terminal SUCCESS via the worker's rebuilt \
             runtime, got: {outcome:?}"
        );
    }

    fn inline_trigger_scope(
        scope_id: impl Into<String>,
    ) -> lash_core::ScopedEffectController<'static> {
        lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
            lash_core::ExecutionScope::runtime_operation(scope_id.into()),
        )
        .expect("inline trigger occurrence execution scope")
    }

    async fn emit_first_clock_alarm(
        core: &LashCore,
        session: &crate::LashSession,
        payload: serde_json::Value,
    ) -> lash_core::facade_support::TriggerEmitReport {
        let registrations = session
            .triggers()
            .by_source_type("clock.Alarm")
            .await
            .expect("list clock trigger registrations");
        let handle = registrations
            .iter()
            .find(|registration| registration.enabled)
            .expect("registered clock trigger");
        let idempotency_key = format!(
            "test-clock:{}",
            payload
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("occurrence")
        );
        core.triggers()
            .emit(
                crate::triggers::TriggerOccurrenceRequest::new(
                    "clock.Alarm",
                    handle.source_key.clone(),
                    payload,
                    idempotency_key.clone(),
                ),
                inline_trigger_scope(format!("trigger:{idempotency_key}")),
            )
            .await
            .expect("emit clock trigger occurrence")
    }

    /// Differential baseline: a live reopen restores the trigger registry route
    /// installed through a normal turn — the same reconstruction the worker
    /// must use for out-of-turn process starts.
    async fn reopen_restores_trigger_registry_state(backend: RuntimeRebuildBackend) {
        let registry = Arc::clone(&backend.process_registry);
        let core = (backend.build_core)(base_builder(
            Arc::clone(&registry),
            Arc::clone(&backend.artifact_store),
        ));
        open_mutate_and_restart(&core, None, &registry).await;

        let reopened = core
            .session(SESSION_ID)
            .open()
            .await
            .expect("reopen session");
        let report = emit_first_clock_alarm(
            &core,
            &reopened,
            serde_json::json!({
                "id": "daily-2026-06-01",
                "scheduled_at": "2026-06-01T08:00:00Z"
            }),
        )
        .await;
        assert_eq!(report.started_process_ids().len(), 1);
    }

    async fn worker_runs_trigger_started_lashlang_process_after_restart(
        backend: RuntimeRebuildBackend,
    ) {
        let registry = Arc::clone(&backend.process_registry);
        let core = (backend.build_core)(base_builder(
            Arc::clone(&registry),
            Arc::clone(&backend.artifact_store),
        ));
        open_mutate_and_restart(&core, None, &registry).await;

        let session = core
            .session(SESSION_ID)
            .open()
            .await
            .expect("reopen session");
        let report = emit_first_clock_alarm(
            &core,
            &session,
            serde_json::json!({
                "id": "daily-2026-06-01",
                "scheduled_at": "2026-06-01T08:00:00Z"
            }),
        )
        .await;
        let started_process_ids = report.started_process_ids();
        assert_eq!(started_process_ids.len(), 1);
        await_success(&registry, &started_process_ids[0]).await;
    }

    async fn trigger_triggered_process_wake_provenance_survives_restart(
        backend: RuntimeRebuildBackend,
    ) {
        let registry = Arc::clone(&backend.process_registry);
        let core = (backend.build_core)(base_builder(
            Arc::clone(&registry),
            Arc::clone(&backend.artifact_store),
        ));
        open_mutate_and_restart_with_prompt(
            &core,
            "register rebuild button trigger",
            None,
            &registry,
        )
        .await;

        let source_key = crate::triggers::empty_trigger_source_key("ui.button.pressed")
            .expect("button source key");
        let idempotency_key = "runtime-rebuild-trigger";
        let report = core
            .triggers()
            .emit(
                crate::triggers::TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    source_key,
                    serde_json::json!({
                    "button": "Red",
                    "message": "user pressed the red button",
                    "pressed_at": "2026-06-02T12:00:00Z"
                    }),
                    idempotency_key,
                )
                .with_source(serde_json::json!({})),
                inline_trigger_scope(format!("trigger:{idempotency_key}")),
            )
            .await
            .expect("emit trigger occurrence");
        let started_process_ids = report.started_process_ids();
        assert_eq!(started_process_ids.len(), 1);
        let process_id = started_process_ids[0].clone();
        let record = registry
            .get_process(&process_id)
            .await
            .expect("read process")
            .expect("trigger-triggered process record");
        let process_caused_by = record
            .provenance
            .caused_by
            .clone()
            .expect("triggered process cause");
        assert!(matches!(
            &process_caused_by,
            lash_core::CausalRef::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                ..
            }
                if occurrence_id == &report.occurrence_id
                    && subscription_id.as_deref()
                        == report.deliveries.first().map(|delivery| delivery.subscription_id.as_str())
        ));

        await_success(&registry, &process_id).await;
        let wake_sequence = registry
            .events_after(&process_id, 0)
            .await
            .expect("trigger-triggered process events")
            .into_iter()
            .find(|event| event.event_type == "process.wake")
            .expect("trigger-triggered process wake event")
            .sequence;
        let session = core
            .session(SESSION_ID)
            .open()
            .await
            .expect("reopen session");
        let describe_process_messages = |read_view: &lash_core::SessionReadView| {
            read_view
                .messages()
                .iter()
                .filter(|message| {
                    matches!(
                        &message.origin,
                        Some(lash_core::MessageOrigin::Process {
                            process_id: message_process_id,
                            ..
                        }) if message_process_id == &process_id
                    )
                })
                .map(|message| format!("{message:?}"))
                .collect::<Vec<_>>()
        };
        let contains_expected_wake = |read_view: &lash_core::SessionReadView| {
            read_view.messages().iter().any(|message| {
                message.role == lash_core::MessageRole::Event
                    && matches!(
                        &message.origin,
                        Some(lash_core::MessageOrigin::Process {
                            process_id: wake_process_id,
                            event_type,
                            sequence,
                            caused_by,
                            ..
                        }) if wake_process_id == &process_id
                            && event_type == "process.wake"
                            && *sequence == wake_sequence
                            && caused_by.as_ref() == Some(&process_caused_by)
                    )
            })
        };
        let observation = session.observe();
        let current = observation.current_observation();
        if !contains_expected_wake(&current.read_view) {
            let mut commits = observation.subscribe_and_recover(current.cursor);
            let mut observed_items = Vec::new();
            // 60s, not 10s: this waits for a full scripted turn through a durable
            // rebuild, and contended CI shards have exceeded 10s (PR #170 run).
            let committed = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match commits.next().await {
                        Some(Ok(crate::observe::SessionObservationStreamItem::Event(event))) => {
                            observed_items.push(format!("{event:?}"));
                            if let lash_core::SessionObservationEventPayload::Committed {
                                read_view,
                            } = &event.payload
                                && contains_expected_wake(read_view)
                            {
                                break;
                            }
                        }
                        Some(Ok(crate::observe::SessionObservationStreamItem::Gap {
                            observation,
                            ..
                        })) => {
                            observed_items.push(format!("gap={observation:?}"));
                            if contains_expected_wake(&observation.read_view) {
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            panic!("wake commit observation failed: {error}")
                        }
                        None => panic!("wake commit observation ended before settlement"),
                    }
                }
            })
            .await;
            if committed.is_err() {
                let deliveries = registry
                    .list_wake_deliveries(None)
                    .await
                    .expect("inspect stalled wake deliveries");
                let queued = session
                    .queued_work()
                    .await
                    .expect("inspect stalled wake queue");
                let latest = observation.current_observation();
                panic!(
                    "wake turn did not commit promptly; deliveries={deliveries:?}; \
                     queued={queued:?}; initial_process_messages={:?}; \
                     latest_process_messages={:?}; observed_items={observed_items:?}",
                    describe_process_messages(&current.read_view),
                    describe_process_messages(&latest.read_view),
                );
            }
        }
        drop(session);

        let session = core
            .session(SESSION_ID)
            .open()
            .await
            .expect("reopen drained session");
        assert!(
            session
                .queued_work()
                .await
                .expect("queued wake drained by background work driver")
                .is_empty()
        );
        let read_view = session.read_view();
        let messages = read_view.messages();
        assert!(
            messages.iter().any(|message| {
                message.role == lash_core::MessageRole::Event
                    && matches!(
                        &message.origin,
                        Some(lash_core::MessageOrigin::Process {
                            process_id: wake_process_id,
                            event_type,
                            sequence,
                            caused_by,
                            ..
                        }) if wake_process_id == &process_id
                            && event_type == "process.wake"
                            && *sequence == wake_sequence
                            && caused_by.as_ref() == Some(&process_caused_by)
                    )
            }),
            "drained process wake should persist as an event message with trigger provenance"
        );
    }

    async fn worker_recovers_tool_call_process_in_restarted_session(
        backend: RuntimeRebuildBackend,
    ) {
        let registry = Arc::clone(&backend.process_registry);
        let core = (backend.build_core)(base_builder(
            Arc::clone(&registry),
            Arc::clone(&backend.artifact_store),
        ));
        let registration = worker_registration(
            lash_core::ProcessInput::ToolCall {
                call: lash_core::PreparedToolCall::from_parts(
                    "rebuild-tool-call",
                    "tool:rebuild_echo",
                    "rebuild_echo",
                    serde_json::json!({ "value": "recovered" }),
                    None,
                    serde_json::Value::Null,
                ),
            },
            "proc-tool-call",
        );
        let registration = attach_rebuild_process_env(&core, registration).await;
        open_mutate_and_restart(&core, Some(registration), &registry).await;
        await_success(&registry, "proc-tool-call").await;
    }

    async fn worker_recovers_session_turn_process_in_restarted_session(
        backend: RuntimeRebuildBackend,
    ) {
        let registry = Arc::clone(&backend.process_registry);
        let core = (backend.build_core)(base_builder(
            Arc::clone(&registry),
            Arc::clone(&backend.artifact_store),
        ));
        let child_policy = lash_core::SessionPolicy {
            model: rebuild_model(),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let registration = worker_registration(
            lash_core::ProcessInput::SessionTurn {
                definition_key: "lash-testing-session-turn:v1".to_string(),
                create_request: Box::new(lash_core::SessionCreateRequest::child(
                    SESSION_ID,
                    lash_core::SessionStartPoint::Empty,
                    child_policy,
                    lash_core::PluginOptions::default(),
                    "rebuild-conformance",
                )),
                turn_input: Box::new(lash_core::TurnInput::text("run child")),
                output_contract: lash_core::ToolOutputContract::Static,
            },
            "proc-session-turn",
        );
        open_mutate_and_restart(&core, Some(registration), &registry).await;
        await_success(&registry, "proc-session-turn").await;
    }
}
