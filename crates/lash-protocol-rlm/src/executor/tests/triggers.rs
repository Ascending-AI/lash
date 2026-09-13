use super::*;

#[derive(Clone)]
struct TestDeferredTriggerResolver {
    outcome: lash_lashlang_runtime::TriggerResolution,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredTriggerResolver for TestDeferredTriggerResolver {
    async fn resolve(
        &self,
        paths: &[&str],
    ) -> BTreeMap<String, lash_lashlang_runtime::TriggerResolution> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        paths
            .iter()
            .map(|path| ((*path).to_string(), self.outcome.clone()))
            .collect()
    }
}

struct MixedTriggerResolver;

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredTriggerResolver for MixedTriggerResolver {
    async fn resolve(
        &self,
        paths: &[&str],
    ) -> BTreeMap<String, lash_lashlang_runtime::TriggerResolution> {
        paths
            .iter()
            .map(|path| {
                let outcome = if *path == "calendar.Changed" {
                    lash_lashlang_runtime::TriggerResolution::Resolved(Box::new(
                        calendar_trigger_grant("calendar-primary"),
                    ))
                } else {
                    lash_lashlang_runtime::TriggerResolution::NotAvailable
                };
                ((*path).to_string(), outcome)
            })
            .collect()
    }
}

struct MixedToolResolver;

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for MixedToolResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        paths
            .iter()
            .map(|path| {
                let outcome = if *path == "web.fetch" {
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition()),
                    ))
                } else {
                    lash_lashlang_runtime::Resolution::NotAvailable
                };
                ((*path).to_string(), outcome)
            })
            .collect()
    }
}

fn calendar_trigger_grant(route: &str) -> lash_lashlang_runtime::TriggerGrant {
    lash_lashlang_runtime::TriggerGrant::new(
        ["calendar", "Changed"],
        lashlang::TypeExpr::Object(vec![]),
        lashlang::NamedDataType::object(
            "calendar.Change",
            vec![lashlang::TypeField {
                name: "id".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            }],
        )
        .expect("valid calendar event type"),
    )
    .with_provider_id("calendar-provider")
    .with_route(serde_json::json!({"route": route}))
}

