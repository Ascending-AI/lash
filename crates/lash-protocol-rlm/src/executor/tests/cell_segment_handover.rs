//! FIG-4739: a code cell parked on a durable wait hands the wait to its
//! Run's successor segment, and the segment that resumes the cell holds
//! everything the cell held.
//!
//! The capture law: a cell stopped at a segment boundary inside it, carried
//! through the session's durable execution state into a fresh state, a fresh
//! handler and a fresh journal, ends exactly as the same cell ends when
//! nothing stops it, and no effect it had already dispatched is dispatched
//! again.

use super::*;
use std::sync::atomic::AtomicBool;

const CELL: &str = r#"
    const worker = async () => { return "done"; };
    let before = 20;
    print("started");
    const handle = await processes.start({ definition: worker });
    const answer = await handle;
    before = before + 22;
    print(answer + " after " + before);
    finish({ answer, before });
"#;

struct Fixture {
    table: crate::testing::DoubleProcesses,
    workers: lash_vm_client::service::Service,
    artifact_store: lashlang::LashlangArtifacts,
    surface: LashlangSurface,
    processes: Arc<dyn lash_core::ProcessService>,
    hand_over: Arc<AtomicBool>,
    session_policy: lash_core::SessionPolicy,
}

impl Fixture {
    async fn new(seed: u64) -> Self {
        let artifact_store: lashlang::LashlangArtifacts =
            crate::testing::fresh_sqlite_memory_artifact_store().await;
        let table = crate::testing::DoubleProcesses::new(seed).await;
        let workers = lash_vm_client::service::Service::default()
            .with_recovery_store(table.backend().worker_recovery());
        let surface = LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        );
        let session_policy = lash_core::SessionPolicy {
            model: Some(lash_core::testing::test_llm_profile_config(
                "mock-model",
                lash_core::LlmProfileMetadata::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("cell handover test model"),
            )),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
        };
        let engine = || {
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                process_engine_surface(surface.clone()),
                table.backend().worker_recovery(),
            )
            .with_worker_service(workers.clone())
        };
        let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
            table.backend().clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        )
        .with_process_engine_registration(
            lash_lashlang_runtime::lashlang_process_engine_registration(engine()),
        );
        table.install_worker(
            lash_core::testing::test_code_protocol_factories(),
            runtime_host,
        );
        let hand_over = Arc::new(AtomicBool::new(false));
        let processes: Arc<dyn lash_core::ProcessService> =
            Arc::new(TypeScriptSignalProcessService {
                hand_over_awaits: Some(Arc::clone(&hand_over)),
                registry: table.registry(),
                effect_host: table.backend().effect_host(),
                originator_override: None,
                env_store: table.env_store(),
                engines: Arc::new(
                    lash_core::ProcessEngineRegistry::new()
                        .with_artifact_ports(lash_core::ArtifactReferrerPorts::of_backend(
                            table.backend(),
                        ))
                        .with_registration(
                            lash_lashlang_runtime::lashlang_process_engine_registration(engine()),
                        ),
                ),
            });
        Self {
            table,
            workers,
            artifact_store,
            surface,
            processes,
            hand_over,
            session_policy,
        }
    }

    /// Runs `CELL` as the cell of `turn`, in a handler and a journal of its
    /// own, admitting the processes it starts while it runs.
    async fn run_cell(&self, state: &mut RlmExecutionState, turn: &str) -> ExecResponse {
        let handler = self
            .table
            .open_handler(lash_core::AdmittedScope::turn(
                lash_core::SessionId::from("test-session"),
                lash_core::TurnId::fixture(turn),
            ))
            .await;
        let ctx = lash_core::testing::TestExecutionContextBuilder::new(
            crate::testing::double_ports(self.table.double(), &handler),
        )
        .provider(Arc::new(ProcessControlToolProvider))
        .tool_catalog(process_control_tool_catalog())
        .processes(Arc::clone(&self.processes))
        .execution_env_spec(lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            self.session_policy.clone(),
        ))
        .runtime_parent_invocation(lash_core::testing::exec_code_invocation(
            "test-session",
            lash_core::TurnId::fixture(turn),
            0,
            0,
            "cell",
            "cell",
        ))
        .build()
        .into_runtime();
        let (response, ()) = Box::pin(tokio::time::timeout(
            std::time::Duration::from_secs(60),
            async {
                tokio::join!(
                    execute_code_with_test_render(
                        state,
                        ctx,
                        ExecRequest {
                            code: CELL.to_string(),
                        },
                        self.artifact_store.clone(),
                        self.surface.clone(),
                        None,
                        RlmProjectedBindings::default(),
                        RlmLashlangExecutionTraceConfig::default(),
                        lashlang::ExecutionBounds::unbounded(),
                        crate::plugin::RlmChannel::Cell,
                    ),
                    async {
                        for _ in 0..20 {
                            self.table.admit_pending().await;
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                )
            },
        ))
        .await
        .expect("the cell ends or stops at its boundary");
        handler.close().await.expect("close the cell handler");
        response
    }

    fn state(&self) -> RlmExecutionState {
        RlmExecutionState::for_engine_with_workers("typescript", self.workers.clone())
    }

    /// Every process the session's cells started.
    async fn started(&self) -> usize {
        self.table
            .registry()
            .list_observed_by(
                &lash_core::SessionId::from("test-session"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await
            .expect("list the session's processes")
            .len()
    }
}

/// What a cell's response says it did, without the identities that differ
/// between two runs.
fn outcome(response: &ExecResponse) -> serde_json::Value {
    serde_json::json!({
        "error": response.error.as_ref().map(|error| error.message.clone()),
        "finish": response.terminal_finish,
        "prints": response
            .observations
            .iter()
            .map(|observation| format!("{:?}", observation.value))
            .collect::<Vec<_>>(),
        "calls": response
            .calls
            .iter()
            .map(|call| (call.operation.clone(), format!("{:?}", call.outcome)))
            .collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn a_cell_handed_over_at_its_await_resumes_with_every_ledger_it_held() {
    // The same cell, never stopped.
    let whole = Fixture::new(0x4739_0001).await;
    let mut whole_state = whole.state();
    let uninterrupted = whole.run_cell(&mut whole_state, "whole").await;
    assert!(!uninterrupted.suspended);
    assert!(uninterrupted.error.is_none(), "{:?}", uninterrupted.error);
    assert_eq!(
        uninterrupted.terminal_finish,
        Some(serde_json::json!({ "answer": "done", "before": 42 }))
    );
    assert_eq!(whole.started().await, 1);

    // The first segment: the build drains while the cell awaits its process.
    let fixture = Fixture::new(0x4739_0002).await;
    fixture.hand_over.store(true, Ordering::SeqCst);
    let mut first = fixture.state();
    let stopped = fixture.run_cell(&mut first, "first").await;
    assert!(
        stopped.suspended,
        "the cell stops at the await: {:?}",
        stopped.error
    );
    assert!(stopped.error.is_none(), "{:?}", stopped.error);
    assert!(
        stopped.terminal_finish.is_none() && stopped.observations.is_empty(),
        "a suspended cell has no answer and puts nothing into history"
    );
    assert_eq!(fixture.started().await, 1, "the cell started its process");
    assert!(first.suspended_cell().is_some());

    // The boundary's commit carries the cell in the session's durable
    // execution state; the successor segment restores it into a state that
    // never ran the cell.
    let durable = first
        .hydrated_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("capture the session's execution state at the boundary");
    drop(first);
    let mut second = fixture.state();
    second
        .restore_execution_state(&durable, lash_core::FleetFormat::current())
        .await
        .expect("restore the execution state in the successor segment");
    assert!(second.suspended_cell().is_some());

    // The successor segment: a new turn, a new handler, a new journal.
    fixture.hand_over.store(false, Ordering::SeqCst);
    let resumed = fixture.run_cell(&mut second, "second").await;
    assert!(!resumed.suspended);
    assert_eq!(
        outcome(&resumed),
        outcome(&uninterrupted),
        "the resumed cell ends as the uninterrupted cell does"
    );
    assert_eq!(
        fixture.started().await,
        1,
        "the start the first segment dispatched is never dispatched again"
    );
    assert!(
        second.suspended_cell().is_none(),
        "a cell that ran to its end leaves nothing suspended"
    );

    // The session's state after the resumed cell is the state after the
    // uninterrupted one: the next cell reads the same bindings.
    assert_eq!(
        second.bound_variable_values(&BTreeSet::new()),
        whole_state.bound_variable_values(&BTreeSet::new()),
    );
}

/// An await a cell performs in place — its worker declined to park, so no
/// state was captured — is never handed over: the cell would have nothing to
/// resume from.
#[tokio::test]
async fn an_execution_that_is_not_resumed_drops_the_suspended_cell() {
    let fixture = Fixture::new(0x4739_0003).await;
    fixture.hand_over.store(true, Ordering::SeqCst);
    let mut state = fixture.state();
    let stopped = fixture.run_cell(&mut state, "first").await;
    assert!(stopped.suspended, "{:?}", stopped.error);
    fixture.hand_over.store(false, Ordering::SeqCst);

    // Another cell runs in the suspended cell's place: the suspended cell is
    // abandoned, and the other cell starts fresh.
    let handler = fixture
        .table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(fixture.table.double(), &handler),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        Arc::clone(&fixture.processes),
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            fixture.session_policy.clone(),
        ),
    );
    let other = execute_code_with_test_render(
        &mut state,
        ctx,
        ExecRequest {
            code: "finish(1 + 1);".to_string(),
        },
        fixture.artifact_store.clone(),
        fixture.surface.clone(),
        None,
        RlmProjectedBindings::default(),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    handler.close().await.expect("close the cell handler");
    assert!(other.error.is_none(), "{:?}", other.error);
    assert_eq!(other.terminal_finish, Some(serde_json::json!(2)));
    assert!(state.suspended_cell().is_none());
}
