// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;

const SEED: u64 = 0x5_2c0b;

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
                        lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition())
                            .with_source_id(lash_core::facade_support::PLUGIN_TOOL_SOURCE_ID),
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

/// A cell context whose tool surface carries `triggers.register`. Registration
/// is a declaring leaf tool (FIG-3116), so a cell reaches the trigger store
/// only through the tool's realized intent: the tool resolves the target
/// against `artifact_store`, and realization publishes the cell's execution
/// env through the router's env store, the one the referrer ports hold.
pub(super) async fn trigger_tool_context<'run>(
    ports: impl Into<lash_core::testing::TestExecutionPorts<'run>>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    artifact_store: &lashlang::LashlangArtifacts,
    invocation: Option<lash_core::RuntimeInvocation>,
) -> lash_core::RuntimeExecutionContext<'run> {
    let ports: lash_core::testing::TestExecutionPorts<'run> = ports.into();
    // Realization publishes into the store the referrer ports acquire from.
    let router = lash_core::testing::test_trigger_router(
        trigger_store,
        crate::testing::sqlite_memory_process_registry().await,
    )
    .with_process_artifacts(
        Arc::clone(&ports.process_env_store),
        lash_core::ProcessEngineRegistry::new()
            .with_artifact_ports(
                ports
                    .artifact_ports
                    .clone()
                    .expect("trigger fixture artifact ports"),
            )
            .with_registration(lash_lashlang_runtime::lashlang_process_engine_registration(
                lash_lashlang_runtime::LashlangProcessEngine::new(
                    artifact_store.clone(),
                    LashlangSurface::new(
                        lashlang::LashlangAbilities::default(),
                        lashlang::LashlangLanguageFeatures::default(),
                        timer_trigger_resources(),
                    ),
                    crate::testing::sqlite_recording_backend()
                        .await
                        .worker_recovery(),
                ),
            )),
    );
    let builder = lash_core::testing::TestExecutionContextBuilder::new(ports)
        .provider(Arc::new(
            lash_lashlang_runtime::register_trigger_tool_provider(
                lash_vm_client::service::Service::default(),
                artifact_store.clone(),
            ),
        ))
        .tool_catalog(lash_core::ToolCatalog::from_tool_definitions(vec![
            lash_lashlang_runtime::register_trigger_tool_definition(),
        ]))
        .trigger_router(Some(router));
    match invocation {
        Some(invocation) => builder.runtime_parent_invocation(invocation),
        None => builder,
    }
    .build()
    .into_runtime()
}