async fn execute_with_deferred_trigger(
    language: &str,
    code: &str,
    resolver: lash_lashlang_runtime::SharedDeferredTriggerResolver,
) -> (RlmExecutionState, ExecResponse) {
    let dialect = if language == "typescript" {
        SourceDialect::Typescript
    } else {
        SourceDialect::Lashlang
    };
    let mut state = RlmExecutionState::for_engine(language);
    let response = execute_code_with_dialect_and_bounds_with_trigger_resolver(
        &mut state,
        lash_core::testing::code_execution_context_with_trigger_store_and_invocation(
            Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
            lash_core::testing::exec_code_invocation(
                "session",
                "turn",
                0,
                0,
                "exec-code",
                format!("exec-code:deferred-trigger:{language}"),
            ),
        ),
        ExecRequest {
            language: language.to_string(),
            code: code.to_string(),
        },
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
        LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_triggers(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
        None,
        Some(resolver),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(dialect),
    )
    .await;
    (state, response)
}

#[test]
fn deferred_trigger_constructor_and_event_schema_link_for_both_frontends() {
    block_on(async {
        let cases = [
            (
                "lashlang",
                r#"
                    process remember(change: calendar.Change) { finish true }
                    source = calendar.Changed({})
                    handle = await triggers.register({
                      source: source,
                      target: remember,
                      inputs: { change: trigger.event }
                    })?
                    finish handle
                "#,
            ),
            (
                "typescript",
                r#"
                    const remember = defineProcess({
                      name: "remember", signals: {},
                      run: async (change: calendar.Change) => true
                    });
                    const source = calendar.Changed({});
                    finish(await registerTrigger({
                      source, target: remember, inputs: { change: trigger.event }
                    }));
                "#,
            ),
        ];
        for (language, code) in cases {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let resolver: lash_lashlang_runtime::SharedDeferredTriggerResolver =
                Arc::new(TestDeferredTriggerResolver {
                    outcome: lash_lashlang_runtime::TriggerResolution::Resolved(Box::new(
                        calendar_trigger_grant("calendar-primary"),
                    )),
                    calls: Arc::clone(&calls),
                });
            let (state, response) = execute_with_deferred_trigger(language, code, resolver).await;
            assert!(response.error.is_none(), "{language}: {:?}", response.error);
            assert_eq!(
                response.terminal_finish.as_ref().unwrap()["type"],
                "trigger_handle"
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(state.deferred_resolutions.resolutions.is_empty());
            assert!(matches!(
                state.deferred_trigger_resolutions.resolutions["calendar.Changed"],
                lash_lashlang_runtime::TriggerResolution::Resolved(ref grant)
                    if grant.route == serde_json::json!({"route": "calendar-primary"})
            ));
        }
    });
}

#[test]
fn deferred_trigger_record_and_provider_route_survive_snapshot_restore() {
    block_on(async {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredTriggerResolver =
            Arc::new(TestDeferredTriggerResolver {
                outcome: lash_lashlang_runtime::TriggerResolution::Resolved(Box::new(
                    calendar_trigger_grant("snapshot-route"),
                )),
                calls,
            });
        let (mut state, response) = execute_with_deferred_trigger(
            "lashlang",
            r#"
                process remember(change: calendar.Change) { finish true }
                source = calendar.Changed({})
                finish await triggers.register({
                  source: source, target: remember,
                  inputs: { change: trigger.event }
                })?
            "#,
            resolver,
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);

        let hydration = hydrate_snapshot(
            state
                .snapshot_execution_state()
                .expect("trigger-bearing state snapshots"),
        );
        let mut restored = RlmExecutionState::for_engine("lashlang");
        restored
            .restore_execution_state(&hydration)
            .expect("trigger-bearing state restores");

        assert!(matches!(
            restored.deferred_trigger_resolutions.resolutions["calendar.Changed"],
            lash_lashlang_runtime::TriggerResolution::Resolved(ref grant)
                if grant.provider_id == "calendar-provider"
                    && grant.route == serde_json::json!({"route": "snapshot-route"})
        ));
        assert!(restored.deferred_resolutions.resolutions.is_empty());
    });
}

#[test]
fn deferred_trigger_references_inside_helpers_and_processes_are_gathered() {
    block_on(async {
        let cases = [
            (
                "lashlang",
                r#"
                    process remember(change: calendar.Change) { finish true }
                    process install() {
                      source = calendar.Changed({})
                      await triggers.register({
                        source: source, target: remember,
                        inputs: { change: trigger.event }
                      })?
                      finish true
                    }
                    finish true
                "#,
            ),
            (
                "typescript",
                r#"
                    const sourceInput = () => ({});
                    const remember = defineProcess({
                      name: "remember", signals: {},
                      run: async (change: calendar.Change) => true
                    });
                    finish(await registerTrigger({
                      source: calendar.Changed(sourceInput()), target: remember,
                      inputs: { change: trigger.event }
                    }));
                "#,
            ),
        ];
        for (language, code) in cases {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let resolver: lash_lashlang_runtime::SharedDeferredTriggerResolver =
                Arc::new(TestDeferredTriggerResolver {
                    outcome: lash_lashlang_runtime::TriggerResolution::Resolved(Box::new(
                        calendar_trigger_grant("helper-route"),
                    )),
                    calls: Arc::clone(&calls),
                });
            let (_, response) = execute_with_deferred_trigger(language, code, resolver).await;
            assert!(response.error.is_none(), "{language}: {:?}", response.error);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    });
}

#[test]
fn deferred_trigger_zero_and_ambiguous_results_fail_before_target_mapping() {
    block_on(async {
        let code = r#"
            process wrong(value: int) { finish true }
            await triggers.register({
              source: calendar.Changed({}), target: wrong,
              inputs: { value: trigger.event }
            })?
            finish true
        "#;
        for (outcome, expected) in [
            (
                lash_lashlang_runtime::TriggerResolution::NotAvailable,
                "unknown name `calendar`",
            ),
            (
                lash_lashlang_runtime::TriggerResolution::Ambiguous {
                    provider_ids: vec!["a".to_string(), "b".to_string()],
                },
                "ambiguous across providers",
            ),
        ] {
            let resolver: lash_lashlang_runtime::SharedDeferredTriggerResolver =
                Arc::new(TestDeferredTriggerResolver {
                    outcome,
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                });
            let (_, response) = execute_with_deferred_trigger("lashlang", code, resolver).await;
            let error = response
                .error
                .expect("link must reject unavailable definition");
            assert!(error.message.contains(expected), "{}", error.message);
            assert!(
                !error.message.contains("trigger event"),
                "{}",
                error.message
            );
        }
    });
}

#[test]
fn mixed_deferred_trigger_and_tool_links_keep_provider_records_separate() {
    block_on(async {
        let mut state = RlmExecutionState::for_engine("lashlang");
        let response = execute_code_with_dialect_and_bounds_with_trigger_resolver(
            &mut state,
            lash_core::testing::code_execution_context_with_trigger_store_and_invocation(
                Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
                lash_core::testing::exec_code_invocation(
                    "session",
                    "turn",
                    0,
                    0,
                    "exec-code",
                    "exec-code:mixed-deferred-definitions",
                ),
            ),
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"
                    process remember(change: calendar.Change) { finish true }
                    process unused() {
                      source = calendar.Changed({})
                      await web.fetch({})?
                      await triggers.register({
                        source: source, target: remember,
                        inputs: { change: trigger.event }
                      })?
                      finish true
                    }
                    finish true
                "#
                .to_string(),
            },
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default()
                    .with_processes()
                    .with_triggers(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            Some(Arc::new(MixedToolResolver)),
            Some(Arc::new(MixedTriggerResolver)),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            RlmSourceContext::cell(SourceDialect::Lashlang),
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(matches!(
            state.deferred_trigger_resolutions.resolutions["calendar.Changed"],
            lash_lashlang_runtime::TriggerResolution::Resolved(_)
        ));
        assert!(matches!(
            state.deferred_trigger_resolutions.resolutions["web.fetch"],
            lash_lashlang_runtime::TriggerResolution::NotAvailable
        ));
        assert!(matches!(
            state.deferred_resolutions.resolutions["web.fetch"],
            lash_lashlang_runtime::Resolution::Resolved(_)
        ));
        assert!(
            !state
                .deferred_resolutions
                .resolutions
                .contains_key("calendar.Changed")
        );
    });
}

pub(super) fn timer_trigger_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_trigger_source_constructor(
            ["timer", "Schedule"],
            lashlang::TypeExpr::Object(vec![
                lashlang::TypeField {
                    name: "expr".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                },
                lashlang::TypeField {
                    name: "tz".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: true,
                },
            ]),
            lashlang::NamedDataType::object(
                "timer.Tick",
                vec![lashlang::TypeField {
                    name: "fired_at".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid timer tick type"),
        )
        .expect("valid timer trigger source");
    resources
}

pub(super) fn disabled_timer_trigger_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = timer_trigger_resources();
    lashlang::add_trigger_resource_operations(&mut resources)
        .expect("trigger resource operations are unique");
    resources
}

#[derive(Clone, Default)]
pub(super) struct CapturingTriggerEffectController {
    envelopes: Arc<std::sync::Mutex<Vec<lash_core::RuntimeEffectEnvelope>>>,
}

impl lash_core::AwaitEventResolver for CapturingTriggerEffectController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for CapturingTriggerEffectController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.envelopes.lock_recover().push(envelope.clone());
        match envelope.command {
            lash_core::RuntimeEffectCommand::Trigger { command } => {
                let operation_id = envelope.invocation.effect_id().to_string();
                let result = lash_core::RuntimeEffectLocalExecutor::into_trigger(local_executor)?
                    .execute(&operation_id, *command)
                    .await?;
                let result = Box::new(result);
                Ok(lash_core::RuntimeEffectOutcome::Trigger { result })
            }
            _ => local_executor.execute(envelope).await,
        }
    }
}

impl CapturingTriggerEffectController {
    fn trigger_effects(&self) -> Vec<(String, &'static str)> {
        self.envelopes
            .lock_recover()
            .iter()
            .filter_map(|envelope| {
                let lash_core::RuntimeEffectCommand::Trigger { command } = &envelope.command else {
                    return None;
                };
                let operation = match command.as_ref() {
                    lash_core::TriggerCommand::Register { .. } => "register",
                    lash_core::TriggerCommand::List { .. } => "list",
                    lash_core::TriggerCommand::Update { .. } => "update",
                    lash_core::TriggerCommand::Enable { .. } => "enable",
                    lash_core::TriggerCommand::Disable { .. } => "disable",
                    lash_core::TriggerCommand::Delete { .. } => "delete",
                    lash_core::TriggerCommand::Revive { .. } => "revive",
                    lash_core::TriggerCommand::Prune { .. } => "prune",
                };
                Some((envelope.invocation.effect_id().to_string(), operation))
            })
            .collect()
    }
}

pub(super) async fn execute_with_capturing_trigger_effects(
    code: &str,
    controller: CapturingTriggerEffectController,
) -> ExecResponse {
    let mut state = RlmExecutionState::new();
    let ctx = lash_core::testing::code_execution_context_with_trigger_store_and_effect_controller(
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
        Arc::new(controller),
    );
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_triggers(),
        lashlang::LashlangLanguageFeatures::default(),
        timer_trigger_resources(),
    );
    execute_code_unbounded_for_tests(
        &mut state,
        ctx,
        ExecRequest {
            language: "lashlang".to_string(),
            code: code.to_string(),
        },
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
    )
    .await
}

pub(super) async fn execute_with_trigger_environment(code: &str) -> ExecResponse {
    execute_with_lashlang_host_environment(
        code,
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_triggers(),
        timer_trigger_resources(),
    )
    .await
}

pub(super) async fn execute_typescript_with_trigger_environment(code: &str) -> ExecResponse {
    let mut state = RlmExecutionState::for_engine("typescript");
    execute_code_with_dialect_and_bounds(
        &mut state,
        lash_core::testing::code_execution_context_with_trigger_store(Arc::new(
            lash_core::facade_support::InMemoryTriggerStore::default(),
        )),
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
        LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_triggers(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        ),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(SourceDialect::Typescript),
    )
    .await
}

#[test]
pub(super) fn typescript_register_trigger_executes_end_to_end() {
    block_on(async {
        let response = execute_typescript_with_trigger_environment(
            r#"
                const remember = defineProcess({
                  name: "remember", signals: {},
                  run: async (tick: unknown) => { return true; }
                });
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const handle = await registerTrigger({
                  source,
                  target: remember,
                  inputs: { tick: trigger.event },
                  name: "remembered"
                });
                finish(handle);
                "#,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        let handle = response.terminal_finish.expect("terminal finish");
        assert_eq!(handle["type"], serde_json::json!("trigger_handle"));
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            vec![("triggers.register", lash_core::ExecutedCallOutcome::Ok)]
        );
    });
}

#[test]
pub(super) fn trigger_registry_operations_execute_foreground_code() {
    block_on(async {
        let response = execute_with_trigger_environment(
            r#"
                process remember(tick: timer.Tick) {
                  finish true
                }

                source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                handle = await triggers.register({
                  source: source,
                  target: remember,
                  inputs: { tick: trigger.event },
                  name: "remembered"
                })?
                registrations = await triggers.list({ target: remember })?

                finish { answer: "foreground ran", handle: handle, registrations: registrations }
                "#,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(response.observations.is_empty());
        let finish = response.terminal_finish.expect("terminal finish");
        assert_eq!(finish["answer"], serde_json::json!("foreground ran"));
        assert_eq!(
            finish["handle"]["type"],
            serde_json::json!("trigger_handle")
        );
        assert_eq!(
            finish["registrations"][0]["name"],
            serde_json::json!("remembered")
        );
        assert_eq!(
            finish["registrations"][0]["source"]["$lash_host_descriptor_type"],
            serde_json::json!("timer.Schedule")
        );
        assert_eq!(
            finish["registrations"][0]["source"]["$lash_host_descriptor_value"]["expr"],
            serde_json::json!("0 8 * * *")
        );
        assert_eq!(
            finish["registrations"][0]["revision"],
            finish["handle"]["revision"]
        );
        assert_eq!(
            finish["registrations"][0]["incarnation"],
            finish["handle"]["incarnation"]
        );
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            vec![
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.list", lash_core::ExecutedCallOutcome::Ok),
            ],
            "trigger effects must appear in the executed-call ledger"
        );
    });
}

#[test]
pub(super) fn keyless_trigger_registration_reaches_effect_and_owner_scoped_store() {
    block_on(async {
        let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
        let controller = CapturingTriggerEffectController::default();
        let ctx =
            lash_core::testing::code_execution_context_with_trigger_store_and_effect_controller(
                store.clone(),
                Arc::new(controller.clone()),
            );
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_triggers(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let response = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            ctx,
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"
                        process remember(tick: timer.Tick) { finish tick.fired_at }
                        source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                        handle = await triggers.register({
                          source: source,
                          target: remember,
                          inputs: { tick: trigger.event }
                        })?
                        finish handle
                    "#
                .to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);

        let expected_key =
            "derived/v3/9956413528fb2c204e9f9941784a6e9d51ef7926b69d3e31e0f20cc493309a4b";
        let (effect_owner_scope, effect_subscription_key) = {
            let envelopes = controller.envelopes.lock_recover();
            let lash_core::RuntimeEffectCommand::Trigger { command } = &envelopes[0].command else {
                panic!("expected trigger effect")
            };
            let lash_core::TriggerCommand::Register {
                owner_scope, draft, ..
            } = command.as_ref()
            else {
                panic!("expected register command")
            };
            (owner_scope.clone(), draft.subscription_key.clone())
        };
        assert_eq!(
            effect_owner_scope,
            lash_core::TriggerOwnerScope::session("test-session")
        );
        assert_eq!(effect_subscription_key, expected_key);

        let stored = lash_core::TriggerStore::list_subscriptions(
            store.as_ref(),
            lash_core::TriggerSubscriptionFilter::for_session("test-session"),
        )
        .await
        .expect("list stored keyless registration");
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].owner_scope,
            lash_core::TriggerOwnerScope::session("test-session")
        );
        assert_eq!(stored[0].subscription_key, expected_key);
    });
}

