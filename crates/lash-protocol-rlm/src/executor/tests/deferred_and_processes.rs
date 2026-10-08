use super::*;
use lash_core::plugin::PluginSessionRequest;

pub(super) struct CountingDeferredResolver {
    calls: Arc<AtomicUsize>,
    batches: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    installed: Arc<AtomicUsize>,
}

pub(super) fn deferred_fetch_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:web_fetch",
        "web_fetch",
        "Fetch a URL",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["web"], "fetch"))
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for CountingDeferredResolver {
    async fn resolve(
        &self,
        _cx: &lash_lashlang_runtime::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.batches
            .lock_recover()
            .push(paths.iter().map(|path| (*path).to_string()).collect());
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == "web.fetch" {
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition())
                            .with_source_id(lash_core::facade_support::PLUGIN_TOOL_SOURCE_ID),
                    ))
                } else {
                    lash_lashlang_runtime::Resolution::NotAvailable
                };
                ((*path).to_string(), resolution)
            })
            .collect()
    }

    fn install_recorded_grant(
        &self,
        path: &str,
        _grant: &lash_lashlang_runtime::ToolGrant,
    ) -> Result<(), lash_lashlang_runtime::RecordedGrantInstallError> {
        assert_eq!(path, "web.fetch");
        self.installed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

pub(super) struct BindingRecordingDeferredProvider {
    pub(super) executions: Arc<AtomicUsize>,
    pub(super) observed_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    pub(super) enumerations: Arc<AtomicUsize>,
}

/// The scope [`restricted_empty_deferred_context`] claims for `session_id`.
fn restricted_empty_deferred_scope(session_id: &str) -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_core::SessionId::fixture(session_id),
        lash_core::TurnId::from("test-turn"),
    )
}

async fn restricted_empty_deferred_context(
    handler: &crate::testing::DurableHost,
    provider: Arc<dyn lash_core::ToolProvider>,
    session_id: &str,
) -> (
    lash_core::RuntimeExecutionContext<'static>,
    Arc<lash_core::ToolRegistry>,
) {
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("deferred_grant_provider"),
        lash_core::plugin::PluginSpec::new().with_tool_provider(provider.clone()),
    )));
    let session = lash_core::facade_support::PluginHost::new(
        factories,
        lash_core::ExecutionBudgets::recommended(),
    )
    .build_session(PluginSessionRequest::creation(
        lash_core::SessionId::fixture(session_id),
        lash_core::plugin::SessionAuthorityContext {
            tool_access: lash_core::SessionToolAccess::restricted([])
                .expect("restricted empty is valid"),
            ..lash_core::plugin::SessionAuthorityContext::ambient_fixture()
        },
    ))
    .expect("restricted-empty deferred session");
    let catalog = session
        .resolved_tool_catalog()
        .expect("restricted-empty catalog");
    assert!(catalog.tools.is_empty());
    let registry = session.tool_registry();
    assert!(
        lash_core::ToolProvider::resolve_manifest_by_id(
            registry.as_ref(),
            &lash_core::ToolId::from("tool:web_fetch"),
        )
        .is_none(),
        "the separately grantable tool is absent from resident membership"
    );
    (
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            handler.ports(),
            provider,
            catalog.as_ref().clone(),
            lash_core::testing::exec_code_invocation(
                lash_core::SessionId::fixture(session_id),
                "test-turn",
                0,
                0,
                "exec-code",
                "exec-code:0",
            ),
        ),
        registry,
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for BindingRecordingDeferredProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:web_fetch"))
            .then(|| deferred_fetch_definition().manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.observed_bindings
                .lock_recover()
                .push(call.context.tool_execution_binding().clone());
            lash_core::ToolOutcome::ok(serde_json::json!("deferred ok"))
        })
        .await
        .into()
    }
}

pub(super) struct BindingDeferredResolver {
    pub(super) calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for BindingDeferredResolver {
    async fn resolve(
        &self,
        _cx: &lash_lashlang_runtime::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == "web.fetch" {
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition())
                            .with_source_id(lash_core::facade_support::PLUGIN_TOOL_SOURCE_ID)
                            .with_execution_binding(serde_json::json!({
                                "kind": "test",
                                "route": "deferred"
                            })),
                    ))
                } else {
                    lash_lashlang_runtime::Resolution::NotAvailable
                };
                ((*path).to_string(), resolution)
            })
            .collect()
    }
}

