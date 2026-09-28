use super::*;

const SEED: u64 = 0x5_2c0a;

/// Runs a deferred tool resolution and then fails its journal commit, in
/// front of a controller that journals every other effect.
pub(super) struct FailingDeferredJournalLayer;

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for FailingDeferredJournalLayer {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        if matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation }
                if operation.starts_with("deferred_tool_resolution:v2:")
        ) {
            local_executor.execute(envelope).await?;
            Err(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "injected deferred journal commit failure",
            ))
        } else {
            inner.execute_effect(envelope, local_executor).await
        }
    }
}

/// Where [`DeferredResolutionFaultLayer`] fails a deferred tool resolution.
#[derive(Clone, Copy)]
enum DeferredResolutionFault {
    /// After the resolver returned, before the engine journaled its answer.
    AfterResolverReturn,
    /// After the engine journaled the resolution.
    AfterDurableRecord,
}

/// Fails every deferred tool resolution at `fault`, in front of the
/// handler's controller, which journals every other effect. The law's
/// attempt then dies, and its redrive replays the journal without the layer.
struct DeferredResolutionFaultLayer {
    fault: DeferredResolutionFault,
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for DeferredResolutionFaultLayer {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let is_deferred = matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation }
                if operation.starts_with("deferred_tool_resolution:v2:")
        );
        if !is_deferred {
            return inner.execute_effect(envelope, local_executor).await;
        }
        match self.fault {
            DeferredResolutionFault::AfterResolverReturn => {
                local_executor.execute(envelope).await?;
            }
            DeferredResolutionFault::AfterDurableRecord => {
                inner.execute_effect(envelope, local_executor).await?;
            }
        }
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            match self.fault {
                DeferredResolutionFault::AfterResolverReturn => {
                    "injected crash after resolver return"
                }
                DeferredResolutionFault::AfterDurableRecord => {
                    "injected crash after durable record"
                }
            },
        ))
    }
}

/// One execution of a cell inside a handler attempt on the server double:
/// what the attempt builds its context from and runs.
#[derive(Clone)]
struct CellAttemptRun {
    provider: Arc<dyn lash_core::ToolProvider>,
    catalog: lash_core::ToolCatalog,
    invocation: lash_core::RuntimeInvocation,
    request: ExecRequest,
    resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    layer: Option<Arc<dyn lash_core::testing::EffectLayer>>,
}

/// What one attempt's cell answered, and the law's counters right after it.
#[derive(Debug)]
struct CellAttemptOutcome {
    response: ExecResponse,
    counts: Vec<usize>,
}

/// A handler attempt that runs `run` on the double's `backend` and records
/// its response and `counters` in `outcomes`. A `crash` attempt then dies, as
/// a deployment does, so the server replays the invocation into the next.
fn cell_attempt(
    backend: lash_core::Backend,
    artifacts: lashlang::LashlangArtifacts,
    run: CellAttemptRun,
    crash: bool,
    counters: Vec<Arc<AtomicUsize>>,
    outcomes: Arc<std::sync::Mutex<Vec<CellAttemptOutcome>>>,
) -> lash_restate_test::HandlerAttempt {
    Arc::new(move |scoped| {
        let backend = backend.clone();
        let artifacts = artifacts.clone();
        let run = run.clone();
        let counters = counters.clone();
        let outcomes = Arc::clone(&outcomes);
        Box::pin(async move {
            let ports = match run.layer {
                Some(layer) => crate::testing::attempt_ports_over_layer(&backend, scoped, layer),
                None => crate::testing::attempt_ports(&backend, scoped),
            };
            let ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                ports,
                run.provider,
                run.catalog,
                run.invocation,
            );
            let response = execute_code_unbounded_with_test_render(
                &mut RlmExecutionState::new(),
                ctx,
                run.request,
                artifacts,
                LashlangSurface::default(),
                run.resolver,
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await;
            let counts = counters
                .iter()
                .map(|counter| counter.load(Ordering::SeqCst))
                .collect();
            outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(CellAttemptOutcome { response, counts });
            assert!(!crash, "the attempt's deployment dies after its cell ran");
        })
    })
}

/// Run each of `crashing` in turn, each dying after its cell ran, then
/// `redrive` on the replayed journal, in one turn handler on a fresh double.
/// Returns every attempt's outcome in order, with `counters` read as each
/// cell ended.
async fn run_cell_through_crashes(
    session_id: &str,
    turn_id: &str,
    crashing: Vec<CellAttemptRun>,
    redrive: CellAttemptRun,
    counters: &[Arc<AtomicUsize>],
) -> Vec<CellAttemptOutcome> {
    let double =
        crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let artifacts = crate::testing::memory_artifact_store().await;
    let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let crashing = crashing
        .into_iter()
        .map(|run| {
            cell_attempt(
                backend.clone(),
                artifacts.clone(),
                run,
                true,
                counters.to_vec(),
                Arc::clone(&outcomes),
            )
        })
        .collect();
    let redrive = cell_attempt(
        backend,
        artifacts,
        redrive,
        false,
        counters.to_vec(),
        Arc::clone(&outcomes),
    );
    double
        .run_crashes_then_redriven(
            lash_core::AdmittedScope::turn(
                lash_core::SessionId::from(session_id),
                lash_core::TurnId::from(turn_id),
            ),
            crashing,
            redrive,
        )
        .await
        .expect("the cell's handler crashed and its redrive completed");
    std::mem::take(
        &mut *outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
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

/// The scope [`restricted_empty_deferred_context`] claims for `session_id`.
fn restricted_empty_deferred_scope(session_id: &str) -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_core::SessionId::from(session_id),
        lash_core::TurnId::from("test-turn"),
    )
}