#[test]
pub(super) fn reordered_keyless_registration_calls_keep_derived_keys_across_module_regeneration() {
    block_on(async {
        async fn capture(code: &str) -> Vec<(String, String, String)> {
            let controller = CapturingTriggerEffectController::default();
            let response = Box::pin(execute_with_capturing_trigger_effects(
                code,
                controller.clone(),
            ))
            .await;
            assert!(response.error.is_none(), "{:?}", response.error);
            controller
                .envelopes
                .lock_recover()
                .iter()
                .filter_map(|envelope| {
                    let lash_core::RuntimeEffectCommand::Trigger { command } = &envelope.command
                    else {
                        return None;
                    };
                    let lash_core::TriggerCommand::Register { draft, .. } = command.as_ref() else {
                        return None;
                    };
                    Some((
                        draft.source_key.clone(),
                        draft.subscription_key.clone(),
                        draft.target_identity.definition.as_ref()?["module_ref"]
                            .as_str()?
                            .to_string(),
                    ))
                })
                .collect()
        }

        // Boxed because the awaited future crosses clippy's large-future
        // threshold once the turn config carries its budgets.
        let first = Box::pin(capture(
                r#"
                process remember(tick: timer.Tick) { finish tick.fired_at }
                morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" })
                await triggers.register({ source: morning, target: remember, inputs: { tick: trigger.event } })?
                await triggers.register({ source: evening, target: remember, inputs: { tick: trigger.event } })?
                finish true
                "#,
            ))
            .await;
        // Boxed because the awaited future crosses clippy's large-future
        // threshold once the turn config carries its budgets.
        let second = Box::pin(capture(
                r#"
                process remember(tick: timer.Tick) { finish tick.fired_at }
                morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" })
                await triggers.register({ source: evening, target: remember, inputs: { tick: trigger.event } })?
                await triggers.register({ source: morning, target: remember, inputs: { tick: trigger.event } })?
                finish true
                "#,
            ))
            .await;

        let first_keys = first
            .iter()
            .map(|(source, key, _)| (source, key))
            .collect::<BTreeMap<_, _>>();
        let second_keys = second
            .iter()
            .map(|(source, key, _)| (source, key))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(first_keys, second_keys);
        assert_ne!(first[0].2, second[0].2, "the artifacts were regenerated");
    });
}

