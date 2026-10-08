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
    async fn resolve(
        &self,
        _cx: &lash_lashlang_runtime::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
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

/// A cell context whose tool surface carries `triggers.register`, a
/// declaring leaf tool (FIG-3116): the tool resolves the target against
/// `artifact_store`, and its trigger command runs in place with the cell's
/// trigger router, which publishes the cell's execution env through the
/// store the referrer ports hold.
pub(super) async fn trigger_tool_context(
    ports: lash_core::testing::TestExecutionPorts,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    artifact_store: &lashlang::LashlangArtifacts,
    invocation: Option<lash_core::RuntimeInvocation>,
) -> lash_core::RuntimeExecutionContext<'static> {
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
    let handler = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("session"),
        lash_core::TurnId::from("turn"),
    ))
    .await;
    let artifact_store = handler.artifacts();
    let response = execute_code_with_trigger_test_render(
        &mut state,
        trigger_tool_context(
            handler.ports(),
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
        let handler = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("session"),
            lash_core::TurnId::from("turn"),
        ))
        .await;
        let artifact_store = handler.artifacts();
        let response = execute_code_with_trigger_test_render(
            &mut state,
            trigger_tool_context(
                handler.ports(),
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

pub(super) async fn execute_with_host_environment(
    code: &str,
    abilities: lashlang::LashlangAbilities,
    resources: lashlang::LashlangHostCatalog,
) -> ExecResponse {
    execute_with_host_environment_and_archives(code, abilities, resources)
        .await
        .0
}

/// Run `code` under the trigger tool surface with `abilities` and
/// `resources`, answering its response and the attachment store its
/// archives went to.
pub(super) async fn execute_with_host_environment_and_archives(
    code: &str,
    abilities: lashlang::LashlangAbilities,
    resources: lashlang::LashlangHostCatalog,
) -> (
    ExecResponse,
    Arc<lash_core::facade_support::RuntimeAttachmentStore>,
) {
    let mut state = RlmExecutionState::new();
    // Triggers are catalogue presence rather than an ability now (FIG-2999), so
    // the harness always supplies the store: a program that never registers one
    // never reaches it.
    let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let artifact_store = handler.artifacts();
    let ctx = trigger_tool_context(
        handler.ports(),
        crate::testing::sqlite_memory_trigger_store().await,
        &artifact_store,
        None,
    )
    .await;
    let surface = LashlangSurface::new(
        abilities,
        lashlang::LashlangLanguageFeatures::default(),
        resources,
    );
    let attachments = ctx.attachment_store();
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
        lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::instructions(1_000_000),
            lashlang::ExecutionBound::Unbounded,
        ),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    (response, attachments)
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

                const pruned = await triggers.prune({ subscription_keys: [handle.subscription_key] });
                finish({ answer: "foreground ran", handle: handle, registrations: registrations, pruned: pruned });
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
        let handle = finish["handle"].as_object().expect("register handle");
        let listed = finish["registrations"][0].as_object().expect("list handle");
        let pruned = finish["pruned"][0].as_object().expect("prune handle");
        let expected_fields = [
            "type",
            "id",
            "subscription_key",
            "incarnation",
            "revision",
            "enabled",
            "disposition",
            "name",
            "source_type",
            "source_key",
            "source",
            "registrant",
            "target",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        for view in [handle, listed, pruned] {
            assert_eq!(
                view.keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                expected_fields
            );
            assert_eq!(view["type"], serde_json::json!("trigger_handle"));
            assert_eq!(view["id"], handle["subscription_key"]);
            assert_eq!(view["incarnation"], handle["incarnation"]);
        }
        assert_eq!(handle["disposition"], serde_json::json!("created"));
        assert_eq!(listed["disposition"], serde_json::Value::Null);
        assert_eq!(pruned["disposition"], serde_json::json!("deleted"));
        assert_eq!(pruned["revision"], serde_json::json!(2));
        assert_eq!(pruned["enabled"], serde_json::json!(false));
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            vec![
                ("triggers.register", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.list", lash_core::ExecutedCallOutcome::Ok),
                ("triggers.prune", lash_core::ExecutedCallOutcome::Ok),
            ],
            "trigger effects must appear in the executed-call ledger"
        );
    });
}

