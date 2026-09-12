use super::*;

struct FailingDeferredJournalController;

impl lash_core::AwaitEventResolver for FailingDeferredJournalController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for FailingDeferredJournalController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation }
                if operation.starts_with("deferred_tool_resolution:v1:")
        ) {
            local_executor.execute(envelope).await?;
            Err(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "injected deferred journal commit failure",
            ))
        } else {
            local_executor.execute(envelope).await
        }
    }
}

#[derive(Clone, Copy)]
enum SqliteDeferredFault {
    AfterResolverReturn,
    AfterDurableRecord,
}

struct FaultingSqliteDeferredController {
    inner: lash_sqlite_store::SqliteRuntimeEffectController,
    fault: SqliteDeferredFault,
}

impl lash_core::AwaitEventResolver for FaultingSqliteDeferredController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for FaultingSqliteDeferredController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let is_deferred = matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation }
                if operation.starts_with("deferred_tool_resolution:v1:")
        );
        if !is_deferred {
            return self.inner.execute_effect(envelope, local_executor).await;
        }
        match self.fault {
            SqliteDeferredFault::AfterResolverReturn => {
                local_executor.execute(envelope).await?;
            }
            SqliteDeferredFault::AfterDurableRecord => {
                self.inner.execute_effect(envelope, local_executor).await?;
            }
        }
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            match self.fault {
                SqliteDeferredFault::AfterResolverReturn => "injected crash after resolver return",
                SqliteDeferredFault::AfterDurableRecord => "injected crash after durable record",
            },
        ))
    }
}

pub(super) struct CountingDeferredResolver {
    calls: Arc<AtomicUsize>,
    batches: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    installed: Arc<AtomicUsize>,
}

struct RetryOnceInstallResolver {
    calls: Arc<AtomicUsize>,
    installs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for RetryOnceInstallResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        paths
            .iter()
            .map(|path| {
                (
                    (*path).to_string(),
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition()),
                    )),
                )
            })
            .collect()
    }

    fn install_recorded_grant(
        &self,
        _path: &str,
        _grant: &lash_lashlang_runtime::ToolGrant,
    ) -> Result<(), lash_lashlang_runtime::RecordedGrantInstallError> {
        if self.installs.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(lash_lashlang_runtime::RecordedGrantInstallError::transient(
                "injected crash before registration",
            ))
        } else {
            Ok(())
        }
    }
}

pub(super) fn deferred_fetch_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:web_fetch",
        "web_fetch",
        "Fetch a URL",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["web"], "fetch"))
}