#[test]
pub(super) fn removing_a_declaration_and_running_unrelated_code_does_not_unregister() {
    block_on(async {
        let trigger_store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
        let artifact_store = Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_triggers(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let mut state = RlmExecutionState::new();

        let first = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context_with_trigger_store(trigger_store.clone()),
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"
                        process remember(tick: timer.Tick) { finish tick.fired_at }
                        source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                        await triggers.register({
                          source: source,
                          target: remember,
                          inputs: { tick: trigger.event },
                          subscription_key: "old-schedule"
                        })?
                        finish await triggers.list({})?
                    "#
                .to_string(),
            },
            artifact_store.clone(),
            surface.clone(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_none(), "{:?}", first.error);
        let listed = first.terminal_finish.expect("registration list");
        let listed = listed.as_array().expect("list result");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["subscription_key"], "old-schedule");
        assert!(listed[0].get("manifest_membership").is_none());
        assert!(listed[0]["registrant"].is_object());

        let before = lash_core::TriggerStore::list_subscriptions(
            trigger_store.as_ref(),
            lash_core::TriggerSubscriptionFilter::for_session("test-session"),
        )
        .await
        .expect("list registration before unrelated execution");

        let unrelated = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context_with_trigger_store(trigger_store.clone()),
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"
                        print "unrelated observation"
                        finish 42
                    "#
                .to_string(),
            },
            artifact_store,
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;

        assert!(unrelated.error.is_none(), "{:?}", unrelated.error);
        assert_eq!(unrelated.terminal_finish, Some(serde_json::json!(42)));
        assert_eq!(unrelated.observations.len(), 1);
        assert!(
            unrelated
                .observations
                .iter()
                .any(|observation| { observation.text.contains("unrelated observation") })
        );
        for observation in &unrelated.observations {
            assert_eq!(
                observation.projection.original_chars,
                observation.text.chars().count(),
                "projection metadata must belong to its observation"
            );
            assert_eq!(
                observation.projection.original_lines,
                observation.text.lines().count(),
                "projection metadata must belong to its observation"
            );
        }

        let after = lash_core::TriggerStore::list_subscriptions(
            trigger_store.as_ref(),
            lash_core::TriggerSubscriptionFilter::for_session("test-session"),
        )
        .await
        .expect("list registration after unrelated execution");
        assert_eq!(
            after, before,
            "unrelated execution must not mutate registration"
        );
    });
}

