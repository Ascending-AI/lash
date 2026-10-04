use super::conformance_harness::LiveConformanceHarness;
use super::*;
use lash::remote::turn_control::RemoteTurnCancelRequest;
use lash_core::facade_support::{
    AssembledTurn, LashRuntime, RuntimeHostConfig, TurnCancelMode, TurnCancelRequest, TurnOptions,
    TurnOutcome, TurnStop, TurnWorkDriver,
};
use lash_core::testing::TestTurnExecution as _;
use tokio_util::sync::CancellationToken;

/// The boundary events of a turn, one inner list per handler execution.
///
/// Restate runs the handler again from the top on every replay of the
/// invocation, and each execution feeds the sinks it is handed: an execution
/// suspended mid-turn leaves a prefix of the committed boundaries, the
/// execution that ends leaves them all. The law asserts that per-execution
/// shape — what replay must preserve — not one flat sequence across
/// executions.
#[derive(Clone, Default)]
struct BoundaryEvents(Arc<Mutex<Vec<Vec<&'static str>>>>);

impl BoundaryEvents {
    /// The committed boundary sequence a turn that stops after one committed
    /// step records.
    const COMMITTED: [&'static str; 2] = ["checkpoint", "stopped"];

    /// Open the next execution's list; the turn fixture calls it before each
    /// execution executes the turn.
    fn begin_execution(&self) {
        self.0.lock_recover().push(Vec::new());
    }

    fn record(&self, event: &'static str) {
        self.0
            .lock_recover()
            .last_mut()
            .expect("a turn execution is recording")
            .push(event);
    }
}

#[async_trait::async_trait]
impl lash_core::runtime::EventSink for BoundaryEvents {
    async fn emit(&self, event: lash_sansio::SessionStreamEvent) {
        if matches!(
            event,
            lash_sansio::SessionStreamEvent::TurnOutcome {
                outcome: TurnOutcome::Stopped(_)
            }
        ) {
            self.record("stopped");
        }
    }
}

#[async_trait::async_trait]
impl lash_core::runtime::TurnActivitySink for BoundaryEvents {
    async fn emit(&self, activity: lash_core::TurnActivity) {
        if matches!(
            activity.event,
            lash_core::TurnEvent::CheckpointRecorded { .. }
        ) {
            self.record("checkpoint");
        }
    }
}

#[derive(Clone)]
struct TurnFixture {
    backend: lash_core::Backend,
    view: lash_core::store::SessionStore,
    provider: lash_core::testing::TestProvider,
    turn_id: TurnId,
    events: BoundaryEvents,
}

impl TurnFixture {
    async fn run(
        &self,
        scope: ScopedEffectController<'_>,
    ) -> Result<AssembledTurn, lash_core::RuntimeError> {
        self.events.begin_execution();
        let mut config = RuntimeHostConfig::new(
            self.backend.clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        config.providers.models =
            lash_core::testing::standard_test_llm_profiles(self.provider.clone().into_handle());
        let policy = lash_core::testing::mock_session_policy();
        let state = lash_core::RuntimeSessionState {
            session_id: self.view.session_id().clone(),
            policy: policy.clone(),
            ..lash_core::RuntimeSessionState::new(policy.clone())
        };
        let tools = Arc::new(lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("remote-cancel-echo"),
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::new(lash_core::testing::runtime_helpers::EchoTool)),
        ));
        let mut runtime = Box::pin(
            LashRuntime::builder(config, lash_core::testing::runtime_lease_owner())
                .with_session_id(self.view.session_id())
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(
                    lash_core::testing::test_standard_protocol_factories()
                        .into_iter()
                        .chain([tools as Arc<dyn lash_core::facade_support::PluginFactory>])
                        .collect(),
                )
                .with_store(self.view.clone())
                .build(),
        )
        .await
        .expect("build the held-step runtime");
        let mut input = lash_core::TurnInput::text("finish this step before stopping");
        input.trace_turn_id = Some(self.turn_id.clone());
        runtime
            .execute_turn(
                input,
                TurnOptions::new(CancellationToken::new(), scope)
                    .with_events(&self.events)
                    .with_turn_events(&self.events),
            )
            .await
    }
}

pub(super) enum TurnRunner<Stores: lash_core::StoreSet + ?Sized> {
    Double(lash_restate_test::RestateTestBackend<Stores>),
    Live(Arc<dyn lash_conformance::ConformanceTurnRunner>),
}

pub(super) async fn held_step_law<Stores: lash_core::StoreSet + ?Sized + 'static>(
    backend: lash_core::Backend,
    runner: TurnRunner<Stores>,
    prefix: &str,
) {
    let session = SessionId::fixture(format!("{prefix}-session"));
    let turn_id = TurnId::fixture(format!("{prefix}-turn"));
    let view = lash_core::runtime::admit_session_view(
        &backend.session_store_factory(),
        &lash_core::testing::store_fixtures::session_store_request(
            &session,
            "remote-cancel-model",
            lash_core::SessionRelation::Root,
        ),
    )
    .await
    .expect("admit the held-step session");
    let driver = TurnWorkDriver::for_session(
        backend.effect_host(),
        session.clone(),
        Arc::clone(view.store()),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let entered = Arc::clone(&entered);
            let release = release.clone();
            let calls = Arc::clone(&calls);
            move |_request| {
                let entered = Arc::clone(&entered);
                let release = release.clone();
                let calls = Arc::clone(&calls);
                async move {
                    assert_eq!(
                        calls.fetch_add(1, Ordering::SeqCst),
                        0,
                        "no next step starts"
                    );
                    entered.notify_one();
                    release.cancelled().await;
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::ToolCall {
                            call_id: "held-step-echo".into(),
                            tool_name: "echo_tool".into(),
                            input_json: serde_json::json!({"value": "committed step"}).to_string(),
                            replay: None,
                        }],
                        ..Default::default()
                    })
                }
            }
        })
        .build();
    let events = BoundaryEvents::default();
    let fixture = TurnFixture {
        backend,
        view: view.clone(),
        provider,
        turn_id: turn_id.clone(),
        events: events.clone(),
    };
    let admitted = lash_core::AdmittedScope::turn(&session, &turn_id);
    let mut turn = tokio::spawn(async move {
        match runner {
            TurnRunner::Double(double) => {
                let handler = double
                    .open_handler(admitted)
                    .await
                    .expect("open the turn handler");
                let result = fixture.run(handler.scoped()).await;
                handler.close().await.expect("close the turn handler");
                result
            }
            TurnRunner::Live(runner) => {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let attempt: lash_conformance::ConformanceTurnAttempt = Arc::new(move |scope| {
                    let fixture = fixture.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        let result = fixture.run(scope).await;
                        let end = lash_conformance::ConformanceTurnEnd::of(&result);
                        tx.send(result).expect("report the live turn result");
                        end
                    })
                });
                runner.run_turn(admitted, attempt).await;
                rx.recv().await.expect("the live runner drove the turn")
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::select! {
            () = entered.notified() => {}
            ended = &mut turn => panic!("turn ended before entering the held step: {ended:?}"),
        }
    })
    .await
    .expect("the provider holds the current step");
    let request = TurnCancelRequest::new(
        TurnAddress::new(&session, &turn_id),
        "remote-stop",
        Some("remote-host".into()),
    )
    .with_reason("stop after the committed step")
    .mode(TurnCancelMode::AfterStep);
    let wire = serde_json::to_vec(&RemoteTurnCancelRequest::from(request))
        .expect("encode the remote cancellation");
    let decoded: RemoteTurnCancelRequest =
        serde_json::from_slice(&wire).expect("decode the remote cancellation");
    let receipt = driver
        .request_cancel(decoded.try_into_core().expect("core cancellation"))
        .await
        .expect("deliver the decoded cancellation through Restate");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut turn)
            .await
            .is_err(),
        "remote AfterStep must not interrupt the held step"
    );
    assert!(
        events
            .0
            .lock_recover()
            .iter()
            .all(|execution| execution.is_empty()),
        "no boundary is committed while the step is held"
    );
    release.cancel();
    let turn = tokio::time::timeout(Duration::from_secs(60), turn)
        .await
        .expect("stop after the committed step")
        .expect("join the turn")
        .expect("assemble the turn");
    let TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) = turn.outcome else {
        panic!("the remote cancellation must stop the turn");
    };
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(
        evidence.honoured_after_step,
        Some(0),
        "boundary events: {:?}; tool calls: {:?}",
        *events.0.lock_recover(),
        turn.tool_calls,
    );
    assert_eq!(evidence.request_id, "remote-stop");
    assert_eq!(evidence.origin.as_deref(), Some("remote-host"));
    assert_eq!(
        evidence.reason.as_deref(),
        Some("stop after the committed step")
    );
    assert!(
        matches!(receipt.outcome, lash_core::facade_support::TurnCancelOutcome::Requested(ref accepted)
        if accepted.mode == TurnCancelMode::AfterStep && accepted.honoured_after_step.is_none())
    );
    let executions = events.0.lock_recover().clone();
    assert!(
        executions
            .iter()
            .all(|execution| BoundaryEvents::COMMITTED.starts_with(execution)),
        "every execution replays a prefix of the committed boundaries: {executions:?}"
    );
    assert!(
        executions
            .iter()
            .any(|execution| execution.as_slice() == BoundaryEvents::COMMITTED),
        "the completing execution observes every committed boundary: {executions:?}"
    );
    assert_eq!(
        turn.tool_calls.len(),
        1,
        "the held step's tool result is retained"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let terminal = view
        .run_terminal(&turn_id)
        .await
        .expect("read durable terminal")
        .expect("the stopped turn is committed in the store");
    assert!(
        matches!(terminal.cause, lash_core::store::RunTerminalCause::Committed {
        outcome: lash_core::store::RunCommittedOutcome::Stopped(TurnStop::Cancelled { evidence: ref durable }), ..
    } if durable == &evidence),
        "the store retains the exact checkpoint evidence"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_after_step_waits_for_committed_boundary() {
    let double = lash_restate_test::backend(0x4282, lash_restate_test::ServerConfig::default())
        .await
        .expect("start the SQLite Restate double");
    held_step_law(
        double.lash_backend(),
        TurnRunner::Double(double),
        "remote-sqlite",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live Restate cancellation delivery: kiln gate with the remote-cancellation suite"]
async fn live_remote_after_step_waits_for_committed_boundary() {
    let harness = LiveConformanceHarness::start_for_tools().await;
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(harness.law_backend())
        .map_effect_host(|_| harness.endpoint_host())
        .into_backend();
    held_step_law(
        backend,
        TurnRunner::<dyn lash_core::StoreSet>::Live(harness.turn_runner()),
        &format!("remote-live-{}", harness.run_nonce()),
    )
    .await;
    harness.finish().await;
}