fn ambient_definition(
    id: &str,
    name: &str,
    module: &str,
    operation: &str,
) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        id,
        name,
        "Ambient replacement",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "boolean" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new([module], operation))
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for CountingDeferredResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.batches
            .lock_recover()
            .push(paths.iter().map(|path| (*path).to_string()).collect());
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == "web.fetch" {
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(deferred_fetch_definition()),
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

fn restricted_empty_deferred_context(
    provider: Arc<dyn lash_core::ToolProvider>,
    session_id: &str,
) -> (
    lash_core::RuntimeExecutionContext<'static>,
    Arc<lash_core::ToolRegistry>,
) {
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "deferred_grant_provider",
        lash_core::plugin::PluginSpec::new().with_tool_provider(provider),
    )));
    let session = lash_core::facade_support::PluginHost::new(factories)
        .build_session_with_parent(
            session_id,
            None,
            lash_core::plugin::SessionCreationConfig {
                authority: lash_core::plugin::SessionAuthorityContext {
                    tool_access: lash_core::SessionToolAccess::restricted([])
                        .expect("restricted empty is valid"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("restricted-empty deferred session");
    let catalog = session
        .resolved_tool_catalog(&lash_core::SessionId::from(session_id))
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
            session.tools(),
            catalog.as_ref().clone(),
            lash_core::testing::exec_code_invocation(
                session_id,
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.observed_bindings
            .lock_recover()
            .push(call.context.tool_execution_binding().clone());
        lash_core::ToolOutcome::ok(serde_json::json!("deferred ok"))
    }
}

pub(super) struct BindingDeferredResolver {
    pub(super) calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for BindingDeferredResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
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
        language: "lashlang".to_string(),
        code: "await web.fetch({})?\nawait mystery.x({})?".to_string(),
    }
}

#[test]
pub(super) fn deferred_resolution_record_is_scoped_to_the_exec_code_link() {
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

        let first_invocation = lash_core::testing::exec_code_invocation(
            "test-session",
            "turn-1",
            1,
            0,
            "effect-1",
            "replay:effect-1",
        );
        let first_ctx =
            lash_core::testing::code_execution_context_with_invocation(first_invocation);
        assert!(first_ctx.tool_catalog().tools.is_empty());
        let mut state = RlmExecutionState::new();
        let first = execute_code_unbounded_for_tests(
            &mut state,
            first_ctx.clone(),
            deferred_matrix_request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_some(), "mystery.x must remain unresolved");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one batch per link");
        assert_eq!(installed.load(Ordering::SeqCst), 1);
        assert!(matches!(
            state.deferred_resolutions.get("web.fetch"),
            Some(lash_lashlang_runtime::Resolution::Resolved(_))
        ));
        assert!(matches!(
            state.deferred_resolutions.get("mystery.x"),
            Some(lash_lashlang_runtime::Resolution::NotAvailable)
        ));
        assert!(first_ctx.tool_catalog().tools.is_empty());

        let snapshot = hydrate_snapshot(
            state
                .snapshot_execution_state()
                .expect("snapshot components"),
        );
        let mut restored = RlmExecutionState::new();
        restored
            .restore_execution_state(&snapshot)
            .expect("restore");

        // Same stable link: both positive and negative outcomes survive the
        // snapshot and win without another authorization decision.
        let replay = execute_code_unbounded_for_tests(
            &mut restored,
            first_ctx.clone(),
            deferred_matrix_request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(replay.error.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "same link must replay");
        assert_eq!(installed.load(Ordering::SeqCst), 2);

        // A second code effect in the same logical turn is a different link
        // and must resolve the same paths against current authority.
        let second_ctx = lash_core::testing::code_execution_context_with_invocation(
            lash_core::testing::exec_code_invocation(
                "test-session",
                "turn-1",
                1,
                0,
                "effect-2",
                "replay:effect-2",
            ),
        );
        let second_link = execute_code_unbounded_for_tests(
            &mut restored,
            second_ctx.clone(),
            deferred_matrix_request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(second_link.error.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(installed.load(Ordering::SeqCst), 3);
        assert!(second_ctx.tool_catalog().tools.is_empty());

        // A new logical turn also selects a fresh record, even when the
        // program references exactly the same paths.
        let next_turn_ctx = lash_core::testing::code_execution_context_with_invocation(
            lash_core::testing::exec_code_invocation(
                "test-session",
                "turn-2",
                2,
                0,
                "effect-3",
                "replay:effect-3",
            ),
        );
        let next_turn = execute_code_unbounded_for_tests(
            &mut restored,
            next_turn_ctx.clone(),
            deferred_matrix_request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(next_turn.error.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(installed.load(Ordering::SeqCst), 4);
        assert!(next_turn_ctx.tool_catalog().tools.is_empty());
        assert_eq!(restored.deferred_resolutions.resolutions.len(), 2);
        assert_eq!(
            *batches.lock_recover(),
            vec![
                vec!["mystery.x".to_string(), "web.fetch".to_string()],
                vec!["mystery.x".to_string(), "web.fetch".to_string()],
                vec!["mystery.x".to_string(), "web.fetch".to_string()],
            ]
        );
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
        let (ctx, registry) =
            restricted_empty_deferred_context(provider, "restricted-empty-lashlang-deferred");
        let enumerations_after_catalog = enumerations.load(Ordering::SeqCst);
        assert!(ctx.tool_catalog().tools.is_empty());

        let mut state = RlmExecutionState::new();
        let response = execute_code_unbounded_for_tests(
            &mut state,
            ctx.clone(),
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"
                        result = await web.fetch({ url: "https://example.test" })?
                        finish result
                    "#
                .to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
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
            state.deferred_resolutions.get("web.fetch"),
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
    });
}

#[test]
pub(super) fn deferred_journal_failure_prevents_dependent_tool_execution() {
    block_on(async {
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::clone(&executions),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            });
        let ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_effect_controller_and_invocation(
            Arc::clone(&provider),
            lash_core::ToolCatalog::default(),
            Arc::new(FailingDeferredJournalController),
            lash_core::testing::exec_code_invocation(
                "deferred-journal-failure",
                "turn-1",
                0,
                0,
                "exec-code",
                "exec-code:0",
            ),
        );
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(BindingDeferredResolver {
                calls: Arc::clone(&resolver_calls),
            });

        let response = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            ctx,
            ExecRequest {
                language: "lashlang".into(),
                code: r#"finish await web.fetch({ url: "https://example.test" })?"#.into(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;

        assert!(
            response.error.is_some(),
            "journal failure must abort linking"
        );
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    });
}

async fn run_sqlite_deferred_fault_boundary(
    fault: SqliteDeferredFault,
    expected_resolver_calls_after_reopen: usize,
) {
    let dir = tempfile::tempdir().expect("temporary effect journal");
    let file_name = match fault {
        SqliteDeferredFault::AfterResolverReturn => "after-resolver.sqlite",
        SqliteDeferredFault::AfterDurableRecord => "after-record.sqlite",
    };
    let path = dir.path().join(file_name);
    let session_id = format!("sqlite-{file_name}");
    let turn_id = "turn-1";
    let replay_key = "exec-code:fault";
    let scope = lash_core::ExecutionScope::turn(&session_id, turn_id);
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let installs = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
        Arc::new(CountingDeferredResolver {
            calls: Arc::clone(&resolver_calls),
            batches: Default::default(),
            installed: Arc::clone(&installs),
        });
    let provider: Arc<dyn lash_core::ToolProvider> = Arc::new(BindingRecordingDeferredProvider {
        executions: Arc::clone(&executions),
        observed_bindings: Default::default(),
        enumerations: Default::default(),
    });
    let request = ExecRequest {
        language: "lashlang".into(),
        code: r#"finish await web.fetch({ url: "https://example.test" })?"#.into(),
    };
    let controller = lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
        .await
        .expect("open SQLite effect controller");
    let first_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
        Arc::clone(&provider),
        lash_core::ToolCatalog::default(),
        lash_core::ScopedEffectController::shared(
            Arc::new(FaultingSqliteDeferredController { inner: controller, fault }),
            scope.clone(),
        )
        .expect("admit SQLite fault controller scope"),
        lash_core::testing::exec_code_invocation(
            &session_id, turn_id, 0, 0, "faulting exec", replay_key,
        ),
    );
    let first = execute_code_unbounded_for_tests(
        &mut RlmExecutionState::new(),
        first_ctx,
        request.clone(),
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        Some(resolver.clone()),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
    )
    .await;
    assert!(first.error.is_some(), "boundary fault must stop execution");
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
    assert_eq!(installs.load(Ordering::SeqCst), 0);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "a failed resolution boundary must prevent the dependent tool effect"
    );

    let reopened = lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
        .await
        .expect("cold-reopen SQLite effect controller");
    let replay_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
        provider,
        lash_core::ToolCatalog::default(),
        lash_core::ScopedEffectController::shared(Arc::new(reopened), scope)
            .expect("admit reopened SQLite controller scope"),
        lash_core::testing::exec_code_invocation(
            &session_id, turn_id, 0, 0, "faulting exec", replay_key,
        ),
    );
    let replay = execute_code_unbounded_for_tests(
        &mut RlmExecutionState::new(),
        replay_ctx,
        request,
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        Some(resolver),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
    )
    .await;
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(
        resolver_calls.load(Ordering::SeqCst),
        expected_resolver_calls_after_reopen
    );
    assert_eq!(installs.load(Ordering::SeqCst), 1);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[test]
pub(super) fn sqlite_fault_after_resolver_return_repeats_discovery_after_reopen() {
    block_on(run_sqlite_deferred_fault_boundary(
        SqliteDeferredFault::AfterResolverReturn,
        2,
    ));
}

#[test]
pub(super) fn sqlite_fault_after_durable_record_never_reresolves_after_reopen() {
    block_on(run_sqlite_deferred_fault_boundary(
        SqliteDeferredFault::AfterDurableRecord,
        1,
    ));
}

#[test]
pub(super) fn sqlite_fault_before_registration_reinstalls_recorded_route_after_reopen() {
    block_on(async {
        let dir = tempfile::tempdir().expect("temporary effect journal");
        let path = dir.path().join("before-registration.sqlite");
        let session_id = "sqlite-before-registration";
        let turn_id = "turn-1";
        let replay_key = "exec-code:registration";
        let scope = lash_core::ExecutionScope::turn(session_id, turn_id);
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let installs = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(RetryOnceInstallResolver {
                calls: Arc::clone(&resolver_calls),
                installs: Arc::clone(&installs),
            });
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::clone(&executions),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            });
        let request = ExecRequest {
            language: "lashlang".into(),
            code: r#"finish await web.fetch({ url: "https://example.test" })?"#.into(),
        };
        let first_controller =
            lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
                .await
                .expect("open SQLite effect controller");
        let first_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            Arc::clone(&provider),
            lash_core::ToolCatalog::default(),
            lash_core::ScopedEffectController::shared(Arc::new(first_controller), scope.clone())
                .expect("admit SQLite controller scope"),
            lash_core::testing::exec_code_invocation(
                session_id, turn_id, 0, 0, "registration exec", replay_key,
            ),
        );
        let first = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            first_ctx,
            request.clone(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_some());
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(installs.load(Ordering::SeqCst), 1);
        assert_eq!(executions.load(Ordering::SeqCst), 0);

        let reopened = lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
            .await
            .expect("cold-reopen SQLite effect controller");
        let replay_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            Arc::clone(&provider),
            lash_core::ToolCatalog::default(),
            lash_core::ScopedEffectController::shared(Arc::new(reopened), scope)
                .expect("admit reopened SQLite controller scope"),
            lash_core::testing::exec_code_invocation(
                session_id, turn_id, 0, 0, "registration exec", replay_key,
            ),
        );
        let replay = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            replay_ctx,
            request,
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(replay.error.is_none(), "{:?}", replay.error);
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(installs.load(Ordering::SeqCst), 2);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    });
}