async fn restricted_empty_deferred_context<'h>(
    double: &lash_restate_test::RestateTestBackend,
    handler: &'h lash_restate_test::OpenHandler,
    provider: Arc<dyn lash_core::ToolProvider>,
    session_id: &str,
) -> (
    lash_core::RuntimeExecutionContext<'h>,
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
            crate::testing::double_ports(double, handler),
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
        code: "await web.fetch({});\nawait mystery.x({});".to_string(),
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

        // Both links of turn 1 run in that turn's handler, and turn 2 runs
        // in its own.
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let turn_1 = double
            .open_handler(lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("test-session"),
                lash_core::TurnId::from("turn-1"),
            ))
            .await
            .expect("open turn 1's handler");
        let turn_2 = double
            .open_handler(lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("test-session"),
                lash_core::TurnId::from("turn-2"),
            ))
            .await
            .expect("open turn 2's handler");
        let first_invocation = lash_core::testing::exec_code_invocation(
            "test-session",
            "turn-1",
            1,
            0,
            "effect-1",
            "replay:effect-1",
        );
        let first_ctx = lash_core::testing::code_execution_context_with_invocation(
            crate::testing::double_ports(&double, &turn_1),
            first_invocation,
        );
        assert!(first_ctx.tool_catalog().tools.is_empty());
        let mut state = RlmExecutionState::new();
        let first = execute_code_unbounded_with_test_render(
            &mut state,
            first_ctx.clone(),
            deferred_matrix_request(),
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
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
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .expect("snapshot components"),
        );
        let mut restored = RlmExecutionState::new();
        restored
            .restore_execution_state(&snapshot, lash_core::FleetFormat::current())
            .expect("restore");

        // Same stable link: both positive and negative outcomes survive the
        // snapshot and win without another authorization decision.
        let replay = execute_code_unbounded_with_test_render(
            &mut restored,
            first_ctx.clone(),
            deferred_matrix_request(),
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(replay.error.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "same link must replay");
        assert_eq!(installed.load(Ordering::SeqCst), 2);

        // A second code effect in the same logical turn is a different link
        // and must resolve the same paths against current authority.
        let second_ctx = lash_core::testing::code_execution_context_with_invocation(
            crate::testing::double_ports(&double, &turn_1),
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
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver.clone()),
            RlmProjectedBindings::default(),
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
            crate::testing::double_ports(&double, &turn_2),
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
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
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
        drop((first_ctx, second_ctx, next_turn_ctx));
        turn_1.close().await.expect("close turn 1's handler");
        turn_2.close().await.expect("close turn 2's handler");
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(restricted_empty_deferred_scope(
                "restricted-empty-lashlang-deferred",
            ))
            .await
            .expect("open the cell's handler");
        let (ctx, registry) = restricted_empty_deferred_context(
            &double,
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
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
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
        drop(ctx);
        handler.close().await.expect("close the cell's handler");
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("deferred-journal-failure"),
                lash_core::TurnId::from("turn-1"),
            ))
            .await
            .expect("open the cell's handler");
        let ctx =
            lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                crate::testing::double_ports_over_layer(
                    &double,
                    &handler,
                    Arc::new(FailingDeferredJournalLayer),
                ),
                Arc::clone(&provider),
                lash_core::ToolCatalog::default(),
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

        let response = execute_code_unbounded_with_test_render(
            &mut RlmExecutionState::new(),
            ctx,
            ExecRequest {
                code: r#"finish(await web.fetch({ url: "https://example.test" }));"#.into(),
            },
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        handler.close().await.expect("close the cell's handler");

        let error = response.error.expect("journal failure must abort linking");
        assert!(
            error
                .message
                .contains("injected deferred journal commit failure"),
            "the journal commit failure must not be shadowed: {error:?}"
        );
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    });
}