async fn execute_with_deferred_trigger(
    language: &str,
    code: &str,
    resolver: lash_lashlang_runtime::SharedDeferredTriggerResolver,
) -> (RlmExecutionState, ExecResponse) {
    let mut state = RlmExecutionState::for_engine(language);
    let double =
        crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("session"),
            lash_core::TurnId::from("turn"),
        ))
        .await
        .expect("open the cell's handler");
    let artifact_store = crate::testing::fresh_sqlite_memory_artifact_store().await;
    let response = execute_code_with_trigger_test_render(
        &mut state,
        trigger_tool_context(
            crate::testing::double_ports(&double, &handler),
            crate::testing::sqlite_memory_trigger_store().await,
            &artifact_store,
            Some(lash_core::testing::exec_code_invocation(
                "session",
                "turn",
                0,
                0,
                "exec-code",
                format!("exec-code:deferred-trigger:{language}"),
            )),
        )
        .await,
        ExecRequest {
            code: code.to_string(),
        },
        artifact_store,
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
        None,
        Some(resolver),
        RlmProjectedBindings::default(),
        None,
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
        crate::render::CodeRendererSlot::default(),
    )
    .await;
    handler.close().await.expect("close the cell's handler");
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
                      source, target: { definition: remember }, inputs: (event) => ({ change: event })
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
            let (state, response) =
                Box::pin(execute_with_deferred_trigger(language, code, resolver)).await;
            assert!(response.error.is_none(), "{language}: {:?}", response.error);
            assert_eq!(
                response.terminal_finish.as_ref().unwrap()["type"],
                "trigger_handle"
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(
                state
                    .deferred_link
                    .as_ref()
                    .expect("active link")
                    .outcomes
                    .is_empty()
            );
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
        let (mut state, response) = Box::pin(execute_with_deferred_trigger(
            "typescript",
            r#"
                const remember = async (change: calendar.Change) => true;
                const source = calendar.Changed({});
                finish(await triggers.register({
                  source, target: { definition: remember }, inputs: (event) => ({ change: event })
                }));
            "#,
            resolver,
        ))
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);

        let hydration = hydrate_snapshot(
            state
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .await
                .expect("trigger-bearing state snapshots"),
        );
        let mut restored = RlmExecutionState::for_engine("typescript");
        restored
            .restore_execution_state(&hydration, lash_core::FleetFormat::current())
            .await
            .expect("trigger-bearing state restores");

        assert!(matches!(
            restored.deferred_trigger_resolutions.resolutions["calendar.Changed"],
            lash_lashlang_runtime::TriggerResolution::Resolved(ref grant)
                if grant.provider_id == "calendar-provider"
                    && grant.route == serde_json::json!({"route": "snapshot-route"})
        ));
        assert!(restored.deferred_link.is_none());
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
                      source: calendar.Changed(sourceInput()), target: { definition: remember },
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
            let (_, response) =
                Box::pin(execute_with_deferred_trigger(language, code, resolver)).await;
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
              source: calendar.Changed({}), target: { definition: wrong },
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
            let (_, response) =
                Box::pin(execute_with_deferred_trigger("typescript", code, resolver)).await;
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("session"),
                lash_core::TurnId::from("turn"),
            ))
            .await
            .expect("open the cell's handler");
        let artifact_store = crate::testing::fresh_sqlite_memory_artifact_store().await;
        let response = execute_code_with_trigger_test_render(
            &mut state,
            trigger_tool_context(
                crate::testing::double_ports(&double, &handler),
                crate::testing::sqlite_memory_trigger_store().await,
                &artifact_store,
                Some(lash_core::testing::exec_code_invocation(
                    "session",
                    "turn",
                    0,
                    0,
                    "exec-code",
                    "exec-code:mixed-deferred-definitions",
                )),
            )
            .await,
            ExecRequest {
                code: r#"
                    const remember = async (change: calendar.Change) => true;
                    const unused = async () => { await web.fetch({}); return true; };
                    const source = calendar.Changed({});
                    await triggers.register({
                      source, target: { definition: remember },
                      inputs: (event) => ({ change: event })
                    });
                    finish(true);
                "#
                .to_string(),
            },
            artifact_store,
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            Some(Arc::new(MixedToolResolver)),
            Some(Arc::new(MixedTriggerResolver)),
            RlmProjectedBindings::default(),
            None,
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
            crate::render::CodeRendererSlot::default(),
        )
        .await;
        handler.close().await.expect("close the cell's handler");

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
            state.deferred_link.as_ref().expect("active link").outcomes["web.fetch"],
            lash_lashlang_runtime::Resolution::Resolved(_)
        ));
        assert!(
            !state
                .deferred_link
                .as_ref()
                .expect("active link")
                .outcomes
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
    let double =
        crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(crate::testing::default_cell_scope())
        .await
        .expect("open the cell's handler");
    let artifact_store = crate::testing::fresh_sqlite_memory_artifact_store().await;
    let ctx = trigger_tool_context(
        crate::testing::double_ports_over_layer(&double, &handler, Arc::new(capture.clone())),
        crate::testing::sqlite_memory_trigger_store().await,
        &artifact_store,
        None,
    )
    .await;
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        timer_trigger_resources(),
    );
    let response = execute_code_unbounded_with_test_render(
        &mut state,
        ctx,
        ExecRequest {
            code: code.to_string(),
        },
        artifact_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        None,
    )
    .await;
    handler.close().await.expect("close the cell's handler");
    response
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
                  target: { definition: remember },
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
                  target: { definition: remember },
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
                  target: { definition: remember },
                  inputs: (event) => ({ tick: event }),
                  name: "remembered"
                });
                const registrations = await triggers.list({ target: { definition: remember } });

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
                        draft.target_identity.definition_id.as_ref()?.to_string(),
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
                await triggers.register({ source: morning, target: { definition: remember }, inputs: (event) => ({ tick: event }) });
                await triggers.register({ source: evening, target: { definition: remember }, inputs: (event) => ({ tick: event }) });
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
                await triggers.register({ source: evening, target: { definition: remember }, inputs: (event) => ({ tick: event }) });
                await triggers.register({ source: morning, target: { definition: remember }, inputs: (event) => ({ tick: event }) });
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
        let trigger_store = crate::testing::sqlite_memory_trigger_store().await;
        let artifact_store = crate::testing::fresh_sqlite_memory_artifact_store().await;
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let mut state = RlmExecutionState::new();

        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let first = execute_code_unbounded_with_test_render(
            &mut state,
            trigger_tool_context(
                crate::testing::double_ports(&double, &handler),
                trigger_store.clone(),
                &artifact_store,
                None,
            )
            .await,
            ExecRequest {
                code: r#"
                        const remember = async (tick: timer.Tick) => tick.fired_at;
                        const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                        await triggers.register({
                          source,
                          target: { definition: remember },
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
            None,
        )
        .await;
        handler.close().await.expect("close the cell's handler");
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

        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let unrelated = execute_code_unbounded_with_test_render(
            &mut state,
            trigger_tool_context(
                crate::testing::double_ports(&double, &handler),
                trigger_store.clone(),
                &artifact_store,
                None,
            )
            .await,
            ExecRequest {
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
            None,
        )
        .await;
        handler.close().await.expect("close the cell's handler");

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
            assert_eq!(observation.projection.limit_chars, 8_000);
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
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        );
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let context = lash_core::testing::code_execution_context_for_process(
            crate::testing::double_ports(&double, &handler),
            lash_core::ProcessId::fixture("host-process"),
            &registration,
        );
        let owner_error = context
            .trigger_owner_scope()
            .expect_err("a bare host process must not have a trigger owner namespace");
        assert!(
            owner_error.to_string().contains("bare host authority"),
            "{owner_error}"
        );
        let response = execute_code_unbounded_with_test_render(
            &mut state,
            context,
            ExecRequest {
                code: "finish(42);".to_string(),
            },
            crate::testing::fresh_sqlite_memory_artifact_store().await,
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            None,
        )
        .await;
        handler.close().await.expect("close the cell's handler");

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));
    });
}