#[test]
pub(super) fn sqlite_reopen_replays_positive_before_ambient_collision_without_resolver() {
    block_on(async {
        let dir = tempfile::tempdir().expect("temporary effect journal");
        let path = dir.path().join("positive-deferred.sqlite");
        let session_id = "sqlite-positive-deferred";
        let turn_id = "turn-1";
        let replay_key = "exec-code:positive";
        let scope = lash_core::ExecutionScope::turn(session_id, turn_id);
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let installed = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(CountingDeferredResolver {
                calls: Arc::clone(&resolver_calls),
                batches: Default::default(),
                installed: Arc::clone(&installed),
            });
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Default::default(),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            });
        let request = ExecRequest {
            language: "lashlang".into(),
            code: r#"
                if false {
                    ignored = await web.fetch({ url: "https://example.test" })?
                }
                finish "journaled-positive"
            "#
            .into(),
        };

        let first_controller =
            lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
                .await
                .expect("open SQLite effect controller");
        let first_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            Arc::clone(&provider),
            lash_core::ToolCatalog::default(),
            lash_core::ScopedEffectController::shared(Arc::new(first_controller), scope.clone())
                .expect("admit SQLite controller scope"),
            lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                0,
                0,
                "original descriptive label",
                replay_key,
            ),
        );
        let first = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            first_ctx,
            request.clone(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(installed.load(Ordering::SeqCst), 1);

        let collision_controller = lash_sqlite_store::SqliteRuntimeEffectController::open(
            &path,
            lash_core::ExecutionScope::turn(session_id, turn_id),
        )
        .await
        .expect("reopen SQLite effect controller for unrelated collision");
        collision_controller.start_replay();
        let unrelated_collision_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            Arc::clone(&provider),
            lash_core::ToolCatalog::from_tool_definitions(vec![
                ambient_definition("tool:other_a", "other_a", "other", "run"),
                ambient_definition("tool:other_b", "other_b", "other", "run"),
            ]),
            lash_core::ScopedEffectController::shared(
                Arc::new(collision_controller),
                lash_core::ExecutionScope::turn(session_id, turn_id),
            )
            .expect("admit unrelated collision replay scope"),
            lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                98,
                42,
                "another descriptive label",
                replay_key,
            ),
        );
        let unrelated_collision = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            unrelated_collision_ctx,
            ExecRequest {
                language: "lashlang".into(),
                code: r#"
                    if false {
                        ignored = await web.fetch({ url: "https://example.test" })?
                    }
                    finish "journaled-positive"
                "#
                .into(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        let error = unrelated_collision
            .error
            .expect("unrelated ambient collision remains an ordinary host failure");
        assert!(
            !error
                .message
                .contains("failed to commit deferred resolution"),
            "the deferred journal replay succeeded before the unrelated collision: {error:?}"
        );

        // Reopen from the file-backed production journal with an empty state
        // projection, no resolver/installer, changed parent attribution and a
        // changed descriptive label. Two new ambient claimants would collide
        // if catalog construction ran before the recorded path was masked.
        let replay_controller =
            lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
                .await
                .expect("reopen SQLite effect controller");
        replay_controller.start_replay();
        let changed_catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
            ambient_definition("tool:ambient_a", "ambient_a", "web", "fetch"),
            ambient_definition("tool:ambient_b", "ambient_b", "web", "fetch"),
        ]);
        let replay_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            provider,
            changed_catalog,
            lash_core::ScopedEffectController::shared(Arc::new(replay_controller), scope)
                .expect("admit reopened SQLite controller scope"),
            lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                97,
                41,
                "renamed descriptive label",
                replay_key,
            ),
        );
        let replay = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            replay_ctx,
            request,
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(replay.error.is_none(), "{:?}", replay.error);
        assert_eq!(
            replay.terminal_finish,
            Some(serde_json::json!("journaled-positive"))
        );
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(installed.load(Ordering::SeqCst), 1);
    });
}

