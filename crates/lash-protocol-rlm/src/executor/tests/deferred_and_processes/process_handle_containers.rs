use super::*;

#[tokio::test]
async fn process_controls_preserve_container_handles_across_suspension_and_snapshot() {
    for (storage, access) in [
        (
            "const handles=[]; for(let i=0;i<1;i=i+1){handles.push(await processes.start({definition:worker}));}",
            "handles[0]",
        ),
        (
            "const handles={child:await processes.start({definition:worker})};",
            "handles.child",
        ),
    ] {
        for suspension in ["", "await sleep(1);"] {
            control_round_trip(storage, access, suspension).await;
        }
    }
}

async fn control_round_trip(storage: &str, access: &str, suspension: &str) {
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_memory_artifact_store().await;
    // The cell runs in a handler on the double, whose process workflow runs
    // the started body: the signal crosses the engine's own delivery route
    // to the waiter the body parks on.
    let table = crate::testing::DoubleProcesses::new(0x4238_0001).await;
    let effect_host = table.backend().effect_host();
    let registry = table.registry();
    let process_env_store = table.env_store();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::all(),
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
            code: format!(
                r#"
                const worker = async () => await waitSignal("ready");
                {storage}
                {suspension}
                await processes.signal({{handle:{access}, name:"ready", payload:{{ok:true}}}});
                finish("signal-sent");
            "#
            ),
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
    let snapshot = state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .expect("capture container state");
    let components = snapshot
        .components
        .into_iter()
        .map(|(key, component)| {
            let lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) = component
            else {
                panic!("a first capture supplies every leaf");
            };
            (key, body)
        })
        .collect();
    let hydrated = lash_core::plugin::HydratedExecutionState {
        root: snapshot.root.expect("snapshot root"),
        components,
    };
    let mut restored = RlmExecutionState::for_engine("typescript");
    restored
        .restore_execution_state(&hydrated, lash_core::FleetFormat::current())
        .expect("restore container state");
    let joined = execute_code_with_test_render(
        &mut restored,
        ctx.clone()
            .with_parent_invocation(lash_core::testing::exec_code_invocation(
                "test-session",
                "test-turn",
                0,
                1,
                "container-join",
                "container-join",
            )),
        ExecRequest {
            code: format!("finish(await processes.await({{handle:{access}}}));"),
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
    assert!(joined.error.is_none(), "{:?}", joined.error);
    assert_eq!(joined.terminal_finish, Some(serde_json::json!({"ok":true})));
    drop(ctx);
    handler.close().await.expect("close the cell's handler");
}

fn journal_handle() -> Value {
    let process_id = lash_sansio::ProcessId::fixture("container-replay-child");
    serde_json::json!({
        "__handle__":"lash",
        "id":lash_sansio::handle::HandleId::process(&process_id).as_str(),
        "process_id":process_id.as_str(),
    })
}

fn journal_definitions() -> Vec<lash_core::ToolDefinition> {
    let handle = serde_json::json!({"x-lash":{"kind":"process_unknown"}});
    vec![
        lash_core::ToolDefinition::raw(
            "tool:container_mint", "container_mint", "Return a process handle for the journal law",
            serde_json::json!({"type":"object"}), handle.clone(),
        ).with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["tools"], "mint")),
        lash_core::ToolDefinition::raw(
            "tool:container_check", "container_check", "Check the journaled process handle",
            serde_json::json!({"type":"object","additionalProperties":false,"properties":{"handle":handle},"required":["handle"]}), handle,
        ).with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["tools"], "check")),
    ]
}

struct JournalHandleProvider {
    mints: Arc<AtomicUsize>,
    checks: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for JournalHandleProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        journal_definitions()
            .iter()
            .map(lash_core::ToolDefinition::manifest)
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        journal_definitions()
            .into_iter()
            .find(|definition| definition.manifest().name == name)
            .map(|definition| Arc::new(definition.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            "container_mint" => {
                self.mints.fetch_add(1, Ordering::SeqCst);
            }
            "container_check" => {
                assert_eq!(call.args["handle"], journal_handle());
                self.checks.fetch_add(1, Ordering::SeqCst);
            }
            other => panic!("unexpected journal-law tool: {other}"),
        }
        lash_core::ToolOutcome::ok(journal_handle()).into()
    }
}

#[test]
fn process_handles_in_containers_replay_without_repeating_tool_effects() {
    block_on(async {
        for (storage, access) in [
            (
                "const handles=[]; handles.push(await tools.mint({}));",
                "handles[0]",
            ),
            (
                "const handles={child:await tools.mint({})};",
                "handles.child",
            ),
        ] {
            let mints = Arc::new(AtomicUsize::new(0));
            let checks = Arc::new(AtomicUsize::new(0));
            let run = CellAttemptRun {
                provider: Arc::new(JournalHandleProvider {
                    mints: Arc::clone(&mints),
                    checks: Arc::clone(&checks),
                }),
                catalog: lash_core::ToolCatalog::from_tool_definitions(journal_definitions()),
                invocation: lash_core::testing::exec_code_invocation(
                    "container-journal-replay",
                    "turn-1",
                    0,
                    0,
                    "container-replay",
                    "exec-code:container-replay",
                ),
                request: ExecRequest {
                    code: format!("{storage} finish(await tools.check({{handle:{access}}}));"),
                },
                resolver: None,
                layer: None,
            };
            let outcomes = run_cell_through_crashes(
                "container-journal-replay",
                "turn-1",
                vec![run.clone()],
                run,
                &[mints, checks],
            )
            .await;
            assert_eq!(outcomes.len(), 2, "a crashed attempt and its redrive");
            for outcome in outcomes {
                assert!(
                    outcome.response.error.is_none(),
                    "{:?}",
                    outcome.response.error
                );
                assert_eq!(outcome.response.terminal_finish, Some(journal_handle()));
                assert_eq!(
                    outcome.counts,
                    [1, 1],
                    "the journal replays the handle without re-running tools"
                );
            }
        }
    });
}