/// Run `code` in one cell under the trigger tool surface and answer the
/// subscriptions its registrations installed in the trigger store.
async fn registered_subscriptions(code: &str) -> Vec<lash_core::TriggerSubscriptionRecord> {
    let trigger_store = crate::testing::sqlite_memory_trigger_store().await;
    let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let artifact_store = host.artifacts();
    let mut state = RlmExecutionState::new();
    let response = execute_code_unbounded_with_test_render(
        &mut state,
        trigger_tool_context(host.ports(), trigger_store.clone(), &artifact_store, None).await,
        ExecRequest {
            code: code.to_string(),
        },
        artifact_store,
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        ),
        None,
        RlmProjectedBindings::default(),
        None,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    lash_core::TriggerStore::list_subscriptions(
        trigger_store.as_ref(),
        lash_core::TriggerSubscriptionFilter::for_session("test-session"),
    )
    .await
    .expect("list the cell's registrations")
}

/// A registration that names no subscription key derives it from its source:
/// a module that makes the same two registrations in the other order installs
/// the same key for each source, though its regenerated artifact gives the
/// targets another definition.
#[test]
pub(super) fn reordered_keyless_registration_calls_keep_derived_keys_across_module_regeneration() {
    block_on(async {
        let program = |first: &str, second: &str| {
            format!(
                r#"
                const remember = async (tick: timer.Tick) => tick.fired_at;
                const morning = timer.Schedule({{ expr: "0 8 * * *", tz: "UTC" }});
                const evening = timer.Schedule({{ expr: "0 18 * * *", tz: "UTC" }});
                await triggers.register({{ source: {first}, target: {{ definition: remember }}, inputs: (event) => ({{ tick: event }}) }});
                await triggers.register({{ source: {second}, target: {{ definition: remember }}, inputs: (event) => ({{ tick: event }}) }});
                finish(true);
                "#
            )
        };
        let first = Box::pin(registered_subscriptions(&program("morning", "evening"))).await;
        let second = Box::pin(registered_subscriptions(&program("evening", "morning"))).await;

        let keys = |records: &[lash_core::TriggerSubscriptionRecord]| {
            records
                .iter()
                .map(|record| (record.source_key.clone(), record.subscription_key.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(keys(&first).len(), 2, "two sources, two subscriptions");
        assert_eq!(keys(&first), keys(&second));
        let definitions = |records: &[lash_core::TriggerSubscriptionRecord]| {
            records
                .iter()
                .map(|record| record.target_identity.definition_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_ne!(
            definitions(&first),
            definitions(&second),
            "the reordered module's artifact was regenerated"
        );
    });
}

/// A trigger command issued inside a process a bare host started is refused
/// before it reaches the trigger store: a bare host owns no trigger
/// namespace. The handler a process body's trigger operation runs through
/// refuses the listing and the deletion of a live session subscription, and
/// the subscription stays as it was.
#[test]
pub(super) fn bare_host_process_trigger_is_refused_before_store_mutation() {
    use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;

    block_on(async {
        let trigger_store = crate::testing::sqlite_memory_trigger_store().await;
        let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let artifact_store = host.artifacts();
        let mut state = RlmExecutionState::new();
        let registered = execute_code_unbounded_with_test_render(
            &mut state,
            trigger_tool_context(host.ports(), trigger_store.clone(), &artifact_store, None).await,
            ExecRequest {
                code: r#"
                        const remember = async (tick: timer.Tick) => tick.fired_at;
                        const source = timer.Schedule({ expr: "0 8 * * *", tz: "UTC" });
                        finish(await triggers.register({
                          source,
                          target: { definition: remember },
                          inputs: (event) => ({ tick: event }),
                          subscription_key: "session-owned"
                        }));
                    "#
                .to_string(),
            },
            artifact_store.clone(),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                timer_trigger_resources(),
            ),
            None,
            RlmProjectedBindings::default(),
            None,
        )
        .await;
        assert!(registered.error.is_none(), "{:?}", registered.error);
        let handle = registered
            .terminal_finish
            .expect("the registration's handle");
        let all = || async {
            lash_core::TriggerStore::list_subscriptions(
                trigger_store.as_ref(),
                lash_core::TriggerSubscriptionFilter::default(),
            )
            .await
            .expect("list every subscription")
        };
        let before = all().await;
        assert_eq!(before.len(), 1);

        let registration = lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        );
        let context = lash_core::testing::TestExecutionContextBuilder::new(host.ports())
            .trigger_router(Some(lash_core::testing::test_trigger_router(
                trigger_store.clone(),
                crate::testing::sqlite_memory_process_registry().await,
            )))
            .build()
            .into_runtime()
            .with_process_execution(
                &lash_core::ProcessRecord::from_registration(
                    registration,
                    lash_core::ProcessId::fixture("bare-host-process"),
                ),
                None,
                None,
            );
        for (operation, payload) in [
            (lashlang::TriggerHostOperation::List, serde_json::json!({})),
            (
                lashlang::TriggerHostOperation::Delete,
                serde_json::json!({
                    "subscription_key": "session-owned",
                    "expected_revision": handle["revision"],
                }),
            ),
        ] {
            let refused = lash_lashlang_runtime::execute_trigger_operation(
                &lash_vm_client::service::Service::default(),
                &context,
                &artifact_store,
                operation,
                payload,
                format!("bare-host:{}", operation.host_operation()),
            )
            .await
            .expect_err("a bare host process owns no trigger namespace");
            assert!(
                refused.to_string().contains("bare host authority"),
                "{operation:?}: {refused}"
            );
        }
        assert_eq!(
            all().await,
            before,
            "a refused command leaves the store as it was"
        );
    });
}

/// The invocation of the cell `effect` of the test turn: each cell of a
/// session is its own execution, whose snapshots hold only its own source.
fn cell_invocation(effect: &str) -> lash_core::RuntimeInvocation {
    lash_core::testing::exec_code_invocation(
        "test-session",
        "test-turn",
        0,
        0,
        effect,
        format!("exec-code:trigger-registration:{effect}"),
    )
}

#[test]
pub(super) fn removing_a_declaration_and_running_unrelated_code_does_not_unregister() {
    block_on(async {
        let trigger_store = crate::testing::sqlite_memory_trigger_store().await;
        let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let artifact_store = host.artifacts();
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            timer_trigger_resources(),
        );
        let mut state = RlmExecutionState::new();

        let handler = &host;
        let first = execute_code_unbounded_with_test_render(
            &mut state,
            trigger_tool_context(
                handler.ports(),
                trigger_store.clone(),
                &artifact_store,
                Some(cell_invocation("register")),
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

        let handler = &host;
        let unrelated = execute_code_unbounded_with_test_render(
            &mut state,
            trigger_tool_context(
                handler.ports(),
                trigger_store.clone(),
                &artifact_store,
                Some(cell_invocation("unrelated")),
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
        let registration = lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        );
        // The environment the held registration captures.
        let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        lash_core::testing::process_execution_env_fixture(
            handler.backend().process_env_store().as_ref(),
        )
        .await;
        let context = lash_core::testing::code_execution_context_for_process(
            handler.ports(),
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
            handler.artifacts(),
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

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));
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
        let (response, attachments) = execute_with_host_environment_and_archives(
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
        let (response, attachments) = execute_with_host_environment_and_archives(
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