pub(super) fn deferred_matrix_request() -> ExecRequest {
    ExecRequest {
        code: "await web.fetch({});\nawait mystery.x({});".to_string(),
    }
}

#[test]
pub(super) fn deferred_link_is_scoped_to_the_exec_code_link() {
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let installed = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(CountingDeferredResolver {
                calls: Arc::clone(&calls),
                batches: Arc::clone(&batches),
                installed: Arc::clone(&installed),
            });
        // Both links of turn 1 run in that turn's handler, and turn 2 runs
        // in its own.
        let turn_1 = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("test-session"),
            lash_core::TurnId::from("turn-1"),
        ))
        .await;
        let turn_2 = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("test-session"),
            lash_core::TurnId::from("turn-2"),
        ))
        .await;
        let first_invocation = lash_core::testing::exec_code_invocation(
            "test-session",
            "turn-1",
            1,
            0,
            "effect-1",
            "replay:effect-1",
        );
        let first_ctx = lash_core::testing::code_execution_context_with_invocation(
            turn_1.ports(),
            first_invocation,
        );
        assert!(first_ctx.tool_catalog().tools.is_empty());
        let mut state = RlmExecutionState::new();
        let first = execute_code_unbounded_with_test_render(
            &mut state,
            first_ctx.clone(),
            deferred_matrix_request(),
            turn_1.artifacts(),
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            None,
        )
        .await;
        assert!(first.error.is_some(), "mystery.x must remain unresolved");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one batch per link");
        assert_eq!(installed.load(Ordering::SeqCst), 1);
        assert!(matches!(
            state
                .deferred_link
                .as_ref()
                .expect("active link")
                .get("web.fetch"),
            Some(lash_lashlang_runtime::Resolution::Resolved(_))
        ));
        assert!(matches!(
            state
                .deferred_link
                .as_ref()
                .expect("active link")
                .get("mystery.x"),
            Some(lash_lashlang_runtime::Resolution::NotAvailable)
        ));
        assert!(first_ctx.tool_catalog().tools.is_empty());

        let snapshot = hydrate_snapshot(
            state
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .await
                .expect("snapshot components"),
        );
        let mut restored = RlmExecutionState::new();
        restored
            .restore_execution_state(&snapshot, lash_core::FleetFormat::current())
            .await
            .expect("restore");

        assert!(
            restored.deferred_link.is_none(),
            "restore carries no tool outcomes"
        );
        let root: BTreeMap<String, serde::de::IgnoredAny> =
            rmp_serde::from_slice(&snapshot.root).expect("root");
        assert!(!root.contains_key("deferred_resolutions"));

        // A second code effect in the same logical turn is a different link
        // and must resolve the same paths against current authority.
        let second_ctx = lash_core::testing::code_execution_context_with_invocation(
            turn_1.ports(),
            lash_core::testing::exec_code_invocation(
                "test-session",
                "turn-1",
                1,
                0,
                "effect-2",
                "replay:effect-2",
            ),
        );
        let second_link = execute_code_unbounded_with_test_render(
            &mut restored,
            second_ctx.clone(),
            deferred_matrix_request(),
            turn_1.artifacts(),
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            None,
        )
        .await;
        assert!(second_link.error.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(installed.load(Ordering::SeqCst), 2);
        assert!(second_ctx.tool_catalog().tools.is_empty());

        // A new logical turn also selects a fresh record, even when the
        // program references exactly the same paths.
        let next_turn_ctx = lash_core::testing::code_execution_context_with_invocation(
            turn_2.ports(),
            lash_core::testing::exec_code_invocation(
                "test-session",
                "turn-2",
                2,
                0,
                "effect-3",
                "replay:effect-3",
            ),
        );
        let next_turn = execute_code_unbounded_with_test_render(
            &mut restored,
            next_turn_ctx.clone(),
            deferred_matrix_request(),
            turn_1.artifacts(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            None,
        )
        .await;
        assert!(next_turn.error.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(installed.load(Ordering::SeqCst), 3);
        assert!(next_turn_ctx.tool_catalog().tools.is_empty());
        assert_eq!(
            restored
                .deferred_link
                .as_ref()
                .expect("active link")
                .outcomes
                .len(),
            2
        );
        assert_eq!(
            *batches.lock_recover(),
            vec![
                vec!["mystery.x".to_string(), "web.fetch".to_string()],
                vec!["mystery.x".to_string(), "web.fetch".to_string()],
                vec!["mystery.x".to_string(), "web.fetch".to_string()],
            ]
        );
        drop((first_ctx, second_ctx, next_turn_ctx));
    });
}