#[test]
pub(super) fn triggerless_execution_requires_no_trigger_namespace() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let registration = lash_core::ProcessRegistration::new(
            "unscoped-host-process",
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        );
        let context = lash_core::testing::code_execution_context_for_process(&registration);
        let owner_error = context
            .trigger_owner_scope()
            .expect_err("a bare host process must not have a trigger owner namespace");
        assert!(
            owner_error.to_string().contains("bare host authority"),
            "{owner_error}"
        );
        let response = execute_code_unbounded_for_tests(
            &mut state,
            context,
            ExecRequest {
                language: "lashlang".to_string(),
                code: "finish 42".to_string(),
            },
            Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));
    });
}

struct TriggerProcessResult {
    terminal: lash_core::ProcessAwaitOutput,
    trigger_effects: Vec<(String, &'static str)>,
    subscriptions: Vec<lash_core::TriggerSubscriptionRecord>,
}

async fn execute_trigger_process(language: &str, code: &str) -> TriggerProcessResult {
    execute_trigger_process_with_originator(
        language,
        code,
        None,
        Some(lash_core::TriggerOwnerScope::session("test-session")),
        true,
    )
    .await
}

async fn execute_trigger_process_with_originator(
    language: &str,
    code: &str,
    originator_override: Option<lash_core::ProcessOriginator>,
    expected_owner_scope: Option<lash_core::TriggerOwnerScope>,
    expect_success: bool,
) -> TriggerProcessResult {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let controller = CapturingTriggerEffectController::default();
    let controller_dyn: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(controller.clone());
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_triggers(),
        lashlang::LashlangLanguageFeatures::default(),
        timer_trigger_resources(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("trigger process test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(
            lash_core::facade_support::NativeEffectHost::new(controller_dyn.clone())
                .allow_process_lifetime_completion_keys(),
        ),
        Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
        process_env_store.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                surface.clone(),
            ),
        ),
    );
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn.clone());
    let worker = lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                lash_core::testing::test_code_protocol_factories(),
            )),
            runtime_host,
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_trigger_store(trigger_store.clone())
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid trigger process worker");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        controller: controller_dyn.clone(),
        originator_override: originator_override.clone(),
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        Arc::new(EmptyTypeScriptSignalToolProvider),
        lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
        None,
        processes,
        controller_dyn,
        process_env_store,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = if language == "typescript" {
        RlmExecutionState::for_engine("typescript")
    } else {
        RlmExecutionState::new()
    };
    let response = execute_code_with_dialect_and_bounds(
        &mut state,
        ctx,
        ExecRequest {
            language: language.to_string(),
            code: code.to_string(),
        },
        artifact_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(if language == "typescript" {
            SourceDialect::Typescript
        } else {
            SourceDialect::Lashlang
        }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(
        response.terminal_finish.is_some(),
        "process start must finish"
    );

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive trigger process");
    let records = registry
        .list_observed_by(
            &SessionId::from("test-session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list trigger process");
    let [record] = records.as_slice() else {
        panic!("expected exactly one trigger process, got {records:?}");
    };
    assert_eq!(
        record.provenance.originator,
        originator_override.unwrap_or_else(|| {
            lash_core::ProcessOriginator::session(lash_core::SessionScope::new("test-session"))
        })
    );
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        lash_core::NativeProcessWork::for_registry(registry_dyn).await_terminal(&record.id),
    )
    .await
    .expect("trigger process reaches terminal state")
    .expect("await trigger process");
    assert_eq!(
        matches!(
            terminal,
            lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ),
        expect_success,
        "unexpected process trigger terminal: {terminal:?}"
    );
    let subscriptions = if let Some(owner_scope) = expected_owner_scope {
        trigger_store
            .list_subscriptions(lash_core::TriggerSubscriptionFilter::for_registrant_scope(
                owner_scope.namespace(),
            ))
            .await
            .expect("list process-created trigger subscriptions")
    } else {
        Vec::new()
    };
    TriggerProcessResult {
        terminal,
        trigger_effects: controller.trigger_effects(),
        subscriptions,
    }
}

#[test]
pub(super) fn named_host_process_trigger_uses_host_owner_scope() {
    block_on(async {
        let owner_scope =
            lash_core::TriggerOwnerScope::host("automation-a").expect("valid host owner");
        let result = execute_trigger_process_with_originator(
            "lashlang",
            r#"
                process remember(tick: timer.Tick) { finish true }
                process registrar() {
                  source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                  receipt = await triggers.register({
                    source: source, target: remember, inputs: { tick: trigger.event },
                    subscription_key: "named-host-process"
                  })?
                  finish receipt.owner_scope
                }
                handle = start registrar()
                finish handle.id
            "#,
            Some(lash_core::ProcessOriginator::host_scoped("automation-a")),
            Some(owner_scope.clone()),
            true,
        )
        .await;

        assert_eq!(result.subscriptions.len(), 1);
        assert_eq!(result.subscriptions[0].owner_scope, owner_scope);
        assert_eq!(
            result.subscriptions[0].registrant,
            lash_core::ProcessOriginator::host_scoped("automation-a")
        );
        assert!(result.subscriptions[0].wake_target.is_some());
        assert_eq!(
            result
                .trigger_effects
                .iter()
                .map(|(_, operation)| *operation)
                .collect::<Vec<_>>(),
            ["register"]
        );
    });
}

#[test]
pub(super) fn bare_host_process_trigger_is_refused_before_store_mutation() {
    block_on(async {
        let registration = lash_core::ProcessRegistration::new(
            "bare-host-trigger-process",
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        );
        let context = lash_core::testing::code_execution_context_for_process(&registration);
        let owner_error = context
            .trigger_owner_scope()
            .expect_err("a bare host process must not have a trigger owner namespace");
        assert!(
            owner_error.to_string().contains("bare host authority"),
            "{owner_error}"
        );

        let result = execute_trigger_process_with_originator(
            "lashlang",
            r#"
                process registrar() { finish await triggers.list({})? }
                handle = start registrar()
                finish handle.id
            "#,
            Some(lash_core::ProcessOriginator::host()),
            None,
            false,
        )
        .await;

        let terminal = serde_json::to_string(&result.terminal).expect("serialize terminal");
        assert!(terminal.contains("bare host authority"), "{terminal}");
        assert!(
            result.trigger_effects.is_empty(),
            "authority refusal must happen before trigger effect execution"
        );
        assert!(result.subscriptions.is_empty());
    });
}

#[test]
pub(super) fn lashlang_process_trigger_batch_uses_command_handler_in_source_order() {
    block_on(async {
        let result = execute_trigger_process(
            "lashlang",
            r#"
                process remember(tick: timer.Tick) { finish true }
                process registrar() {
                  source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                  seed = await triggers.register({
                    source: source, target: remember, inputs: { tick: trigger.event },
                    subscription_key: "process-update"
                  })?
                  results = await {
                    registered: triggers.register({
                      source: source, target: remember, inputs: { tick: trigger.event },
                      subscription_key: "process-register"
                    })?,
                    listed: triggers.list({ target: remember })?,
                    updated: triggers.update({
                      subscription_key: "process-update", expected_revision: seed.revision,
                      source: source, target: remember, inputs: { tick: trigger.event },
                      name: "updated-by-process"
                    })?
                  }
                  finish { registered: results.registered, count: len(results.listed) }
                }
                handle = start registrar()
                finish handle.id
            "#,
        )
        .await;

        let expected = lash_core::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::success(serde_json::json!({
                "registered": result.subscriptions.iter().find(|record| {
                    record.subscription_key == "process-register"
                }).map(|record| serde_json::json!({
                    "type": "trigger_handle",
                    "id": record.subscription_key,
                    "owner_scope": record.owner_scope,
                    "subscription_key": record.subscription_key,
                    "subscription_id": record.subscription_id,
                    "incarnation": record.incarnation,
                    "revision": record.revision,
                    "definition_fingerprint": record.definition_fingerprint,
                    "enabled": record.enabled,
                    "disposition": "created",
                    "record_snapshot": record,
                })).expect("registered subscription"),
                "count": 2,
            })),
        );
        assert_eq!(result.terminal, expected);
        assert!(result.subscriptions.iter().all(|record| {
            record.owner_scope == lash_core::TriggerOwnerScope::session("test-session")
                && record.registrant
                    == lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                        "test-session",
                    ))
                && record.wake_target.is_some()
        }));
        assert_eq!(
            result
                .trigger_effects
                .iter()
                .map(|(_, operation)| *operation)
                .collect::<Vec<_>>(),
            ["register", "register", "list", "update"]
        );
        assert!(
            result.trigger_effects[1..]
                .iter()
                .all(|(effect_id, _)| effect_id.contains(":child:")),
            "batched process effects must retain child positions: {:?}",
            result.trigger_effects
        );
    });
}

