// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

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
    let mut state = RlmExecutionState::for_engine(language);
    let response = execute_code_with_channel_and_bounds_with_trigger_resolver(
        &mut state,
        lash_core::testing::code_execution_context_with_trigger_store_and_invocation(
            crate::testing::memory_backend_ports().await,
            crate::testing::memory_trigger_store().await,
            crate::testing::memory_process_registry().await,
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
        crate::testing::fresh_memory_artifact_store().await,
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
        None,
        Some(resolver),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    (state, response)
}

#[test]
fn deferred_trigger_constructor_and_event_schema_link() {
    block_on(async {
        let cases = [(
            "typescript",
            r#"
                    const remember = async (change: calendar.Change) => true;
                    const source = calendar.Changed({});
                    finish(await triggers.register({
                      source, target: remember, inputs: (event) => ({ change: event })
                    }));
                "#,
        )];
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
            "typescript",
            r#"
                const remember = async (change: calendar.Change) => true;
                const source = calendar.Changed({});
                finish(await triggers.register({
                  source, target: remember, inputs: (event) => ({ change: event })
                }));
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
        let mut restored = RlmExecutionState::for_engine("typescript");
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
        let cases = [(
            "typescript",
            r#"
                    const sourceInput = () => ({});
                    const remember = async (change: calendar.Change) => true;
                    finish(await triggers.register({
                      source: calendar.Changed(sourceInput()), target: remember,
                      inputs: (event) => ({ change: event })
                    }));
                "#,
        )];
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
            const wrong = async (value: number) => true;
            await triggers.register({
              source: calendar.Changed({}), target: wrong,
              inputs: (event) => ({ value: event })
            });
            finish(true);
        "#;
        for (outcome, expected) in [
            (
                lash_lashlang_runtime::TriggerResolution::NotAvailable,
                "unknown module `calendar`",
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
            let (_, response) = execute_with_deferred_trigger("typescript", code, resolver).await;
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
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_channel_and_bounds_with_trigger_resolver(
            &mut state,
            lash_core::testing::code_execution_context_with_trigger_store_and_invocation(
                crate::testing::memory_backend_ports().await,
                crate::testing::memory_trigger_store().await,
                crate::testing::memory_process_registry().await,
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
                language: "typescript".to_string(),
                code: r#"
                    const remember = async (change: calendar.Change) => true;
                    const unused = async () => { await web.fetch({}); return true; };
                    const source = calendar.Changed({});
                    await triggers.register({
                      source, target: remember,
                      inputs: (event) => ({ change: event })
                    });
                    finish(true);
                "#
                .to_string(),
            },
            crate::testing::fresh_memory_artifact_store().await,
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            Some(Arc::new(MixedToolResolver)),
            Some(Arc::new(MixedTriggerResolver)),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
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

/// Captures every effect envelope a runtime sends through a SQLite memory
/// backend's effect host, which journals and runs each one.
#[derive(Clone, Default)]
pub(super) struct TriggerEffectCapture {
    envelopes: Arc<std::sync::Mutex<Vec<lash_core::RuntimeEffectEnvelope>>>,
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for TriggerEffectCapture {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.envelopes.lock_recover().push(envelope.clone());
        inner.execute_effect(envelope, local_executor).await
    }
}

impl TriggerEffectCapture {
    /// A fresh memory backend with this capture layered over its effect
    /// host.
    async fn backend(&self) -> lash_core::Backend {
        let layer: Arc<dyn lash_core::testing::EffectLayer> = Arc::new(self.clone());
        lash_core::testing::runtime_helpers::LayeredBackend::over(
            Arc::new(
                lash_sqlite_store::SqliteBackend::memory()
                    .await
                    .expect("open a memory backend"),
            )
            .into(),
        )
        .map_effect_host(|host| Arc::new(lash_core::testing::LayeredEffectHost::new(host, layer)))
        .into_backend()
    }

    /// [`Self::backend`]'s effect host.
    async fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.backend().await.effect_host()
    }

    /// The registration drafts the runtime sent, in order.
    fn register_drafts(&self) -> Vec<lash_core::TriggerSubscriptionDraft> {
        self.envelopes
            .lock_recover()
            .iter()
            .filter_map(|envelope| {
                let lash_core::RuntimeEffectCommand::Trigger { command } = &envelope.command else {
                    return None;
                };
                match command.as_ref() {
                    lash_core::TriggerCommand::Register { draft, .. } => Some(draft.clone()),
                    _ => None,
                }
            })
            .collect()
    }

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
    capture: TriggerEffectCapture,
) -> ExecResponse {
    let mut state = RlmExecutionState::new();
    let ctx = lash_core::testing::code_execution_context_with_trigger_store(
        crate::testing::ports_over_host(capture.effect_host().await).await,
        crate::testing::memory_trigger_store().await,
        crate::testing::memory_process_registry().await,
    );
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        timer_trigger_resources(),
    );
    execute_code_unbounded_for_tests(
        &mut state,
        ctx,
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        crate::testing::fresh_memory_artifact_store().await,
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
    )
    .await
}

pub(super) async fn execute_with_trigger_environment(code: &str) -> ExecResponse {
    execute_with_host_environment(
        code,
        lashlang::LashlangAbilities::default(),
        timer_trigger_resources(),
    )
    .await
}

#[test]
pub(super) fn typescript_register_trigger_executes_end_to_end() {
    block_on(async {
        let response = execute_with_trigger_environment(
            r#"
                const remember = async (tick: unknown) => { return true; };
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const handle = await triggers.register({
                  source,
                  target: remember,
                  inputs: (event) => ({ tick: event }),
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

/// A registration the target cannot accept is refused before the cell's
/// foreground code continues.
///
/// This is the retired lashlang test
/// `trigger_registration_failure_prevents_foreground_execution`, re-authored
/// over TypeScript: the property needed a target whose declared input type
/// disagrees with the source's event, and until a declared `run` parameter
/// type reached the process signature (FIG-3071) every TypeScript process
/// accepted everything. `timer.Schedule` emits a `timer.Tick` record, which a
/// `string` parameter cannot receive.
#[test]
fn trigger_registration_failure_prevents_foreground_execution() {
    block_on(async {
        let response = execute_with_trigger_environment(
            r#"
                const remember = async (tick: string) => { return true; };
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                await triggers.register({
                  source,
                  target: remember,
                  inputs: (event) => ({ tick: event }),
                  name: "remembered"
                });
                console.log("the foreground must not reach here");
                finish("should not run");
                "#,
        )
        .await;

        let error = response
            .error
            .as_ref()
            .expect("an event type mismatch must refuse the registration");
        assert!(
            error.message.contains("trigger source emits"),
            "{}",
            error.message
        );
        assert!(response.observations.is_empty());
        assert!(response.terminal_finish.is_none());
    });
}

#[test]
pub(super) fn trigger_registry_operations_execute_foreground_code() {
    block_on(async {
        let response = execute_with_trigger_environment(
            r#"
                const remember = async (tick: timer.Tick) => true;
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const handle = await triggers.register({
                  source,
                  target: remember,
                  inputs: (event) => ({ tick: event }),
                  name: "remembered"
                });
                const registrations = await triggers.list({ target: remember });

                finish({ answer: "foreground ran", handle: handle, registrations: registrations });
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
        let store = crate::testing::memory_trigger_store().await;
        let capture = TriggerEffectCapture::default();
        let ctx = lash_core::testing::code_execution_context_with_trigger_store(
            crate::testing::ports_over_host(capture.effect_host().await).await,
            store.clone(),
            crate::testing::memory_process_registry().await,
        );
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let response = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            ctx,
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                        const remember = async (tick: timer.Tick) => tick.fired_at;
                        const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                        const handle = await triggers.register({
                          source,
                          target: remember,
                          inputs: (event) => ({ tick: event })
                        });
                        finish(handle);
                    "#
                .to_string(),
            },
            crate::testing::memory_artifact_store().await,
            surface,
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);

        // Re-pinned by FIG-2999: a keyless key is derived from the source and
        // the target's definition, and the target is now a lifted process
        // whose declaration is named by its lift digest rather than by the
        // `const` the source spells. The derivation is unchanged; its input
        // moved with the dialect. Re-pinned again by FIG-3571: the target's
        // definition names the artifact that carries the linked program
        // verbatim, so its module ref moved; the derivation is unchanged. The
        // FIG-3571 cutover moved it once more with the semantic hash version
        // and the lifted-process name domain.
        let expected_key =
            "derived/v3/9579ddf94026db8f3517f8e16148c3a089d710c7efbbeffb4f74744a5b90f1dd";
        // The fixture's production effect address gives the cell's binding
        // set (FIG-3587) and the deferred-resolution journal their link
        // identity, so the journaled binding set is the first envelope, the
        // resolution production always wrote the second, the register the
        // third.
        let (effect_owner_scope, effect_subscription_key) = {
            let envelopes = capture.envelopes.lock_recover();
            let lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation } =
                &envelopes[0].command
            else {
                panic!("expected the cell's binding set first")
            };
            assert_eq!(
                operation,
                "cell_tool_bindings:v1:[\"timer.Schedule\",\"triggers.register\"]"
            );
            let lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation } =
                &envelopes[1].command
            else {
                panic!("expected the deferred tool resolution effect second")
            };
            assert_eq!(
                operation,
                "deferred_tool_resolution:v2:[\"timer.Schedule\",\"triggers.register\"]"
            );
            let lash_core::RuntimeEffectCommand::Trigger { command } = &envelopes[2].command else {
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
            let capture = TriggerEffectCapture::default();
            let response = Box::pin(execute_with_capturing_trigger_effects(
                code,
                capture.clone(),
            ))
            .await;
            assert!(response.error.is_none(), "{:?}", response.error);
            capture
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
                        draft
                            .target_identity
                            .definition
                            .as_ref()?
                            .definition
                            .as_json()["module_ref"]
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
                const remember = async (tick: timer.Tick) => tick.fired_at;
                const morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" });
                await triggers.register({ source: morning, target: remember, inputs: (event) => ({ tick: event }) });
                await triggers.register({ source: evening, target: remember, inputs: (event) => ({ tick: event }) });
                finish(true);
                "#,
            ))
            .await;
        // Boxed because the awaited future crosses clippy's large-future
        // threshold once the turn config carries its budgets.
        let second = Box::pin(capture(
                r#"
                const remember = async (tick: timer.Tick) => tick.fired_at;
                const morning = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const evening = timer.Schedule({ expr: "0 18 * * *", tz: "UTC" });
                await triggers.register({ source: evening, target: remember, inputs: (event) => ({ tick: event }) });
                await triggers.register({ source: morning, target: remember, inputs: (event) => ({ tick: event }) });
                finish(true);
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
        let trigger_store = crate::testing::memory_trigger_store().await;
        let artifact_store = crate::testing::fresh_memory_artifact_store().await;
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let mut state = RlmExecutionState::new();

        let first = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context_with_trigger_store(
                crate::testing::memory_backend_ports().await,
                trigger_store.clone(),
                crate::testing::memory_process_registry().await,
            ),
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                        const remember = async (tick: timer.Tick) => tick.fired_at;
                        const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                        await triggers.register({
                          source,
                          target: remember,
                          inputs: (event) => ({ tick: event }),
                          subscription_key: "old-schedule"
                        });
                        finish(await triggers.list({}));
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
            lash_core::testing::code_execution_context_with_trigger_store(
                crate::testing::memory_backend_ports().await,
                trigger_store.clone(),
                crate::testing::memory_process_registry().await,
            ),
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                        console.log("unrelated observation");
                        finish(42);
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
        let context = lash_core::testing::code_execution_context_for_process(
            crate::testing::memory_backend_ports().await,
            &registration,
        );
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
                language: "typescript".to_string(),
                code: "finish(42);".to_string(),
            },
            crate::testing::fresh_memory_artifact_store().await,
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
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_memory_artifact_store().await;
    let capture = TriggerEffectCapture::default();
    let backend = capture.backend().await;
    let registry = backend.process_registry();
    let registry_dyn = Arc::clone(&registry);
    let trigger_store = backend.trigger_store();
    let process_env_store = backend.process_env_store();
    let effect_host = backend.effect_host();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        timer_trigger_resources(),
    );
    let engine_surface = process_engine_surface(surface.clone());
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("trigger process test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                engine_surface,
            ),
        ),
    );
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn.clone());
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                lash_core::testing::test_code_protocol_factories(),
            )),
            runtime_host,
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid trigger process worker");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: originator_override.clone(),
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone()),
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        lash_core::testing::TestExecutionPorts::over_host(effect_host, process_env_store),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
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
    let response = execute_code_with_channel_and_bounds(
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
        crate::plugin::RlmChannel::Cell,
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
        trigger_effects: capture.trigger_effects(),
        subscriptions,
    }
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
        let context = lash_core::testing::code_execution_context_for_process(
            crate::testing::memory_backend_ports().await,
            &registration,
        );
        let owner_error = context
            .trigger_owner_scope()
            .expect_err("a bare host process must not have a trigger owner namespace");
        assert!(
            owner_error.to_string().contains("bare host authority"),
            "{owner_error}"
        );

        let result = execute_trigger_process_with_originator(
            "typescript",
            r#"
                const registrar = async () => await triggers.list({});
                const handle = await processes.start({ definition: registrar });
                finish(handle.process_id);
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
pub(super) fn typescript_process_body_uses_trigger_command_handler() {
    block_on(async {
        let result = execute_trigger_process(
            "typescript",
            r#"
                const registrar = async () => await triggers.list({});
                const handle = await processes.start({ definition: registrar });
                finish(handle.process_id);
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
                const registrar = async () => {
                  const listRegistrations = () => triggers.list({});
                  return await listRegistrations();
                };
                const handle = await processes.start({ definition: registrar });
                finish(handle.process_id);
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
        let scalar = TriggerEffectCapture::default();
        let response = Box::pin(execute_with_capturing_trigger_effects(
            r#"
                const remember = async (tick: timer.Tick) => true;
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const registered = await triggers.register({
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  name: "scalar", subscription_key: "scalar"
                });
                const listed = await triggers.list({ target: remember });
                const updated = await triggers.update({
                  subscription_key: "scalar", expected_revision: registered.revision,
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  name: "scalar-updated"
                });
                const disabled = await triggers.disable({
                  subscription_key: "scalar", expected_revision: updated.revision
                });
                const enabled = await triggers.enable({
                  subscription_key: "scalar", expected_revision: disabled.revision
                });
                const deleted = await triggers.delete({
                  subscription_key: "scalar", expected_revision: enabled.revision
                });
                await triggers.register({
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  subscription_key: "prune-me"
                });
                const pruned = await triggers.prune({ subscription_keys: ["prune-me"] });
                finish(listed.length);
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

        let batched = TriggerEffectCapture::default();
        let response = Box::pin(execute_with_capturing_trigger_effects(
            r#"
                const remember = async (tick: timer.Tick) => true;
                const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                const update_seed = await triggers.register({
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-update"
                });
                const registered_enable_seed = await triggers.register({
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-enable"
                });
                const enable_seed = await triggers.disable({
                  subscription_key: "batch-enable", expected_revision: registered_enable_seed.revision
                });
                const disable_seed = await triggers.register({
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-disable"
                });
                const delete_seed = await triggers.register({
                  source, target: remember, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-delete"
                });
                const results = await Promise.all([
                  triggers.register({
                    source, target: remember, inputs: (event) => ({ tick: event }),
                    subscription_key: "batch-register"
                  }),
                  triggers.list({}),
                  triggers.update({
                    subscription_key: "batch-update", expected_revision: update_seed.revision,
                    source, target: remember, inputs: (event) => ({ tick: event }),
                    name: "batch-updated"
                  }),
                  triggers.enable({
                    subscription_key: "batch-enable", expected_revision: enable_seed.revision
                  }),
                  triggers.disable({
                    subscription_key: "batch-disable", expected_revision: disable_seed.revision
                  }),
                  triggers.delete({
                    subscription_key: "batch-delete", expected_revision: delete_seed.revision
                  })
                ]);
                finish(results[1].length);
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
                const remember = async (tick: timer.Tick) => true;
                const source = timer.Schedule({ expr: "0 8 * * *" });
                const handle = await triggers.register({
                  source,
                  target: remember,
                  inputs: (event) => ({ tick: event }),
                  name: "remembered",
                  subscription_key: "remembered"
                });
                const disabled = await triggers.disable({
                  subscription_key: "remembered",
                  expected_revision: handle.revision
                });
                const registrations = await triggers.list({ target: remember });
                finish({ disposition: disabled.disposition, enabled: registrations[0].enabled });
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
pub(super) fn foreground_sleep_executes_through_runtime_context() {
    block_on(async {
        let response = execute_with_abilities(
            r#"
                await sleep(0);
                finish("awake");
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
        let record = format!(
            "{{ output: {}, status: \"failed\", error: \"boom\", exit_code: 2, stderr: \"short\" }}",
            serde_json::to_string(&large).expect("string literal")
        );
        let response = execute_with_abilities(
            &format!("print({record});"),
            lashlang::LashlangAbilities::default(),
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.observations.len(), 1);
        assert!(
            response.observations[0].text.contains(&large),
            "raw observation should preserve full printed value"
        );
        let metadata = &response.observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert_eq!(metadata.original_chars, 61_517);
        // `print` hands the host the record itself, so the projector summarises
        // it field by field instead of cutting the rendering at the byte limit
        // (FIG-3061).
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

/// The byte cap is still the backstop for the rendering route: `console.log`
/// stringifies before the observation is written, so its projection is a cut
/// string, not a summary. Both routes stay inside the same budget.
#[test]
pub(super) fn console_log_of_a_large_record_still_stops_at_the_byte_cap() {
    block_on(async {
        let large = "x".repeat(60 * 1024);
        let code = format!(
            "console.log({{ output: {}, status: \"failed\" }});",
            serde_json::to_string(&large).expect("string literal")
        );
        let response = execute_with_abilities(&code, lashlang::LashlangAbilities::default()).await;

        assert!(response.error.is_none(), "{:?}", response.error);
        let metadata = &response.observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert!(
            metadata.projected_chars < metadata.original_chars,
            "{metadata:?}"
        );
    });
}

/// FIG-2999: `sleep` is the one lashlang feature a host can still withhold.
/// Declaring a process, starting one, signalling one and registering a trigger
/// are no longer abilities — a process is an ordinary value and the controls
/// are leaf tools, so their availability is the catalogue's presence or absence
/// rather than a flag the host sets.
#[test]
pub(super) fn executor_reports_a_disabled_lashlang_ability_at_link_time() {
    block_on(async {
        let code = "await sleep(1000);";
        lash_typescript::parse(code).expect("the fixture parses");
        let response = execute_with_host_environment(
            code,
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangHostCatalog::new(),
        )
        .await;
        let error = response
            .error
            .as_ref()
            .expect("a withheld ability fails at link time");

        assert!(
            error
                .message
                .contains("lashlang feature `sleep` is disabled by this host"),
            "error was {}",
            error.message,
        );
        assert!(response.calls.is_empty(), "no runtime tools are called");
        assert!(
            response.observations.is_empty(),
            "no observations are emitted"
        );
        assert!(response.printed_images.is_empty(), "no images are emitted");
        assert!(
            response.terminal_finish.is_none(),
            "the program does not finish terminally"
        );
    });
}

/// The `inputs` arrow is erased, so it must leave no trace anywhere downstream.
///
/// `inputs: (event) => ({ tick: event, label: "daily" })` replaced
/// `inputs: { tick: trigger.event, label: "daily" }` (FIG-2986, GitHub #1350).
/// The fixture beside this file was captured on the commit before the old
/// spelling was deleted, with the command recorded in that PR's body, so this
/// cannot pass by comparing the new implementation against itself: it pins the
/// canonical artifact bytes, the module and host-requirement hashes, the
/// exports, the process definition identity the trigger targets, the compiled
/// program, and the registration draft the runtime actually sends.
///
/// Artifact-byte identity is what the hashes and the bytecode are derived
/// from, so a drift in any of them is a drift in the first assertion too; the
/// rest are pinned because they are what other systems durably hold.
///
/// The captured bytes were re-pinned once more when TypeScript became the only
/// RLM language (ADR 0096): the artifact no longer carries a
/// `compilation_dialect` field, and every hash derived from those bytes moved
/// with it. Nothing else in the capture changed, so the arrow form still
/// reproduces the retired record form structure for structure.
///
/// They were re-pinned again by FIG-3071: this source annotates both `run`
/// parameters, so their declared types now reach the process signature, and
/// `LASHLANG_SEMANTIC_HASH_VERSION` moved to `v10` to announce that identity
/// computation changed. Both moves are visible here — the parameter types in
/// the canonical IR, and every hash derived from the artifact.
///
/// They were re-pinned once on top of that when the trigger operations stopped
/// being hand-written types and started being declared as JSON Schema like
/// every other host operation (FIG-2993). A schema's properties are an
/// unordered map, so the imported field list for `triggers.register` is in
/// name order rather than the order the old Rust literal happened to write;
/// the fields, their types and their optionality are identical, and the hashes
/// moved because that ordering is inside the hashed host requirements.
///
/// They were re-pinned once more by FIG-2996 part 1, which collapsed the VM's
/// two handle encodings onto one `HandleId` and moved
/// `LASHLANG_SEMANTIC_HASH_VERSION` to `v11` to announce it. Only the derived
/// hashes moved: the canonical IR, the exports structure, the compiled program
/// and the registration draft are byte-identical to the previous capture, which
/// is what a version move is supposed to look like. Regenerated with
/// `cargo nextest run -p lash-internal-protocol-rlm -E
/// 'test(repin_trigger_inputs_retired_record_form)' --run-ignored all`.
///
/// They were re-pinned once more by FIG-3120, which moved
/// `LASHLANG_SEMANTIC_HASH_VERSION` to `v15` after `canonical_program_ir`
/// started alpha-normalizing local binder names so that one module ref
/// addresses one byte string. This re-pin is visible in the capture as well as
/// in the hashes: the `const` binders `remember`, `source` and `handle` are now
/// written as `local#0`, `local#1` and `local#2` in the canonical IR. The
/// process parameter names and the lifted process declaration name are
/// unchanged, because those are ABI names, not locals.
///
/// They were re-pinned again by FIG-3571, which deleted that normalizer: the
/// artifact carries the linked program verbatim as `ir`, so `remember`,
/// `source` and `handle` are spelled as authored again, the lifted process
/// records its origin, and the module ref, the lifted name and the
/// registration identity moved with them. The compiled program is unchanged.
///
/// The FIG-3571 cutover re-pinned them once more: `LASHLANG_SEMANTIC_HASH_VERSION`
/// moved to `v17` and the lifted-process name domain to `v2`, so the lifted
/// name, the module and host-requirement hashes and the registration identity
/// moved. The canonical IR is otherwise byte-identical.
///
/// FIG-3620 moved `LASHLANG_SEMANTIC_HASH_VERSION` to `v18` for the new
/// `__typescript_global_get` builtin: only the module, host-requirement and
/// component hashes moved; the lifted name and the canonical IR did not.
///
/// FIG-3655 re-pinned them once more: `FunctionExpr` gained `js_name`, so the
/// canonical artifact bytes moved even though the JSON view of the IR is
/// unchanged (`js_name` skips serialization when absent). Only the module and
/// component hashes and the derived registration identity moved; the lifted
/// name, the compiled program and the registration payload did not.
///
/// FIG-3701 re-pinned them again: `LASHLANG_SEMANTIC_HASH_VERSION` moved to
/// v21 because a member read of an advertised method now means the built-in
/// function, so every module hash moved with it; the artifact's IR did not.
///
/// FIG-3707 re-pinned them again: `LASHLANG_SEMANTIC_HASH_VERSION` moved to
/// v22 for the binding-cell intrinsics, so the module, host-requirement and
/// component hashes moved; the artifact's IR did not.
///
/// FIG-3728 and FIG-3745 re-pinned them again: `LASHLANG_SEMANTIC_HASH_VERSION`
/// moved to `v23` because the lowerer converts a computed member key once at
/// reference creation, probes a composite dispatch's own member before the
/// arguments, and converts a computed object-literal key before its value.
/// This source spells none of those, so the artifact's IR is unchanged and
/// only the hashes moved.
/// The arrow spelling under test. The capture's own `source` field records the
/// *retired* record form it was taken from, so a re-pin compiles this one.
const TRIGGER_INPUTS_ARROW_SOURCE: &str = r#"
const remember = async (tick: timer.Tick, label: string) => {
  return true;
};
const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
const handle = await triggers.register({
  source,
  target: remember,
  inputs: (event) => ({ tick: event, label: "daily" }),
  name: "remembered",
  subscription_key: "remembered-key"
});
finish(handle);
"#;

#[test]
fn trigger_inputs_arrow_reproduces_the_retired_record_form() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/trigger_inputs_retired_record_form.json"
    ))
    .expect("the captured fixture is valid JSON");

    block_on(async {
        let capture = TriggerEffectCapture::default();
        let store = crate::testing::fresh_memory_artifact_store().await;
        let response = Box::pin(execute_typescript_with_capturing_trigger_effects(
            TRIGGER_INPUTS_ARROW_SOURCE,
            capture.clone(),
            store.clone(),
        ))
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);

        let drafts = capture.register_drafts();
        let [draft] = drafts.as_slice() else {
            panic!("exactly one registration, got {}", drafts.len());
        };
        let identity = draft
            .target_identity
            .definition
            .clone()
            .expect("the target carries a process definition identity")
            .definition
            .into_json();
        let identity: lashlang::ProcessDefinitionIdentity =
            serde_json::from_value(identity).expect("a process definition identity");

        let artifact =
            lashlang::LashlangArtifacts::get_module_artifact(&store, &identity.module_ref)
                .await
                .expect("the store is readable")
                .expect("the registered module was stored");

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &artifact.to_store_bytes().expect("artifact serializes")
            )
            .expect("artifact bytes are JSON"),
            fixture["artifact"],
            "the arrow form must produce byte-identical canonical artifacts"
        );
        assert_eq!(
            serde_json::json!(artifact.module_ref().to_string()),
            fixture["module_ref"]
        );
        assert_eq!(
            serde_json::json!(artifact.host_requirements_ref().to_string()),
            fixture["host_requirements_ref"]
        );
        assert_eq!(
            serde_json::to_value(artifact.exports()).expect("exports serialize"),
            fixture["exports"]
        );
        assert_eq!(
            serde_json::to_value(&identity).expect("identity serializes"),
            fixture["process_definition_identity"]
        );

        let compiled = lashlang::testing::harness::try_compile_program(artifact.ir())
            .expect("the canonical IR compiles");
        assert_eq!(
            serde_json::json!(format!("{compiled:?}")),
            fixture["compiled_program_debug"]
        );

        assert_eq!(
            serde_json::to_value(draft).expect("the draft serializes"),
            fixture["registration_draft"],
            "the registration payload the runtime sends must be unchanged"
        );
    });
}

/// Re-pins the capture above after a deliberate identity move.
///
/// Ignored, so it never runs as part of a suite; the re-pinning lane runs it
/// by name and reviews the diff. It re-measures only the derived fields — the
/// `source` is the authored cell and is never rewritten — so a structural
/// drift still shows up as a diff a reviewer reads, not as a silent pass.
#[test]
#[ignore = "re-pins the trigger-inputs capture; run by name after a deliberate identity move"]
fn repin_trigger_inputs_retired_record_form() {
    let path = std::env::var("BUILD_WORKSPACE_DIRECTORY")
        .map(|root| {
            std::path::PathBuf::from(root)
                .join("crates/lash-protocol-rlm/src/executor/tests/fixtures")
        })
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/executor/tests/fixtures")
        })
        .join("trigger_inputs_retired_record_form.json");
    let mut fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the capture is readable"))
            .expect("the captured fixture is valid JSON");

    block_on(async {
        let capture = TriggerEffectCapture::default();
        let store = crate::testing::fresh_memory_artifact_store().await;
        let response = Box::pin(execute_typescript_with_capturing_trigger_effects(
            TRIGGER_INPUTS_ARROW_SOURCE,
            capture.clone(),
            store.clone(),
        ))
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);

        let drafts = capture.register_drafts();
        let [draft] = drafts.as_slice() else {
            panic!("exactly one registration, got {}", drafts.len());
        };
        let identity = draft
            .target_identity
            .definition
            .clone()
            .expect("the target carries a process definition identity")
            .definition
            .into_json();
        let identity: lashlang::ProcessDefinitionIdentity =
            serde_json::from_value(identity).expect("a process definition identity");
        let artifact =
            lashlang::LashlangArtifacts::get_module_artifact(&store, &identity.module_ref)
                .await
                .expect("the store is readable")
                .expect("the registered module was stored");
        let compiled = lashlang::testing::harness::try_compile_program(artifact.ir())
            .expect("the canonical IR compiles");

        fixture["artifact"] =
            serde_json::from_slice(&artifact.to_store_bytes().expect("artifact serializes"))
                .expect("artifact bytes are JSON");
        fixture["module_ref"] = serde_json::json!(artifact.module_ref().to_string());
        fixture["host_requirements_ref"] =
            serde_json::json!(artifact.host_requirements_ref().to_string());
        fixture["exports"] = serde_json::to_value(artifact.exports()).expect("exports serialize");
        fixture["process_definition_identity"] =
            serde_json::to_value(&identity).expect("identity serializes");
        fixture["compiled_program_debug"] = serde_json::json!(format!("{compiled:?}"));
        fixture["registration_draft"] = serde_json::to_value(draft).expect("the draft serializes");

        let mut text = serde_json::to_string_pretty(&fixture).expect("the capture serializes");
        text.push('\n');
        std::fs::write(&path, text).expect("the capture is writable");
    });
}

async fn execute_typescript_with_capturing_trigger_effects(
    code: &str,
    capture: TriggerEffectCapture,
    store: lashlang::LashlangArtifacts,
) -> ExecResponse {
    let mut state = RlmExecutionState::for_engine("typescript");
    execute_code_with_channel_and_bounds(
        &mut state,
        lash_core::testing::code_execution_context_with_trigger_store(
            crate::testing::ports_over_host(capture.effect_host().await).await,
            crate::testing::memory_trigger_store().await,
            crate::testing::memory_process_registry().await,
        ),
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        store,
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        ),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await
}