/// A deferred resolution whose attempt dies at `fault` and is redriven: the
/// resolver runs again on the replay only if its answer never reached the
/// journal.
async fn run_deferred_fault_boundary(
    fault: DeferredResolutionFault,
    expected_resolver_calls_after_replay: usize,
) {
    let session_id = match fault {
        DeferredResolutionFault::AfterResolverReturn => "deferred-after-resolver",
        DeferredResolutionFault::AfterDurableRecord => "deferred-after-record",
    };
    let turn_id = "turn-1";
    let replay_key = "exec-code:fault";
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
    let run = CellAttemptRun {
        provider,
        catalog: lash_core::ToolCatalog::default(),
        invocation: lash_core::testing::exec_code_invocation(
            session_id,
            turn_id,
            0,
            0,
            "faulting exec",
            replay_key,
        ),
        request: ExecRequest {
            code: r#"finish(await web.fetch({ url: "https://example.test" }));"#.into(),
        },
        resolver: Some(resolver),
        layer: None,
    };
    let first = CellAttemptRun {
        layer: Some(Arc::new(DeferredResolutionFaultLayer { fault })),
        ..run.clone()
    };
    let outcomes = run_cell_through_crashes(
        session_id,
        turn_id,
        vec![first],
        run,
        &[resolver_calls, installs, executions],
    )
    .await;
    let [first, replay] = outcomes.as_slice() else {
        panic!("one crashed attempt and one redrive ran: {outcomes:?}");
    };
    assert!(
        first.response.error.is_some(),
        "boundary fault must stop execution"
    );
    assert_eq!(
        first.counts,
        [1, 0, 0],
        "a failed resolution boundary must prevent the dependent tool effect"
    );
    assert!(
        replay.response.error.is_none(),
        "{:?}",
        replay.response.error
    );
    assert_eq!(
        replay.counts,
        [expected_resolver_calls_after_replay, 1, 1],
        "resolver calls, installs and executions after the replay"
    );
}

#[test]
pub(super) fn fault_after_resolver_return_repeats_discovery_on_the_replay() {
    block_on(run_deferred_fault_boundary(
        DeferredResolutionFault::AfterResolverReturn,
        2,
    ));
}

#[test]
pub(super) fn fault_after_durable_record_never_reresolves_on_the_replay() {
    block_on(run_deferred_fault_boundary(
        DeferredResolutionFault::AfterDurableRecord,
        1,
    ));
}

#[test]
pub(super) fn fault_before_registration_reinstalls_recorded_route_on_the_replay() {
    block_on(async {
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
        let run = CellAttemptRun {
            provider,
            catalog: lash_core::ToolCatalog::default(),
            invocation: lash_core::testing::exec_code_invocation(
                "deferred-before-registration",
                "turn-1",
                0,
                0,
                "registration exec",
                "exec-code:registration",
            ),
            request: ExecRequest {
                code: r#"finish(await web.fetch({ url: "https://example.test" }));"#.into(),
            },
            resolver: Some(resolver),
            layer: None,
        };
        let outcomes = run_cell_through_crashes(
            "deferred-before-registration",
            "turn-1",
            vec![run.clone()],
            run,
            &[resolver_calls, installs, executions],
        )
        .await;
        let [first, replay] = outcomes.as_slice() else {
            panic!("one crashed attempt and one redrive ran: {outcomes:?}");
        };
        assert!(first.response.error.is_some());
        assert_eq!(first.counts, [1, 1, 0]);
        assert!(
            replay.response.error.is_none(),
            "{:?}",
            replay.response.error
        );
        // The journaled resolution is never resolved again; the replay
        // reinstalls its recorded route and runs the tool once.
        assert_eq!(replay.counts, [1, 2, 1]);
    });
}

#[test]
pub(super) fn replay_serves_an_ambient_failure_as_ambient() {
    block_on(async {
        let session_id = "deferred-ambient-failure";
        let turn_id = "turn-1";
        let invocation = lash_core::testing::exec_code_invocation(
            session_id,
            turn_id,
            0,
            0,
            "ambient failure",
            "exec-code:ambient-failure",
        );
        let program = lash_typescript::parse(r#"await web.fetch({});"#).expect("parse");
        let collision = lash_core::ToolCatalog::from_tool_definitions(vec![
            ambient_definition("tool:ambient_a", "ambient_a", "web", "fetch"),
            ambient_definition("tool:ambient_b", "ambient_b", "web", "fetch"),
        ]);
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Default::default(),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            });
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        // The first attempt classifies the live collision; the redrive meets
        // an empty catalog, so only the journal can say `Ambient`.
        let attempt = |catalog: lash_core::ToolCatalog,
                       crash: bool|
         -> lash_restate_test::HandlerAttempt {
            let backend = backend.clone();
            let provider = Arc::clone(&provider);
            let invocation = invocation.clone();
            let program = program.clone();
            let messages = Arc::clone(&messages);
            Arc::new(move |scoped| {
                let ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                    crate::testing::attempt_ports(&backend, scoped),
                    Arc::clone(&provider),
                    catalog.clone(),
                    invocation.clone(),
                );
                let program = program.clone();
                let catalog = catalog.clone();
                let messages = Arc::clone(&messages);
                Box::pin(async move {
                    let mut record = lash_lashlang_runtime::DeferredResolutionRecord::default();
                    record.select_link(
                        lash_lashlang_runtime::DeferredResolutionLinkKey::from_exec_code_invocation(
                            ctx.parent_invocation().expect("parent invocation"),
                        )
                        .expect("effect invocation has a link identity"),
                    );
                    let failure = lash_lashlang_runtime::resolve_and_build_deferred_environment(
                        &program,
                        &LashlangSurface::default(),
                        &catalog,
                        None,
                        &mut record,
                        &ctx,
                    )
                    .await
                    .expect_err("the ambient failure is classified");
                    let message = match failure {
                        lash_lashlang_runtime::DeferredResolutionError::Ambient(error) => {
                            error.to_string()
                        }
                        other => panic!("the failure was not Ambient: {other:?}"),
                    };
                    messages
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(message);
                    assert!(!crash, "the attempt's deployment dies after it classified");
                })
            })
        };
        double
            .run_crashed_then_redriven(
                lash_core::AdmittedScope::turn(
                    lash_core::SessionId::from(session_id),
                    lash_core::TurnId::from(turn_id),
                ),
                attempt(collision, true),
                attempt(lash_core::ToolCatalog::default(), false),
            )
            .await
            .expect("the classification crashed and its redrive completed");
        let messages = messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let [live, replayed] = messages.as_slice() else {
            panic!("one crashed attempt and one redrive classified: {messages:?}");
        };
        assert_eq!(replayed, live, "the replay preserves the ambient failure");
    });
}