#[test]
pub(super) fn sqlite_reopen_replays_negative_before_changed_ambient_without_resolver() {
    block_on(async {
        let dir = tempfile::tempdir().expect("temporary effect journal");
        let path = dir.path().join("negative-deferred.sqlite");
        let session_id = "sqlite-negative-deferred";
        let turn_id = "turn-1";
        let replay_key = "exec-code:negative";
        let scope = lash_core::ExecutionScope::turn(session_id, turn_id);
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(CountingDeferredResolver {
                calls: Arc::clone(&resolver_calls),
                batches: Default::default(),
                installed: Default::default(),
            });
        let request = deferred_matrix_request();
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Default::default(),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            });
        let first_controller =
            lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
                .await
                .expect("open SQLite effect controller");
        let first_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            Arc::clone(&provider),
            lash_core::ToolCatalog::default(),
            lash_core::ScopedEffectController::shared(Arc::new(first_controller), scope.clone())
                .expect("admit SQLite controller scope"),
            lash_core::testing::exec_code_invocation(
                session_id, turn_id, 0, 0, "negative original", replay_key,
            ),
        );
        let first = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            first_ctx,
            request.clone(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_some(), "mystery.x is durably unavailable");
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);

        let replay_controller =
            lash_sqlite_store::SqliteRuntimeEffectController::open(&path, scope.clone())
                .await
                .expect("reopen SQLite effect controller");
        replay_controller.start_replay();
        let replay_ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(
            provider,
            lash_core::ToolCatalog::from_tool_definitions(vec![ambient_definition(
                "tool:ambient_mystery",
                "ambient_mystery",
                "mystery",
                "x",
            )]),
            lash_core::ScopedEffectController::shared(Arc::new(replay_controller), scope)
                .expect("admit reopened SQLite controller scope"),
            lash_core::testing::exec_code_invocation(
                session_id, turn_id, 12, 33, "negative renamed", replay_key,
            ),
        );
        let replay = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            replay_ctx,
            request,
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        let error = replay
            .error
            .expect("journaled negative must mask the newly live ambient tool");
        assert!(error.message.contains("mystery.x"), "{error:?}");
        assert!(
            !error
                .message
                .contains("failed to commit deferred resolution")
        );
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
pub(super) fn typescript_deferred_call_executes_through_the_same_grant_path() {
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
        let (ctx, registry) =
            restricted_empty_deferred_context(provider, "restricted-empty-typescript-deferred");
        let enumerations_after_catalog = enumerations.load(Ordering::SeqCst);

        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_dialect_and_bounds(
                &mut state,
                ctx.clone(),
                ExecRequest {
                    language: "typescript".to_string(),
                    code: "const result = await web.fetch({ url: 'https://example.test' }); finish(result);".to_string(),
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                Some(resolver),
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                RlmSourceContext::cell(SourceDialect::Typescript),
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
        assert!(ctx.tool_catalog().tools.is_empty());
        assert!(matches!(
            state.deferred_resolutions.get("web.fetch"),
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
    });
}

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
        let ctx =
            lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
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

        let response = execute_code_unbounded_for_tests(
            &mut RlmExecutionState::new(),
            ctx,
            ExecRequest {
                language: "lashlang".to_string(),
                code: r#"
                        print "printed before failure"
                        _ = await web.fetch({ url: "https://example.test" })?
                        finish to_int("invalid_int")
                    "#
                .to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;

        assert!(
            response.error.is_some(),
            "execution should report runtime failure"
        );
        let error = response.error.as_ref().unwrap();
        assert!(
            error.message.contains("to_int") || error.message.contains("invalid_int"),
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

#[test]
pub(super) fn process_handle_derivation_matches_runtime_await_authority() {
    let globals = lashlang::from_json(serde_json::json!({
        "canonical": { "__handle__": "process", "id": "p1" },
        "alternate": { "handle": "p2" },
        "empty_id": { "__handle__": "process", "id": "" },
        "other_kind": { "__handle__": "tool", "id": "t1" },
        "plain_record": { "id": "not-a-handle" },
        "scalar": 1,
    }));
    let globals = globals.as_record().expect("fixture globals are a record");

    assert_eq!(
        process_handle_names(globals),
        BTreeSet::from([
            "alternate".to_string(),
            "canonical".to_string(),
            "empty_id".to_string(),
            "other_kind".to_string(),
        ])
    );
}

#[test]
pub(super) fn execute_code_stores_process_module_artifact_once() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let request = || ExecRequest {
            language: "lashlang".to_string(),
            code: "process later() { finish 1 }\nfinish 1".to_string(),
        };
        let resolver = || Arc::new(ProjectionRegistry::new());
        let context = || lash_core::testing::code_execution_context();
        let surface = || {
            LashlangSurface::new(
                lashlang::LashlangAbilities::default().with_processes(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            )
        };

        let first = execute_code_unbounded_for_tests(
            &mut state,
            context(),
            request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            surface(),
            None,
            RlmProjectedBindings::default(),
            resolver(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(state.stored_lashlang_modules.len(), 1);

        let second = execute_code_unbounded_for_tests(
            &mut state,
            context(),
            request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            surface(),
            None,
            RlmProjectedBindings::default(),
            resolver(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(second.error.is_none(), "{:?}", second.error);
        assert_eq!(state.stored_lashlang_modules.len(), 1);
        let stats = state.linked_programs.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    });
}

#[test]
pub(super) fn typescript_executor_stores_a_typescript_process_artifact() {
    block_on(async {
        let artifact_store = Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_dialect_and_bounds(
            &mut state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                        const worker = defineProcess({
                          name: "worker", signals: {},
                          run: async (input: unknown) => { return input; }
                        });
                        finish(1);
                    "#
                .to_string(),
            },
            artifact_store.clone(),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default().with_processes(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            RlmSourceContext::cell(SourceDialect::Typescript),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let module_ref = state
            .stored_lashlang_modules
            .iter()
            .next()
            .expect("stored process module");
        let artifact = lashlang::LashlangArtifactStore::get_module_artifact(
            artifact_store.as_ref(),
            module_ref,
        )
        .await
        .expect("read stored artifact")
        .expect("artifact exists");
        assert_eq!(
            artifact.compilation_dialect,
            lashlang::CompilationDialect::Typescript
        );
    });
}

#[derive(Clone)]
pub(super) struct TypeScriptSignalProcessService {
    pub(super) registry: Arc<lash_core::TestLocalProcessRegistry>,
    pub(super) controller: Arc<dyn lash_core::RuntimeEffectController>,
    pub(super) originator_override: Option<lash_core::ProcessOriginator>,
}

pub(super) struct EmptyTypeScriptSignalToolProvider;

pub(super) fn status_inspect_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:status_inspect",
        "status_inspect",
        "Inspect a process status",
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string" }
            },
            "required": ["process_id"]
        }),
        serde_json::json!({ "type": "string" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["status_tool"],
        "inspect",
    ))
}

pub(super) struct TypeScriptProcessInspectionToolProvider {
    inspected_process_id: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for TypeScriptProcessInspectionToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![status_inspect_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "status_inspect" || name == "tool:status_inspect")
            .then(|| Arc::new(status_inspect_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        if call.name == "status_inspect" || call.name == "tool:status_inspect" {
            let pid = call
                .args
                .get("process_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            *self.inspected_process_id.lock().unwrap() = pid;
            lash_core::ToolOutcome::ok(serde_json::json!("inspected-ok"))
        } else {
            lash_core::ToolOutcome::err(serde_json::json!(format!("unknown tool `{}`", call.name)))
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for EmptyTypeScriptSignalToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        lash_core::ToolOutcome::err(serde_json::json!(format!(
            "signal round-trip test has no tool `{}`",
            call.name
        )))
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessService for TypeScriptSignalProcessService {
    // The recorded-intent routes belong to atomic tool attempts, which this
    // signal fixture never opens. Refuse them rather than pretend, so a test
    // that starts using them fails loudly instead of silently taking a
    // non-atomic path.
    async fn start_from_recorded_intent(
        &self,
        _session_id: &SessionId,
        _request: lash_core::ProcessStartRequest,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessHandleView, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "recorded process starts are unavailable in this test".to_string(),
        ))
    }

    async fn cancel_recorded_intent(
        &self,
        _session_id: &SessionId,
        _process_id: &ProcessId,
        _reason: Option<String>,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "recorded process cancellation is unavailable in this test".to_string(),
        ))
    }

    async fn finish_recorded_intent_parent(
        &self,
        _session_id: &SessionId,
        _identity: lash_core::ToolIntentIdentity,
        _process_id: ProcessId,
        _policy: lash_core::ProcessParentEndPolicy,
        _reason: String,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ToolIntentParentEndOutcome, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "recorded parent end is unavailable in this test".to_string(),
        ))
    }

    async fn signal_recorded_intent(
        &self,
        _session_id: &SessionId,
        _process_id: &ProcessId,
        _signal: String,
        _call_id: String,
        _payload: serde_json::Value,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "recorded process signals are unavailable in this test".to_string(),
        ))
    }

    async fn emit_event_recorded_intent(
        &self,
        _session_id: &SessionId,
        _process_id: &ProcessId,
        _event: String,
        _call_id: String,
        _payload: serde_json::Value,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "recorded process events are unavailable in this test".to_string(),
        ))
    }

    async fn start(
        &self,
        session_id: &SessionId,
        mut registration: lash_core::ProcessRegistration,
        options: lash_core::ProcessStartOptions,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
        let (originator, wake_session_id) = self
            .originator_override
            .clone()
            .map(|originator| (originator, None))
            .or_else(|| {
                options
                    .spawn_provenance
                    .map(|spawn| (spawn.originator, spawn.wake_session_id))
            })
            .unwrap_or_else(|| {
                (
                    lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session_id)),
                    Some(session_id.clone()),
                )
            });
        if self.originator_override.is_some()
            && matches!(originator, lash_core::ProcessOriginator::Host { .. })
        {
            // This fixture's override emulates a host-admin start, including its host parent.
            registration.lifecycle.parent = lash_core::ParentScope::Host;
        }
        registration = registration
            .with_process_provenance(lash_core::ProcessProvenance::new(originator))
            .with_wake_session_id(wake_session_id);
        lash_core::ProcessRegistrar::register_process_with_observers(
            self.registry.as_ref(),
            registration,
            &options.initial_observers,
        )
        .await
    }

    async fn await_process(
        &self,
        process_id: &ProcessId,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessAwaitOutput, lash_core::PluginError> {
        let registry: Arc<dyn lash_core::ProcessRegistry> = self.registry.clone();
        lash_core::NativeProcessWork::for_registry(registry)
            .await_terminal(process_id)
            .await
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: lash_core::ProcessListMode,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<Vec<lash_core::ProcessRecord>, lash_core::PluginError> {
        match mode {
            lash_core::ProcessListMode::Live => {
                self.registry.list_live_observed_by(session_id).await
            }
            lash_core::ProcessListMode::All => {
                self.registry
                    .list_observed_by(
                        session_id,
                        &lash_core::ProcessListFilter {
                            status: lash_core::ProcessStatusFilter::Any,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }
    }

    async fn validate_visible(
        &self,
        session_id: &SessionId,
        process_ids: &[ProcessId],
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<(), lash_core::PluginError> {
        for process_id in process_ids {
            if !self.registry.is_observer(session_id, process_id).await? {
                return Err(lash_core::PluginError::Session(format!(
                    "process `{process_id}` is not visible"
                )));
            }
        }
        Ok(())
    }

    async fn cancel(
        &self,
        _session_id: &SessionId,
        _process_id: &ProcessId,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "process cancellation is unused in this test".to_string(),
        ))
    }

    async fn signal_possessed(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: Value,
        scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
        // The signal itself goes through the shared effect-backed service,
        // which wires the process effect controller the durable signal
        // route requires. What this fixture adds is the waiter side: the
        // await key the TypeScript program is parked on has to resolve
        // with the delivered payload.
        let event = lash_core::testing::effect_backed_process_service(self.registry.clone())
            .signal_possessed(
                session_id,
                process_id,
                signal_name.clone(),
                signal_id,
                payload,
                scope,
            )
            .await?;
        let event = Box::new(event);
        let ordinal = lash_core::ProcessEventLog::count_events_through(
            self.registry.as_ref(),
            process_id,
            event.event_type.as_str(),
            event.sequence,
        )
        .await?;
        let key = self
            .controller
            .await_event_key(
                &lash_core::ExecutionScope::process(process_id),
                lash_core::AwaitEventWaitIdentity::process_signal(
                    process_id,
                    &signal_name,
                    ordinal,
                ),
            )
            .await
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
        // The durable signal route resolves the waiter itself, so this
        // fixture asserts the delivery rather than performing it: a second
        // resolution must report the terminal the program will observe.
        let resolved = self
            .controller
            .resolve_await_event(&key, lash_core::Resolution::Ok(event.payload.clone()))
            .await
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
        assert_eq!(
            resolved,
            lash_core::ResolveOutcome::AlreadyResolved {
                terminal: lash_core::Resolution::Ok(event.payload.clone()),
            },
            "the durable signal route must have delivered the payload to the waiter"
        );
        Ok(*event)
    }

    async fn transfer(
        &self,
        _from_session_id: &SessionId,
        _to_session_id: &SessionId,
        _process_ids: Vec<ProcessId>,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "process transfer is unused in this test".to_string(),
        ))
    }
}

#[tokio::test]
pub(super) async fn typescript_signal_round_trip_crosses_protocol_and_process_engine() {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
        lash_core::facade_support::NativeRuntimeEffectController::default()
            .allow_process_lifetime_completion_keys(),
    );
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_process_signals(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("TypeScript signal test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(
            lash_core::facade_support::NativeEffectHost::new(controller.clone())
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
    let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn);
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
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid test native substrate config");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        controller: controller.clone(),
        originator_override: None,
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        Arc::new(EmptyTypeScriptSignalToolProvider),
        lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
        None,
        processes,
        controller,
        process_env_store,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_dialect_and_bounds(
        &mut state,
        ctx.clone(),
        ExecRequest {
            language: "typescript".to_string(),
            code: r#"
                    const worker = defineProcess({
                      name: "worker", signals: { ready: null },
                      run: async () => await waitSignal("ready")
                    });
                    const handle = start(worker);
                    wake(handle, "ready", { ok: true });
                    finish("signal-sent");
                "#
            .to_string(),
        },
        artifact_store.clone(),
        surface.clone(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(SourceDialect::Typescript),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!("signal-sent"))
    );

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive signalled TypeScript process");
    let records = registry
        .list_observed_by(
            &SessionId::from("test-session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list started TypeScript process");
    let [record] = records.as_slice() else {
        panic!("expected exactly one started TypeScript process, got {records:?}");
    };
    assert_eq!(
        record.lifecycle,
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Turn {
                session_id: SessionId::from("test-session"),
                turn_id: lash_core::TurnId::from("test-turn")
            },
            lash_core::OnParentEnd::Abandon,
        )
    );
    let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
    let terminal = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        lash_core::NativeProcessWork::for_registry(registry_dyn).await_terminal(&record.id),
    )
    .await
    {
        Ok(output) => output.expect("await TypeScript signal process"),
        Err(_) => panic!(
            "TypeScript signal process reaches terminal state: {:?}",
            registry.get_process(&record.id).await
        ),
    };
    assert_eq!(
        terminal,
        lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!({ "ok": true }),
        ))
    );
}