#[test]
pub(super) fn typescript_process_body_uses_trigger_command_handler() {
    block_on(async {
        let result = execute_trigger_process(
            "typescript",
            r#"
                const registrar = defineProcess({
                  name: "registrar", signals: {},
                  run: async () => await triggers.list({})
                });
                const handle = start(registrar);
                finish(handle.id);
            "#,
        )
        .await;

        assert!(
            matches!(
                result.terminal,
                lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
            ),
            "{:?}",
            result.terminal
        );
        assert_eq!(
            result
                .trigger_effects
                .iter()
                .map(|(_, operation)| *operation)
                .collect::<Vec<_>>(),
            ["list"]
        );
        assert!(result.trigger_effects[0].0.starts_with("lashlang:"));
        assert!(result.subscriptions.is_empty());
    });
}

#[test]
pub(super) fn typescript_process_local_helper_reaches_trigger_command_handler() {
    block_on(async {
        let result = execute_trigger_process(
            "typescript",
            r#"
                const registrar = defineProcess({
                  name: "registrar", signals: {},
                  run: async () => {
                    const listRegistrations = () => triggers.list({});
                    return await listRegistrations();
                  }
                });
                const handle = start(registrar);
                finish(handle.id);
            "#,
        )
        .await;

        assert_eq!(
            result.terminal,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!([])
            ))
        );
        assert_eq!(
            result
                .trigger_effects
                .iter()
                .map(|(_, operation)| *operation)
                .collect::<Vec<_>>(),
            ["list"]
        );
        assert!(result.trigger_effects[0].0.starts_with("lashlang:"));
        assert!(result.subscriptions.is_empty());
    });
}