/// The trigger store a process body's trigger effects execute against,
/// recording each command it runs: the effect's id (the operation id the
/// trigger executor passes) and the command's verb. The engine journals the
/// effect in the process segment's own invocation, so the store the worker's
/// runtime executes it on is where the effect is observable.
struct RecordingTriggerStore {
    inner: Arc<dyn lash_core::TriggerStore>,
    commands: Arc<std::sync::Mutex<Vec<(String, &'static str)>>>,
}

fn trigger_verb(command: &lash_core::TriggerCommand) -> &'static str {
    match command {
        lash_core::TriggerCommand::Register { .. } => "register",
        lash_core::TriggerCommand::List { .. } => "list",
        lash_core::TriggerCommand::Update { .. } => "update",
        lash_core::TriggerCommand::Enable { .. } => "enable",
        lash_core::TriggerCommand::Disable { .. } => "disable",
        lash_core::TriggerCommand::Delete { .. } => "delete",
        lash_core::TriggerCommand::Revive { .. } => "revive",
        lash_core::TriggerCommand::Prune { .. } => "prune",
    }
}

#[async_trait::async_trait]
impl lash_core::TriggerStore for RecordingTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core::TriggerCommand,
    ) -> Result<lash_core::TriggerEffectResult, lash_core::PluginError> {
        self.commands
            .lock_recover()
            .push((operation_id.to_string(), trigger_verb(&command)));
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: lash_core::TriggerSubscriptionFilter,
    ) -> Result<Vec<lash_core::TriggerSubscriptionRecord>, lash_core::PluginError> {
        self.inner.list_subscriptions(filter).await
    }

    async fn subscriptions_changed_since(
        &self,
        cursor: lash_core::TriggerSubscriptionChangeCursor,
        limit: usize,
    ) -> std::result::Result<
        (
            Vec<lash_core::TriggerSubscriptionChange>,
            lash_core::TriggerSubscriptionChangeCursor,
        ),
        lash_core::PluginError,
    > {
        self.inner.subscriptions_changed_since(cursor, limit).await
    }
    async fn list_subscriptions_with_cursor(
        &self,
    ) -> std::result::Result<
        (
            Vec<lash_core::TriggerSubscriptionRecord>,
            lash_core::TriggerSubscriptionChangeCursor,
        ),
        lash_core::PluginError,
    > {
        self.inner.list_subscriptions_with_cursor().await
    }
    async fn compact_subscription_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner
            .compact_subscription_tombstones(cutoff_epoch_ms)
            .await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, lash_core::PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> Result<lash_core::TriggerIngressReceipt, lash_core::PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: lash_core::TriggerOccurrenceFilter,
    ) -> Result<Vec<lash_core::TriggerOccurrenceRecord>, lash_core::PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.inner.list_deliveries().await
    }

    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &lash_core::ProcessId,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .bind_delivery_process(occurrence_id, subscription_id, process_id)
            .await
    }

    async fn list_delivery_process_ids(
        &self,
    ) -> Result<Vec<lash_core::ProcessId>, lash_core::PluginError> {
        self.inner.list_delivery_process_ids().await
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryRetentionCandidate>, lash_core::PluginError> {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> Result<Vec<SessionId>, lash_core::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<lash_core::TriggerRetentionReconciliationReport, lash_core::PluginError> {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, lash_core::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core::TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    async fn forget_trigger_tombstones(
        &self,
        written_before_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::StoreError> {
        self.inner
            .forget_trigger_tombstones(written_before_epoch_ms)
            .await
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, lash_core::PluginError> {
        self.inner
            .prune_non_fired_occurrences(cutoff_epoch_ms)
            .await
    }
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
        crate::testing::fresh_sqlite_memory_artifact_store().await;
    let table = crate::testing::DoubleProcesses::new(0x7219_0001).await;
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let effect_host = table.backend().effect_host();
    let registry = table.registry();
    let trigger_store = table.backend().trigger_store();
    let process_env_store = table.env_store();
    let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recording: Arc<dyn lash_core::TriggerStore> = Arc::new(RecordingTriggerStore {
        inner: Arc::clone(&trigger_store),
        commands: Arc::clone(&commands),
    });
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        timer_trigger_resources(),
    );
    let engine_surface = process_engine_surface(surface.clone());
    let session_policy = lash_core::SessionPolicy {
        model: Some(lash_core::LlmProfileConfig::new(
            lash_core::RecordedLlmProfile::mint(
                lash_core::LlmProfileKey::from("mock-model"),
                lash_core::LlmProfileMetadata::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("trigger process test model"),
            ),
        )),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        lash_core::testing::runtime_helpers::LayeredBackend::over(table.backend().clone())
            .map_trigger_store(move |_| recording)
            .into_backend(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                engine_surface,
                table.backend().worker_recovery(),
            ),
        ),
    );
    table.install_worker(
        lash_core::testing::test_code_protocol_factories(),
        runtime_host,
    );
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        hand_over_awaits: None,
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: originator_override.clone(),
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone(), table.backend()),
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(table.double(), &handler),
        process_definition_tool_provider(
            Arc::new(ProcessControlToolProvider),
            surface.clone(),
            lash_vm_client::service::Service::default()
                .with_recovery_store(table.backend().worker_recovery()),
        ),
        process_definition_tool_catalog(),
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            session_policy,
        ),
    );
    // A host-origin process starts its child under the same host authority.
    // The recorded start reads dispatch provenance, rather than a service override.
    let (ctx, parent_id) = if let Some(originator) = &originator_override {
        let parent = lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::ProcessProvenance::new(originator.clone()),
            lash_core::Lifetime::Detached,
        );
        let record =
            lash_core::ProcessRegistrar::register_process(registry.as_ref(), parent.clone())
                .await
                .expect("register the host-origin parent");
        (
            ctx.with_process_execution(record.id.clone(), &parent, None),
            Some(record.id),
        )
    } else {
        (ctx, None)
    };
    let mut state = if language == "typescript" {
        RlmExecutionState::for_engine("typescript")
    } else {
        RlmExecutionState::new()
    };
    let response = execute_code_with_test_render(
        &mut state,
        ctx,
        ExecRequest {
            code: code.to_string(),
        },
        artifact_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        None,
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    handler.close().await.expect("close the cell's handler");
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(
        response.terminal_finish.is_some(),
        "process start must finish"
    );

    let records = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .expect("list trigger process")
        .into_iter()
        .filter(|record| Some(&record.id) != parent_id.as_ref())
        .collect::<Vec<_>>();
    let [record] = records.as_slice() else {
        panic!("expected exactly one trigger process, got {records:?}");
    };
    assert_eq!(
        record.provenance.originator,
        originator_override.unwrap_or_else(|| {
            lash_core::ProcessOriginator::session(lash_core::SessionScope::for_agent_frame(
                "test-session",
                lash_core::FrameNodeId::new("test-frame").expect("fixture frame"),
            ))
        })
    );
    // Deliver this fixture's start, rather than scanning every native Run's
    // obligations while their own-commit deliveries are settling.
    let relay = lash_core::runtime::process_start::ProcessStartRelay::new(
        table
            .backend()
            .obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
        registry.clone(),
        Arc::clone(table.backend().process_work().port()),
        Arc::new(lash_core::facade_support::SystemClock),
    );
    relay
        .deliver_start(&record.id)
        .await
        .expect("deliver the trigger process start");
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        table.await_terminal(&record.id),
    )
    .await
    .expect("trigger process reaches terminal state");
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
        trigger_effects: commands.lock_recover().clone(),
        subscriptions,
    }
}