#[test]
pub(super) fn replay_serves_a_positive_before_an_ambient_collision_from_the_journal() {
    block_on(async {
        let session_id = "deferred-positive";
        let turn_id = "turn-1";
        let replay_key = "exec-code:positive";
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
            code: r#"
                if (false) {
                    const ignored = await web.fetch({ url: "https://example.test" });
                }
                finish("journaled-positive");
            "#
            .into(),
        };
        let first = CellAttemptRun {
            provider: Arc::clone(&provider),
            catalog: lash_core::ToolCatalog::default(),
            invocation: lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                0,
                0,
                "original descriptive label",
                replay_key,
            ),
            request: request.clone(),
            resolver: Some(resolver),
            layer: None,
        };
        // A replay whose catalog collides on a path the cell never resolved:
        // the journaled resolution replays first, and the collision is then
        // an ordinary host failure.
        let unrelated_collision = CellAttemptRun {
            catalog: lash_core::ToolCatalog::from_tool_definitions(vec![
                ambient_definition("tool:other_a", "other_a", "other", "run"),
                ambient_definition("tool:other_b", "other_b", "other", "run"),
            ]),
            invocation: lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                98,
                42,
                "another descriptive label",
                replay_key,
            ),
            resolver: None,
            ..first.clone()
        };
        // The replay under an empty state projection, changed parent
        // attribution and a changed descriptive label. Two new ambient
        // claimants of the recorded path would collide if catalog
        // construction ran before the recorded path was masked, and the
        // resolver answers again if the journal did not.
        let replay = CellAttemptRun {
            catalog: lash_core::ToolCatalog::from_tool_definitions(vec![
                ambient_definition("tool:ambient_a", "ambient_a", "web", "fetch"),
                ambient_definition("tool:ambient_b", "ambient_b", "web", "fetch"),
            ]),
            invocation: lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                97,
                41,
                "renamed descriptive label",
                replay_key,
            ),
            ..first.clone()
        };
        let outcomes = run_cell_through_crashes(
            session_id,
            turn_id,
            vec![first, unrelated_collision],
            replay,
            &[Arc::clone(&resolver_calls), Arc::clone(&installed)],
        )
        .await;
        let [first, unrelated_collision, replay] = outcomes.as_slice() else {
            panic!("two crashed attempts and one redrive ran: {outcomes:?}");
        };
        assert!(first.response.error.is_none(), "{:?}", first.response.error);
        assert_eq!(first.counts, [1, 1]);
        let error = unrelated_collision
            .response
            .error
            .as_ref()
            .expect("unrelated ambient collision remains an ordinary host failure");
        assert!(
            !error
                .message
                .contains("failed to commit deferred resolution"),
            "the deferred journal replay succeeded before the unrelated collision: {error:?}"
        );
        assert!(
            replay.response.error.is_none(),
            "{:?}",
            replay.response.error
        );
        assert_eq!(
            replay.response.terminal_finish,
            Some(serde_json::json!("journaled-positive"))
        );
        assert_eq!(
            replay.counts,
            [1, 2],
            "the replay reinstalls the journaled grant and never resolves again"
        );
    });
}