#[test]
pub(super) fn scalar_and_batched_trigger_verbs_emit_typed_effect_envelopes() {
    block_on(async {
        let scalar = CapturingTriggerEffectController::default();
        let response = Box::pin(execute_with_capturing_trigger_effects(
            r#"
                process remember(tick: timer.Tick) { finish true }
                source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                registered = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  name: "scalar", subscription_key: "scalar"
                })?
                listed = await triggers.list({ target: remember })?
                updated = await triggers.update({
                  subscription_key: "scalar", expected_revision: registered.revision,
                  source: source, target: remember, inputs: { tick: trigger.event },
                  name: "scalar-updated"
                })?
                disabled = await triggers.disable({
                  subscription_key: "scalar", expected_revision: updated.revision
                })?
                enabled = await triggers.enable({
                  subscription_key: "scalar", expected_revision: disabled.revision
                })?
                deleted = await triggers.delete({
                  subscription_key: "scalar", expected_revision: enabled.revision
                })?
                await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "prune-me"
                })?
                pruned = await triggers.prune({ subscription_keys: ["prune-me"] })?
                finish len(listed)
                "#,
            scalar.clone(),
        ))
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            [
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.list", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.update", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.disable", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.enable", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.delete", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.prune", lash_core::ExecutedCallOutcome::Ok),
            ],
            "ledger order is source dispatch order"
        );
        let scalar_effects = scalar.trigger_effects();
        assert_eq!(
            scalar_effects
                .iter()
                .map(|(_, operation)| *operation)
                .collect::<Vec<_>>(),
            [
                "register", "list", "update", "disable", "enable", "delete", "register", "prune"
            ]
        );
        assert!(
            scalar_effects
                .iter()
                .all(|(effect_id, _)| !effect_id.contains(":child:"))
        );

        let batched = CapturingTriggerEffectController::default();
        let response = Box::pin(execute_with_capturing_trigger_effects(
            r#"
                process remember(tick: timer.Tick) { finish true }
                source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" })
                update_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-update"
                })?
                enable_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-enable"
                })?
                enable_seed = await triggers.disable({
                  subscription_key: "batch-enable", expected_revision: enable_seed.revision
                })?
                disable_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-disable"
                })?
                delete_seed = await triggers.register({
                  source: source, target: remember, inputs: { tick: trigger.event },
                  subscription_key: "batch-delete"
                })?
                results = await {
                  registered: triggers.register({
                    source: source, target: remember, inputs: { tick: trigger.event },
                    subscription_key: "batch-register"
                  })?,
                  listed: triggers.list({})?,
                  updated: triggers.update({
                    subscription_key: "batch-update", expected_revision: update_seed.revision,
                    source: source, target: remember, inputs: { tick: trigger.event },
                    name: "batch-updated"
                  })?,
                  enabled: triggers.enable({
                    subscription_key: "batch-enable", expected_revision: enable_seed.revision
                  })?,
                  disabled: triggers.disable({
                    subscription_key: "batch-disable", expected_revision: disable_seed.revision
                  })?,
                  deleted: triggers.delete({
                    subscription_key: "batch-delete", expected_revision: delete_seed.revision
                  })?
                }
                finish len(results.listed)
                "#,
            batched.clone(),
        ))
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            [
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.disable", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.list", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.update", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.enable", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.disable", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.delete", lash_core::ExecutedCallOutcome::Ok),
            ],
            "ledger order is source dispatch order"
        );
        let batch_effects = batched
            .trigger_effects()
            .into_iter()
            .filter(|(effect_id, _)| effect_id.contains(":child:"))
            .map(|(_, operation)| operation)
            .collect::<Vec<_>>();
        assert_eq!(
            batch_effects,
            ["register", "list", "update", "enable", "disable", "delete"]
        );
    });
}

#[test]
pub(super) fn trigger_disable_is_revision_checked_and_keeps_registry_entry() {
    block_on(async {
        let response = execute_with_trigger_environment(
            r#"
                process remember(tick: timer.Tick) {
                  finish true
                }

                source = timer.Schedule({ expr: "0 8 * * *" })
                handle = await triggers.register({
                  source: source,
                  target: remember,
                  inputs: { tick: trigger.event },
                  name: "remembered",
                  subscription_key: "remembered"
                })?
                disabled = await triggers.disable({
                  subscription_key: "remembered",
                  expected_revision: handle.revision
                })?
                registrations = await triggers.list({ target: remember })?
                finish { disposition: disabled.disposition, enabled: registrations[0].enabled }
                "#,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!({ "disposition": "disabled", "enabled": false }))
        );
    });
}

#[test]
pub(super) fn trigger_registration_failure_prevents_foreground_execution() {
    block_on(async {
        let response = execute_with_trigger_environment(
            r#"
                process remember(tick: str) {
                  finish tick
                }

                source = timer.Schedule({ expr: "0 8 * * *" })
                await triggers.register({
                  source: source,
                  target: remember,
                  inputs: { tick: trigger.event }
                })?

                finish "should not run"
                "#,
        )
        .await;

        let error = response.error.as_ref().expect("event mismatch should fail");
        assert!(
            error.message.contains("trigger source emits"),
            "{}",
            error.message,
        );
        assert!(response.observations.is_empty());
        assert!(response.terminal_finish.is_none());
    });
}

#[test]
pub(super) fn foreground_sleep_executes_through_runtime_context() {
    block_on(async {
        let response = execute_with_lashlang_abilities(
            r#"
                sleep for "0ms"
                finish "awake"
                "#,
            lashlang::LashlangAbilities::default().with_sleep(),
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.terminal_finish, Some(serde_json::json!("awake")));
    });
}