#[tokio::test]
pub(super) async fn typescript_restored_process_handle_await_crosses_turn_boundary() {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
        lash_core::facade_support::NativeRuntimeEffectController::default()
            .allow_process_lifetime_completion_keys(),
    );
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default().with_processes(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("TypeScript cross-turn test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(
            lash_core::facade_support::NativeEffectHost::new(controller.clone())
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
    let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn);
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
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid test native substrate config");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        controller: controller.clone(),
        originator_override: None,
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        Arc::new(EmptyTypeScriptSignalToolProvider),
        lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
        None,
        processes,
        controller,
        process_env_store,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine("typescript");
    let turn_n = execute_code_with_dialect_and_bounds(
        &mut state,
        ctx.clone(),
        ExecRequest {
            language: "typescript".to_string(),
            code: r#"
                    const worker = defineProcess({
                      name: "worker", signals: {},
                      run: async () => { return "done"; }
                    });
                    const handle = start(worker);
                    finish("started");
                "#
            .to_string(),
        },
        artifact_store.clone(),
        surface.clone(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(SourceDialect::Typescript),
    )
    .await;
    assert!(turn_n.error.is_none(), "{:?}", turn_n.error);
    assert_eq!(turn_n.terminal_finish, Some(serde_json::json!("started")));

    let (turn_n_plus_one, _) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            execute_code_with_dialect_and_bounds(
                &mut state,
                ctx,
                ExecRequest {
                    language: "typescript".to_string(),
                    code: "finish(await handle);".to_string(),
                },
                artifact_store,
                surface,
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                RlmSourceContext::cell(SourceDialect::Typescript),
            ),
            worker.drive_pending_processes()
        )
    })
    .await
    .expect("turn N+1 process-handle await must not hang");
    assert!(
        turn_n_plus_one.error.is_none(),
        "{:?}",
        turn_n_plus_one.error
    );
    assert_eq!(
        turn_n_plus_one.terminal_finish,
        Some(serde_json::json!("done"))
    );
}