#[test]
pub(super) fn replay_serves_a_negative_before_a_changed_ambient_without_resolver() {
    block_on(async {
        let session_id = "deferred-negative";
        let turn_id = "turn-1";
        let replay_key = "exec-code:negative";
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
            Arc::new(CountingDeferredResolver {
                calls: Arc::clone(&resolver_calls),
                batches: Default::default(),
                installed: Default::default(),
            });
        let provider: Arc<dyn lash_core::ToolProvider> =
            Arc::new(BindingRecordingDeferredProvider {
                executions: Default::default(),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            });
        let first = CellAttemptRun {
            provider,
            catalog: lash_core::ToolCatalog::default(),
            invocation: lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                0,
                0,
                "negative original",
                replay_key,
            ),
            request: deferred_matrix_request(),
            resolver: Some(resolver),
            layer: None,
        };
        // `mystery.x` is live on the replay, as an ambient tool that
        // appeared between the two attempts.
        let replay = CellAttemptRun {
            catalog: lash_core::ToolCatalog::from_tool_definitions(vec![ambient_definition(
                "tool:ambient_mystery",
                "ambient_mystery",
                "mystery",
                "x",
            )]),
            invocation: lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                12,
                33,
                "negative renamed",
                replay_key,
            ),
            resolver: None,
            ..first.clone()
        };
        let outcomes = run_cell_through_crashes(
            session_id,
            turn_id,
            vec![first],
            replay,
            &[Arc::clone(&resolver_calls)],
        )
        .await;
        let [first, replay] = outcomes.as_slice() else {
            panic!("one crashed attempt and one redrive ran: {outcomes:?}");
        };
        let first_error = first
            .response
            .error
            .as_ref()
            .expect("mystery.x is durably unavailable");
        assert_eq!(first.counts, [1]);
        let error = replay
            .response
            .error
            .as_ref()
            .expect("journaled negative must mask the newly live ambient tool");
        // The journaled negative masks the now-live ambient `mystery.x`: the
        // replay fails exactly as the original did, rather than resolving the
        // tool that appeared between the two runs.
        assert!(error.message.contains("mystery"), "{error:?}");
        assert_eq!(error.message, first_error.message);
        assert!(
            !error
                .message
                .contains("failed to commit deferred resolution")
        );
        assert_eq!(replay.counts, [1]);
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(restricted_empty_deferred_scope(
                "restricted-empty-typescript-deferred",
            ))
            .await
            .expect("open the cell's handler");
        let (ctx, registry) = restricted_empty_deferred_context(
            &double,
            &handler,
            provider,
            "restricted-empty-typescript-deferred",
        )
        .await;
        let enumerations_after_catalog = enumerations.load(Ordering::SeqCst);

        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_test_render(
                &mut state,
                ctx.clone(),
                ExecRequest {
                    code: "const result = await web.fetch({ url: 'https://example.test' }); finish(result);".to_string(),
                },
                crate::testing::memory_artifact_store().await,
                LashlangSurface::default(),
                Some(resolver),
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
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
        drop(ctx);
        handler.close().await.expect("close the cell's handler");
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("runtime-output-retention"),
                lash_core::TurnId::from("turn-1"),
            ))
            .await
            .expect("open the cell's handler");
        let ctx =
            lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                crate::testing::double_ports(&double, &handler),
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
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            Some(resolver),
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        handler.close().await.expect("close the cell's handler");

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

/// The executor names exactly the process handles the runtime would await.
///
/// Before ADR 0095 this asserted the opposite: a bare `handle` key, an empty
/// id and a *tool* handle were all counted as process handles, because
/// `is_process_handle` only looked for the presence of a key. That looseness is
/// the defect the one handle kind removes — the executor and the VM now ask the
/// same parse, so a record that names no process is not a process handle here
/// either.
#[test]
pub(super) fn process_handle_derivation_matches_runtime_await_authority() {
    let canonical_id = format!("p.{}", lash_core::ProcessId::fixture("p1"));
    let globals = lashlang::from_json(serde_json::json!({
        "canonical": { "__handle__": "lash", "id": canonical_id },
        // The incarnation-qualified spelling the minted id retired.
        "retired_incarnation_handle": { "__handle__": "lash", "id": "p.1.p1" },
        // The record part 1 still minted: the incarnation beside the id, and a
        // kind of its own. Part 2 retires both.
        "retired_process_record": { "__handle__": "process", "id": "p1", "incarnation": 1 },
        "no_incarnation": { "__handle__": "process", "id": "p1" },
        "alternate": { "handle": "p2" },
        "empty_id": { "__handle__": "process", "id": "", "incarnation": 1 },
        "other_kind": { "__handle__": "tool", "id": "t1" },
        "tool_handle": { "__handle__": "lash", "id": "t.0000000000000000.0" },
        "plain_record": { "id": "not-a-handle" },
        "scalar": 1,
    }));
    let globals = globals.as_record().expect("fixture globals are a record");

    assert_eq!(
        process_handle_names(globals),
        BTreeSet::from(["canonical".to_string()]),
        "only a well-formed process handle is a process handle"
    );
}

#[test]
pub(super) fn execute_code_stores_process_module_artifact_once() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let request = || ExecRequest {
            code: r#"const later = async () => { return 1; };
            finish(1);"#
                .to_string(),
        };
        let surface = || {
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            )
        };

        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let first = execute_code_unbounded_with_test_render(
            &mut state,
            lash_core::testing::code_execution_context(crate::testing::double_ports(
                &double, &handler,
            )),
            request(),
            crate::testing::memory_artifact_store().await,
            surface(),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        handler.close().await.expect("close the cell's handler");
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(state.stored_lashlang_modules.len(), 1);

        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let second = execute_code_unbounded_with_test_render(
            &mut state,
            lash_core::testing::code_execution_context(crate::testing::double_ports(
                &double, &handler,
            )),
            request(),
            crate::testing::memory_artifact_store().await,
            surface(),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        handler.close().await.expect("close the cell's handler");
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
        let artifact_store = crate::testing::fresh_memory_artifact_store().await;
        let mut state = RlmExecutionState::for_engine("typescript");
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let response = execute_code_with_test_render(
            &mut state,
            lash_core::testing::code_execution_context(crate::testing::double_ports(
                &double, &handler,
            )),
            ExecRequest {
                code: r#"
                        const worker = async (input: unknown) => { return input; };
                        finish(1);
                    "#
                .to_string(),
            },
            artifact_store.clone(),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        handler.close().await.expect("close the cell's handler");
        assert!(response.error.is_none(), "{:?}", response.error);
        let module_ref = state
            .stored_lashlang_modules
            .iter()
            .next()
            .expect("stored process module");
        let artifact =
            lashlang::LashlangArtifacts::get_module_artifact(&artifact_store, module_ref)
                .await
                .expect("read stored artifact")
                .expect("artifact exists");
        // TypeScript is the only language a module can be compiled from
        // (ADR 0096), so a stored artifact names no dialect at all; one that
        // still does is refused as an incompatible format.
        let encoded = serde_json::to_value(artifact.as_ref()).expect("encode stored artifact");
        assert!(
            encoded.get("compilation_dialect").is_none(),
            "a stored artifact names no dialect: {encoded}"
        );
    });
}