#[test]
pub(super) fn bare_host_process_trigger_is_refused_before_store_mutation() {
    block_on(async {
        let registration = lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        );
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let context = lash_core::testing::code_execution_context_for_process(
            crate::testing::double_ports(&double, &handler),
            lash_core::ProcessId::fixture("host-process"),
            &registration,
        );
        let owner_error = context
            .trigger_owner_scope()
            .expect_err("a bare host process must not have a trigger owner namespace");
        assert!(
            owner_error.to_string().contains("bare host authority"),
            "{owner_error}"
        );
        drop(context);
        handler.close().await.expect("close the cell's handler");

        let result = execute_trigger_process_with_originator(
            "typescript",
            r#"
                const registrar = await processes.create({
                  dialect: "typescript",
                  source: 'const registrar = async () => await triggers.list({});'
                });
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
                const registrar = await processes.create({
                  dialect: "typescript",
                  source: 'const registrar = async () => await triggers.list({});'
                });
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
                const registrar = await processes.create({
                  dialect: "typescript",
                  source: `const registrar = async () => {
                    const listRegistrations = () => triggers.list({});
                    return await listRegistrations();
                  };`
                });
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
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
                  name: "scalar", subscription_key: "scalar"
                });
                const listed = await triggers.list({ target: { definition: remember } });
                const updated = await triggers.update({
                  subscription_key: "scalar", expected_revision: registered.revision,
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
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
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
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
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-update"
                });
                const registered_enable_seed = await triggers.register({
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-enable"
                });
                const enable_seed = await triggers.disable({
                  subscription_key: "batch-enable", expected_revision: registered_enable_seed.revision
                });
                const disable_seed = await triggers.register({
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-disable"
                });
                const delete_seed = await triggers.register({
                  source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
                  subscription_key: "batch-delete"
                });
                const results = await Promise.all([
                  triggers.register({
                    source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
                    subscription_key: "batch-register"
                  }),
                  triggers.list({}),
                  triggers.update({
                    subscription_key: "batch-update", expected_revision: update_seed.revision,
                    source, target: { definition: remember }, inputs: (event) => ({ tick: event }),
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
        // The host operations run inside their batch children. A registration
        // is a declaring leaf tool (FIG-3116): its child only declares the
        // intent, and the register effect lands when the intent is realized,
        // outside the child: all five registrations, the batched one
        // included, emit their register effect there.
        let (child_effects, realized_effects): (Vec<_>, Vec<_>) = batched
            .trigger_effects()
            .into_iter()
            .partition(|(effect_id, _)| effect_id.contains(":child:"));
        assert_eq!(
            child_effects
                .into_iter()
                .map(|(_, operation)| operation)
                .collect::<Vec<_>>(),
            ["list", "update", "enable", "disable", "delete"]
        );
        assert_eq!(
            realized_effects
                .iter()
                .filter(|(_, operation)| *operation == "register")
                .count(),
            5
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
                  target: { definition: remember },
                  inputs: (event) => ({ tick: event }),
                  name: "remembered",
                  subscription_key: "remembered"
                });
                const disabled = await triggers.disable({
                  subscription_key: "remembered",
                  expected_revision: handle.revision
                });
                const registrations = await triggers.list({ target: { definition: remember } });
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
pub(super) fn print_observation_preserves_typed_value_and_records_cut_metadata() {
    block_on(async {
        let large = "x".repeat(60 * 1024);
        let record = format!(
            "{{ output: {}, status: \"failed\", error: \"boom\", exit_code: 2, stderr: \"short\" }}",
            serde_json::to_string(&large).expect("string literal")
        );
        let (response, attachments) =
            super::lifecycle_and_diagnostics::execute_with_host_environment_and_archives(
                &format!("print({record});"),
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangHostCatalog::new(),
            )
            .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(response.observations.is_empty());
        let archive = response
            .output_archive
            .as_ref()
            .expect("aggregate is archived");
        let bytes = attachments
            .get(&archive.reference.id)
            .await
            .expect("exact archive")
            .bytes;
        let observations: Vec<lash_core::Observation> =
            serde_json::from_slice(&bytes).expect("observations");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].value["output"], large);
        assert!(observations[0].text.starts_with("[cut: "));
        assert!(observations[0].text.contains("narrow with history["));
        let metadata = &observations[0].projection;
        assert!(metadata.truncated, "{metadata:?}");
        assert!(metadata.original_chars > 60_000);
        assert!(metadata.projected_chars < metadata.original_chars);
        assert_eq!(metadata.limit_chars, 8_000);
    });
}

/// Console output is also rendered under the fixed character cap.
#[test]
pub(super) fn console_log_of_a_large_record_stops_at_the_char_cap() {
    block_on(async {
        let large = "x".repeat(60 * 1024);
        let code = format!(
            "console.log({{ output: {}, status: \"failed\" }});",
            serde_json::to_string(&large).expect("string literal")
        );
        let (response, attachments) =
            super::lifecycle_and_diagnostics::execute_with_host_environment_and_archives(
                &code,
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangHostCatalog::new(),
            )
            .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let archive = response.output_archive.as_ref().expect("archive");
        let bytes = attachments
            .get(&archive.reference.id)
            .await
            .expect("archive bytes")
            .bytes;
        let observations: Vec<lash_core::Observation> =
            serde_json::from_slice(&bytes).expect("observations");
        let metadata = &observations[0].projection;
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

#[test]
pub(super) fn subcap_prints_stay_fully_inline_including_empty_and_null_values() {
    block_on(async {
        let response = execute_with_host_environment(
            "print(\"\"); print(null); print({text: \"é🙂\", nested: [1, false]});",
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangHostCatalog::new(),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(response.output_archive.is_none());
        assert_eq!(
            response
                .observations
                .iter()
                .map(|print| print.value.clone())
                .collect::<Vec<_>>(),
            vec![
                serde_json::json!(""),
                serde_json::Value::Null,
                serde_json::json!({"text": "é🙂", "nested": [1, false]})
            ]
        );
    });
}