#[tokio::test]
pub(super) async fn typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation() {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let controller: Arc<dyn lash_core::RuntimeEffectController> = Arc::new(
        lash_core::facade_support::NativeRuntimeEffectController::default()
            .allow_process_lifetime_completion_keys(),
    );
    let inspected = Arc::new(std::sync::Mutex::new(None));
    let tool_provider = Arc::new(TypeScriptProcessInspectionToolProvider {
        inspected_process_id: Arc::clone(&inspected),
    });
    let tool_catalog =
        lash_core::ToolCatalog::from_tool_definitions(vec![status_inspect_definition()]);
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_process_signals(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("TypeScript process handle id test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(
            lash_core::facade_support::NativeEffectHost::new(controller.clone())
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
    let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn);
    let _worker = lash_core::facade_support::DurableProcessWorker::new(
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
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid test native substrate config");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        controller: controller.clone(),
        originator_override: None,
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        tool_provider,
        tool_catalog,
        None,
        processes,
        controller,
        process_env_store,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let response = execute_code_with_dialect_and_bounds(
        &mut RlmExecutionState::for_engine("typescript"),
        ctx,
        ExecRequest {
            language: "typescript".to_string(),
            code: r#"
                    const worker = defineProcess({
                      name: "worker", signals: {},
                      run: async () => { return "done"; }
                    });
                    const handle = start(worker);
                    const processId = handle.id;
                    const status = await status_tool.inspect({ process_id: processId });
                    finish({ id: processId, status: status });
                "#
            .to_string(),
        },
        artifact_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(SourceDialect::Typescript),
    )
    .await;

    assert!(response.error.is_none(), "{:?}", response.error);
    let finish = response.terminal_finish.expect("finish result");
    let finish_id = finish
        .get("id")
        .and_then(|v| v.as_str())
        .expect("id string");
    assert!(!finish_id.is_empty(), "id must not be empty");
    assert_eq!(
        finish.get("status"),
        Some(&serde_json::json!("inspected-ok"))
    );

    let recorded_pid = inspected
        .lock()
        .unwrap()
        .clone()
        .expect("inspected process id");
    assert_eq!(finish_id, recorded_pid);
}