#[derive(Clone)]
pub(super) struct TypeScriptSignalProcessService {
    pub(super) registry: Arc<dyn lash_core::ProcessRegistry>,
    pub(super) effect_host: Arc<dyn lash_core::EffectHost>,
    pub(super) originator_override: Option<lash_core::ProcessOriginator>,
    /// Where a recorded start publishes the execution env its registration
    /// then references. FIG-2999: `processes.start` is a declaring leaf tool,
    /// so the env is captured on the recorded-intent route rather than by the
    /// host-bridge arm the executor no longer has.
    pub(super) env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    /// The engine registry a recorded start is admitted against. FIG-2999: the
    /// signal event types a process registers with come from the engine
    /// resolving the definition the start named, which used to be computed by
    /// the in-attempt start path the executor no longer has. Without the
    /// admission a signalled process refuses its own signal at delivery
    /// ("emitted undeclared event type `signal.ready`").
    pub(super) engines: Arc<lash_core::ProcessEngineRegistry>,
}

/// The surface a process engine runs a child against.
///
/// FIG-2999: the cell's module requires the `processes` module the moment it
/// starts a process, and a child replaying that module resolves the requirement
/// against the engine's own surface. The cell reads those operations off its
/// tool catalogue; the engine, which has no catalogue, carries them as host
/// resources instead.
pub(super) fn process_engine_surface(surface: LashlangSurface) -> LashlangSurface {
    surface
        .with_resources(
            lash_lashlang_runtime::lashlang_resources_from_tool_catalog(
                &process_control_tool_catalog(),
            )
            .expect("process control tools bind"),
        )
        .expect("process control operations are unique")
}

/// The engine registry a fixture process service admits recorded starts
/// against: the one stock engine, over the artifacts the test publishes.
pub(super) fn fixture_process_engines(
    artifact_store: lashlang::LashlangArtifacts,
    surface: LashlangSurface,
) -> Arc<lash_core::ProcessEngineRegistry> {
    Arc::new(lash_core::ProcessEngineRegistry::new().with_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store,
                process_engine_surface(surface),
            ),
        ),
    ))
}

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
    // FIG-2999: the fixture's cell starts a process before it inspects one, and
    // starting is a leaf tool, so this provider serves the process controls
    // beside its own inspection tool.
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        let mut manifests = vec![status_inspect_definition().manifest()];
        manifests.extend(ProcessControlToolProvider.tool_manifests());
        manifests
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        if name == "status_inspect" || name == "tool:status_inspect" {
            return Some(Arc::new(status_inspect_definition().contract()));
        }
        ProcessControlToolProvider.resolve_contract(name)
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        ProcessControlToolProvider.attempt_may_defer(tool_id)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "status_inspect" || call.name() == "tool:status_inspect" {
            *self.inspected_process_id.lock().unwrap() = call
                .args
                .get("process_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            return lash_core::ToolAttemptOutcome::done_without_intents(
                lash_core::ToolOutcomeDone::ok(serde_json::json!("inspected-ok")),
            );
        }
        ProcessControlToolProvider.execute(call).await
    }
}

/// The process-control leaf tools a cell reaches for, backed by the shipped
/// plugin.
///
/// FIG-2999: starting, signalling and yielding are leaf tools rather than
/// dialect special forms, so a fixture that drives a process installs the same
/// declarations and the same attempt bodies `lash-plugin-process-controls`
/// ships, instead of a host-bridge arm the executor no longer has.
pub(super) struct ProcessControlToolProvider;

pub(super) fn process_control_tool_definitions() -> Vec<lash_core::ToolDefinition> {
    vec![
        lash_plugin_process_controls::process_start_tool_definition(),
        lash_plugin_process_controls::process_signal_tool_definition(),
        lash_plugin_process_controls::process_emit_tool_definition(),
        lash_plugin_process_controls::process_register_tool_definition(),
        lash_plugin_process_controls::process_await_tool_definition(),
        lash_plugin_process_controls::process_cancel_tool_definition(),
    ]
}