#[test]
pub(super) fn deferred_call_executes_through_grant_without_mutating_catalog() {
    block_on(async {
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let observed_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let enumerations = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(BindingDeferredResolver {
                calls: Arc::clone(&resolver_calls),
            });
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::clone(&executions),
                observed_bindings: Arc::clone(&observed_bindings),
                enumerations: Arc::clone(&enumerations),
            });
        let handler = crate::testing::DurableHost::open(restricted_empty_deferred_scope(
            "restricted-empty-lashlang-deferred",
        ))
        .await;
        let (ctx, registry) = restricted_empty_deferred_context(
            &handler,
            provider,
            "restricted-empty-lashlang-deferred",
        )
        .await;
        let enumerations_after_catalog = enumerations.load(Ordering::SeqCst);
        assert!(ctx.tool_catalog().tools.is_empty());

        let mut state = RlmExecutionState::new();
        let response = execute_code_unbounded_with_test_render(
            &mut state,
            ctx.clone(),
            ExecRequest {
                code: r#"
                        const result = await web.fetch({ url: "https://example.test" });
                        finish(result);
                    "#
                .to_string(),
            },
            handler.artifacts(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!("deferred ok"))
        );
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            enumerations.load(Ordering::SeqCst),
            enumerations_after_catalog,
            "deferred execution must not re-enumerate provider residents"
        );
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            vec![("web.fetch", lash_core::ExecutedCallOutcome::Ok)],
            "the ledger must retain the source module.operation, not the host tool id"
        );
        assert_eq!(
            *observed_bindings.lock_recover(),
            vec![serde_json::json!({ "kind": "test", "route": "deferred" })]
        );
        assert!(ctx.tool_catalog().tools.is_empty());
        assert!(matches!(
            state
                .deferred_link
                .as_ref()
                .expect("active link")
                .get("web.fetch"),
            Some(lash_lashlang_runtime::Resolution::Resolved(_))
        ));
        assert!(
            lash_core::ToolProvider::resolve_manifest_by_id(
                registry.as_ref(),
                &lash_core::ToolId::from("tool:web_fetch"),
            )
            .is_none(),
            "grant execution must not promote the deferred tool into resident membership"
        );
        drop(ctx);
    });
}

/// A deferred resolution whose attempt dies at `fault` and is redriven: the
/// resolver runs again on the replay only if its answer never reached the
/// journal.

#[test]
pub(super) fn runtime_failure_after_prints_and_tool_calls_retains_collected_outputs() {
    block_on(async {
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let observed_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let enumerations = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(BindingDeferredResolver {
                calls: Arc::clone(&resolver_calls),
            });
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::clone(&executions),
                observed_bindings: Arc::clone(&observed_bindings),
                enumerations,
            });
        let handler = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("runtime-output-retention"),
            lash_core::TurnId::from("turn-1"),
        ))
        .await;
        let ctx =
            lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                handler.ports(),
                provider,
                lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
                lash_core::testing::exec_code_invocation(
                    "runtime-output-retention",
                    "turn-1",
                    0,
                    0,
                    "exec-code",
                    "exec-code:0",
                ),
            );

        let response = execute_code_unbounded_with_test_render(
            &mut RlmExecutionState::new(),
            ctx,
            ExecRequest {
                code: r#"
                        console.log("printed before failure");
                        await web.fetch({ url: "https://example.test" });
                        finish(JSON.parse("invalid_int"));
                    "#
                .to_string(),
            },
            handler.artifacts(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            None,
        )
        .await;

        assert!(
            response.error.is_some(),
            "execution should report runtime failure"
        );
        let error = response.error.as_ref().unwrap();
        assert!(
            error.message.contains("JSON") || error.message.contains("invalid_int"),
            "expected runtime error diagnostic, got: {}",
            error.message,
        );
        assert_eq!(response.observations.len(), 1);
        assert!(
            response.observations[0]
                .text
                .contains("printed before failure"),
            "observation should be retained despite runtime failure"
        );
        assert_eq!(
            response
                .calls
                .iter()
                .map(|call| (call.operation.as_str(), call.outcome))
                .collect::<Vec<_>>(),
            vec![("web.fetch", lash_core::ExecutedCallOutcome::Ok)],
            "executed tool call records should be retained despite runtime failure"
        );
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            *observed_bindings.lock_recover(),
            vec![serde_json::json!({ "kind": "test", "route": "deferred" })]
        );
    });
}