#[test]
pub(super) fn print_observation_preserves_raw_output_and_records_projection_metadata() {
    block_on(async {
        let large = "x".repeat(60 * 1024);
        let code = format!(
            "print {{ output: {}, status: \"failed\", error: \"boom\", exit_code: 2, stderr: \"short\" }}",
            serde_json::to_string(&large).expect("string literal")
        );
        let response =
            execute_with_lashlang_abilities(&code, lashlang::LashlangAbilities::default()).await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.observations.len(), 1);
        assert!(
            response.observations[0].text.contains(&large),
            "raw observation should preserve full printed value"
        );
        let metadata = &response.observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert_eq!(metadata.original_chars, 61_517);
        assert_eq!(metadata.projected_chars, 330);
        assert_ne!(metadata.original_chars, metadata.projected_chars);
        assert_eq!(metadata.original_lines, 1);
        assert_eq!(metadata.projected_lines, 1);
        assert_eq!(
            metadata.limit,
            crate::rlm_support::PRINT_HISTORY_PROJECTION_CONFIG.max_bytes
        );
        assert_eq!(
            metadata.max_lines,
            crate::rlm_support::PRINT_HISTORY_PROJECTION_CONFIG.max_lines
        );
    });
}

#[test]
pub(super) fn executor_reports_rlm_bare_tool_call_diagnostic_at_link_time() {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["files"],
            "Files",
            "read",
            "read_file",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");

    block_on(async {
        let response = execute_with_lashlang_host_environment(
            r#"finish read_file({ path: "Cargo.toml" })"#,
            lashlang::LashlangAbilities::default(),
            resources,
        )
        .await;
        let error = response
            .error
            .as_ref()
            .expect("bare tool call should fail at link time");

        assert_eq!(
            error.kind,
            lash_core::CellFailureKind::Policy,
            "a link refusal is a policy failure, not a runtime one: {}",
            error.message,
        );
        assert!(
            error.message.starts_with(RLM_BARE_TOOL_CALL_DIAGNOSTIC),
            "{}",
            error.message,
        );
        assert!(
            error.message.contains("hint: use `files.read`"),
            "{}",
            error.message,
        );
        assert!(response.calls.is_empty());
        assert!(response.terminal_finish.is_none());
    });
}

#[test]
pub(super) fn top_level_typo_on_line_40_fails_before_any_effect() {
    block_on(async {
        let mut lines = (1..40)
            .map(|index| format!("print {index}"))
            .collect::<Vec<_>>();
        lines.push("finish misspelled_result".to_string());
        let response = execute_with_lashlang_abilities(
            &lines.join("\n"),
            lashlang::LashlangAbilities::default(),
        )
        .await;

        let error = response.error.expect("link should reject typo");
        assert!(
            error.message.contains("unknown name `misspelled_result`"),
            "{}",
            error.message,
        );
        assert!(
            error.message.contains("--> line 40, column 8"),
            "{}",
            error.message,
        );
        assert!(
            response.observations.is_empty(),
            "no print effect may execute before a link failure"
        );
        assert!(response.calls.is_empty());
        assert!(response.terminal_finish.is_none());
    });
}

#[test]
pub(super) fn executor_reports_disabled_lashlang_abilities_at_link_time() {
    struct DisabledCase {
        name: &'static str,
        code: &'static str,
        abilities: lashlang::LashlangAbilities,
        resources: fn() -> lashlang::LashlangHostCatalog,
        feature: &'static str,
    }

    let cases = [
        DisabledCase {
            name: "process declaration",
            code: "process worker() { finish null }",
            abilities: lashlang::LashlangAbilities::default(),
            resources: lashlang::LashlangHostCatalog::new,
            feature: "processes",
        },
        DisabledCase {
            name: "process start",
            code: "start worker()",
            abilities: lashlang::LashlangAbilities::default(),
            resources: lashlang::LashlangHostCatalog::new,
            feature: "processes",
        },
        DisabledCase {
            name: "sleep",
            code: r#"sleep for "1s""#,
            abilities: lashlang::LashlangAbilities::default(),
            resources: lashlang::LashlangHostCatalog::new,
            feature: "sleep",
        },
        DisabledCase {
            name: "wait_signal",
            code: "process worker() signals { ready: any } { payload = wait_signal(\"ready\") }",
            abilities: lashlang::LashlangAbilities::default().with_processes(),
            resources: lashlang::LashlangHostCatalog::new,
            feature: "process signals",
        },
        DisabledCase {
            name: "signal_run",
            code: "process worker(target: any) { signal_run(target, \"ready\", null) }",
            abilities: lashlang::LashlangAbilities::default().with_processes(),
            resources: lashlang::LashlangHostCatalog::new,
            feature: "process signals",
        },
        DisabledCase {
            name: "trigger",
            code: r#"
                    process worker(tick: timer.Tick) { finish true }
                    source = timer.Schedule({ expr: "0 8 * * *" })
                    await triggers.register({
                      source: source,
                      target: worker,
                      inputs: { tick: trigger.event }
                    })?
                "#,
            abilities: lashlang::LashlangAbilities::default().with_processes(),
            resources: disabled_timer_trigger_resources,
            feature: "triggers",
        },
    ];

    block_on(async {
        for case in cases {
            lashlang::parse(case.code)
                .unwrap_or_else(|err| panic!("{} should parse: {err}", case.name));
            let response = execute_with_lashlang_host_environment(
                case.code,
                case.abilities,
                (case.resources)(),
            )
            .await;
            let error = response
                .error
                .as_ref()
                .unwrap_or_else(|| panic!("{} should fail at link time", case.name));

            assert!(
                error.message.contains(&format!(
                    "lashlang feature `{}` is disabled by this host",
                    case.feature
                )),
                "{} error was {}",
                case.name,
                error.message,
            );
            assert!(
                response.calls.is_empty(),
                "{} should not call runtime tools",
                case.name
            );
            assert!(
                response.observations.is_empty(),
                "{} should not emit observations",
                case.name
            );
            assert!(
                response.printed_images.is_empty(),
                "{} should not emit images",
                case.name
            );
            assert!(
                response.terminal_finish.is_none(),
                "{} should not finish terminally",
                case.name
            );
        }
    });
}