pub(super) fn process_control_tool_catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(process_control_tool_definitions())
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ProcessControlToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        process_control_tool_definitions()
            .iter()
            .map(lash_core::ToolDefinition::manifest)
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        process_control_tool_definitions()
            .into_iter()
            .find(|definition| definition.manifest().name == name)
            .map(|definition| Arc::new(definition.contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id.as_str() == "tool:await_process"
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            "start_process" => {
                lash_plugin_process_controls::execute_process_start_tool_call(
                    call.context,
                    call.args,
                    &(Arc::new(lash_core::lifetime::detached) as lash_core::LifetimePolicy),
                )
                .await
            }
            "signal_process" => lash_plugin_process_controls::execute_process_signal_tool_call(
                call.context,
                call.args,
            ),
            "emit_process_event" => lash_plugin_process_controls::execute_process_emit_tool_call(
                call.context,
                call.args,
            ),
            "register_process" => lash_plugin_process_controls::execute_process_register_tool_call(
                call.context,
                call.args,
            ),
            "await_process" => lash_plugin_process_controls::execute_process_await_tool_call(
                call.context,
                call.args,
            ),
            other => {
                let lash_core::ToolOutcome::Done(output) = lash_core::ToolOutcome::err(
                    serde_json::json!(format!("unknown process control tool `{other}`")),
                ) else {
                    unreachable!("an error outcome is always done")
                };
                lash_core::ToolAttemptOutcome::done_without_intents(
                    lash_core::ToolOutcomeDone::from_output(*output),
                )
            }
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessService for TypeScriptSignalProcessService {
    /// FIG-2999: `processes.start` is a declaring leaf tool, so a cell that
    /// starts a process reaches the registry through the recorded-intent route
    /// rather than through the host-bridge arm the executor no longer has.
    /// This fixture registers the child the same way its direct `start` does.
    async fn start_from_recorded_intent(
        &self,
        session_id: &SessionId,
        request: lash_core::ProcessStartRequest,
        scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessHandleView, lash_core::PluginError> {
        // The starting session observes the child it started, the way the
        // runtime's own start command records it.
        let mut observers = request.observers.clone();
        if !observers.contains(session_id) {
            observers.push(session_id.clone());
        }
        let request_env_spec = request.env_spec.clone();
        let env_ref = match request.env_spec.clone() {
            Some(spec) => Some(
                lash_core::testing::publish_process_execution_env_for_testing(
                    self.env_store.as_ref(),
                    &lash_core::ArtifactOwner::process_start(
                        &lash_core::ProcessCommand::start_effect_id(request.start_key.as_ref()),
                    ),
                    &spec,
                )
                .await?,
            ),
            None => None,
        };
        let registration = request.into_registration(env_ref);
        // The runtime's recorded-intent route admits an engine start against
        // the env its own record carries and stamps the identity the engine
        // resolved, which is where the process's signal event types come from.
        let registration = match registration.input.as_ref() {
            lash_core::ProcessInput::Engine { kind, payload } => {
                let admitted = self
                    .engines
                    .admit(kind, payload, request_env_spec.as_ref())
                    .await?;
                registration.with_admitted_identity(admitted)
            }
            _ => registration,
        };
        let record = self
            .start(
                session_id,
                registration,
                lash_core::ProcessStartOptions::new().with_initial_observers(observers),
                scope,
            )
            .await?;
        Ok(lash_core::ProcessHandleView::from_record(record))
    }

    // The remaining recorded-intent routes belong to atomic tool attempts this
    // signal fixture never opens. Refuse them rather than pretend, so a test
    // that starts using them fails loudly instead of silently taking a
    // non-atomic path.

    async fn cancel_recorded_intent(
        &self,
        _session_id: &SessionId,
        _process_id: &ProcessId,
        _identity: lash_core::ToolIntentIdentity,
        _scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
        Err(lash_core::PluginError::Session(
            "recorded process cancellation is unavailable in this test".to_string(),
        ))
    }

    /// FIG-2999: `processes.signal` is a declaring leaf tool, so a cell that
    /// signals reaches the process through the recorded-intent route. The
    /// delivery is the same one the possessed route performs — the fixture's
    /// waiter-side assertion included — so it delegates rather than growing a
    /// second copy that could drift from it.
    async fn signal_recorded_intent(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        signal: String,
        call_id: String,
        payload: serde_json::Value,
        scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessEvent, lash_core::PluginError> {
        self.signal_possessed(session_id, process_id, signal, call_id, payload, scope)
            .await
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
        scope: lash_core::ProcessOpScope<'_>,
    ) -> Result<lash_core::ProcessRecord, lash_core::PluginError> {
        // A runtime start records the start context it was admitted under,
        // as the runtime's own realization does (FIG-3607 R1, R2).
        if let Some(cx) = scope
            .start_cx()
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?
        {
            registration = registration.with_start_cx(&cx);
        }
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
            // This fixture's override emulates a host-admin start, which is a root.
            registration.ancestry = lash_core::Ancestry::root();
            registration.session_capability = None;
        }
        registration = registration
            .with_process_provenance(lash_core::ProcessProvenance::new(originator))
            .with_wake_session_id(wake_session_id);
        // This fixture's `start` registers directly, so it performs the
        // journaled effect's env publish itself: a spec-carrying start is
        // staged under its start-scoped owner and stamped with the reference
        // the publish produced.
        if registration.env_ref.is_none()
            && let Some(spec) = options.env_spec.as_ref()
        {
            let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
                self.env_store.as_ref(),
                &lash_core::ArtifactOwner::process_start(
                    &lash_core::ProcessCommand::start_effect_id(registration.start_key.as_ref()),
                ),
                spec,
            )
            .await?;
            registration = registration.with_execution_env_ref(Some(env_ref));
        }
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
        lash_core::NoProcessWork::for_registry(registry)
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
        let event = lash_core::testing::effect_backed_process_service(
            self.registry.clone(),
            Arc::clone(&self.env_store),
        )
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
            .effect_host
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
            .effect_host
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
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_memory_artifact_store().await;
    // The cell runs in a handler on the double, whose process workflow runs
    // the started body: the signal crosses the engine's own delivery route
    // to the waiter the body parks on.
    let table = crate::testing::DoubleProcesses::new(0x7519_0001).await;
    let effect_host = table.backend().effect_host();
    let registry = table.registry();
    let process_env_store = table.env_store();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
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
        table.backend().clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                process_engine_surface(surface.clone()),
            ),
        ),
    );
    table.install_worker(
        lash_core::testing::test_code_protocol_factories(),
        runtime_host,
        session_policy.clone(),
    );
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone()),
    });
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(table.double(), &handler),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_test_render(
        &mut state,
        ctx.clone(),
        ExecRequest {
            code: r#"
                    const worker = async () => await waitSignal("ready");
                    const handle = await processes.start({ definition: worker });
                    await processes.signal({ handle: handle, name: "ready", payload: { ok: true } });
                    finish("signal-sent");
                "#
            .to_string(),
        },
        artifact_store.clone(),
        surface.clone(),
        None,
        RlmProjectedBindings::default(),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!("signal-sent"))
    );
    drop(ctx);
    handler.close().await.expect("close the cell's handler");

    table.admit_pending().await;
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
    assert_eq!(record.lifetime, lash_core::LifetimeDecision::Detached);
    assert_eq!(
        record.ancestry.starter(),
        Some(&lash_core::ScopeId::turn(
            SessionId::from("test-session"),
            lash_core::TurnId::from("test-turn"),
        )),
        "the start records the turn that started it"
    );
    let terminal = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        table.await_terminal(&record.id),
    )
    .await
    {
        Ok(output) => output,
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
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_memory_artifact_store().await;
    // Both cells run in the double's handler; its process workflow runs the body.
    let table = crate::testing::DoubleProcesses::new(0x7519_0002).await;
    let effect_host = table.backend().effect_host();
    let registry = table.registry();
    let process_env_store = table.env_store();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
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
        table.backend().clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                process_engine_surface(surface.clone()),
            ),
        ),
    );
    table.install_worker(
        lash_core::testing::test_code_protocol_factories(),
        runtime_host,
        session_policy.clone(),
    );
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone()),
    });
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(table.double(), &handler),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine("typescript");
    let turn_n = execute_code_with_test_render(
        &mut state,
        ctx.clone(),
        ExecRequest {
            code: r#"
                    const worker = async () => { return "done"; };
                    const handle = await processes.start({ definition: worker });
                    finish("started");
                "#
            .to_string(),
        },
        artifact_store.clone(),
        surface.clone(),
        None,
        RlmProjectedBindings::default(),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert!(turn_n.error.is_none(), "{:?}", turn_n.error);
    assert_eq!(turn_n.terminal_finish, Some(serde_json::json!("started")));

    let (turn_n_plus_one, _) = Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            tokio::join!(
                execute_code_with_test_render(
                    &mut state,
                    // Turn N+1's cell is its own code-execution effect: its
                    // nested effects are keyed under its own replay key.
                    ctx.with_parent_invocation(lash_core::testing::exec_code_invocation(
                        "test-session",
                        "test-turn",
                        0,
                        1,
                        "exec-code-turn-n-plus-one",
                        "test-session:test-turn:0:1:exec_code:1",
                    )),
                    ExecRequest {
                        code: "finish(await handle);".to_string(),
                    },
                    artifact_store,
                    surface,
                    None,
                    RlmProjectedBindings::default(),
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                    crate::plugin::RlmChannel::Cell,
                ),
                table.admit_pending()
            )
        },
    ))
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
    handler.close().await.expect("close the cell handler");
}

#[tokio::test]
pub(super) async fn typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation() {
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_memory_artifact_store().await;
    let table = crate::testing::DoubleProcesses::new(0x7519_0003).await;
    let registry = table.registry();
    let process_env_store = table.env_store();
    let effect_host = table.backend().effect_host();
    let inspected = Arc::new(std::sync::Mutex::new(None));
    let tool_provider = Arc::new(TypeScriptProcessInspectionToolProvider {
        inspected_process_id: Arc::clone(&inspected),
    });
    let mut catalog_definitions = vec![status_inspect_definition()];
    catalog_definitions.extend(process_control_tool_definitions());
    let tool_catalog = lash_core::ToolCatalog::from_tool_definitions(catalog_definitions);
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
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
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone()),
    });
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(table.double(), &handler),
        tool_provider,
        tool_catalog,
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let response = execute_code_with_test_render(
        &mut RlmExecutionState::for_engine("typescript"),
        ctx,
        ExecRequest {
            code: r#"
                    const worker = async () => { return "done"; };
                    const handle = await processes.start({ definition: worker });
                    const processId = handle.process_id;
                    const status = await status_tool.inspect({ process_id: processId });
                    finish({ id: processId, status: status });
                "#
            .to_string(),
        },
        artifact_store,
        surface,
        None,
        RlmProjectedBindings::default(),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
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
    handler.close().await.expect("close the cell handler");
}
