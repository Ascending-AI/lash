//! FIG-3587: a model call whose recorded envelope no longer matches the one a
//! redrive reconstructs parks its turn, and the turn finishes once the surface
//! it ran under is restored.
//!
//! The law runs on a protocol that journals the environment its model calls
//! are built from (the RLM protocol). First, a redrive whose registry
//! removed a tool the prompt rendered replays the recorded model call from
//! the journaled prompt with no provider request. Then, the first attempt
//! asks the model once and crashes after its effect loop, before the turn
//! commits. The redrive runs under a changed host setting the
//! model request is built from (the session's generation temperature): the
//! recorded model call's envelope hash conflicts, and the conflict parks the
//! turn — it aborts with the typed replay refusal, a `TurnPark` names the
//! diverged effect kind, the model is not asked again and nothing terminal is
//! written. A second redrive under the original setting replays the recorded
//! model call, finishes the turn and clears the park.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::store::SessionCommitStore as _;
use lash_sansio::{SessionId, TurnId};

/// The model's one answer: a cell that finishes the turn.
const ANSWER_CELL: &str = "<typescript>\nfinish(\"answered once\");\n</typescript>";

/// Panics when the effect loop ends: the model call is journaled and the
/// turn has not committed.
struct PanicBeforeTurnCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicBeforeTurnCommit {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            panic!("injected crash after the model call and before the turn commit");
        }
    }

    fn begin_named(&self, _phase: &str) {}
}

#[derive(Clone)]
struct DriftParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimePersistence>,
    /// What the registry offers for `tools.probe`, whose descriptor the
    /// model's prompt renders.
    probe: super::cell_binding_drift::Probe,
    executions: Arc<AtomicUsize>,
    /// The tier's protocol: one that journals the environment its model
    /// calls are built from (the RLM protocol).
    protocol: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: DriftParts, temperature: Option<f64>) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    policy.generation.temperature = temperature
        .map(|value| crate::NonNegativeFiniteF64::new(value).expect("a valid temperature"));
    let state = crate::RuntimeSessionState {
        session_id: parts.session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(
                parts
                    .protocol
                    .iter()
                    .cloned()
                    .chain([super::cell_binding_drift::probe_factory(
                        parts.probe,
                        Arc::clone(&parts.executions),
                    )])
                    .collect(),
            )
            .with_store(parts.store)
            .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
            .build(),
    )
    .await
    .expect("build the model-call drift conformance runtime")
}

fn drift_input(turn_id: &TurnId) -> crate::TurnInput {
    let mut input = crate::TurnInput::text("answer once");
    input.trace_turn_id = Some(turn_id.clone());
    input
}

/// Runs one attempt of the law's turn under `temperature` and sends back
/// what it returned.
fn attempt(
    parts: &DriftParts,
    turn_id: &TurnId,
    temperature: Option<f64>,
    crash: bool,
    result_tx: Option<
        tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>,
    >,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let turn_id = turn_id.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let turn_id = turn_id.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts, temperature).await;
            if crash {
                runtime.set_turn_phase_probe(Arc::new(PanicBeforeTurnCommit));
            }
            let turn = runtime
                .stream_turn(
                    drift_input(&turn_id),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            if crash {
                panic!("the crash probe did not fire before the turn commit: {turn:?}");
            }
            let end = crate::ConformanceTurnEnd::of(&turn);
            if let Some(result_tx) = result_tx {
                let _ = result_tx.send(turn);
            }
            end
        })
    })
}

/// A model call's replay hash conflict parks the turn with nothing asked
/// again and nothing terminal written; restoring the surface finishes it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn model_call_drift_parks_then_completes_once_restored(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) {
    let session_id = SessionId::from(format!("{prefix}-model-drift-session"));
    let turn_id = TurnId::from(format!("{prefix}-model-drift-turn"));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: ANSWER_CELL.to_string(),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut host = crate::LawBackend::in_process()
        .with_effect_host(Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let store = Arc::new(crate::InMemorySessionStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();

    // The journaled prompt is served: the redrive's registry removed the
    // tool the prompt rendered, and the recorded model call still replays
    // with no provider request, finishing the turn.
    let served_session = SessionId::from(format!("{prefix}-model-served-session"));
    let served_store = Arc::new(crate::InMemorySessionStore::new());
    let served = DriftParts {
        session_id: served_session.clone(),
        host: host.clone(),
        store: Arc::clone(&served_store) as Arc<dyn crate::RuntimePersistence>,
        probe: super::cell_binding_drift::Probe::Registered,
        executions: Arc::clone(&executions),
        protocol: protocol.clone(),
    };
    let removed = DriftParts {
        probe: super::cell_binding_drift::Probe::Removed,
        ..served.clone()
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&served_session, &turn_id)),
            attempt(&served, &turn_id, None, true, None),
            attempt(&removed, &turn_id, None, false, Some(result_tx.clone())),
        )
        .await;
    let turn = result_rx
        .recv()
        .await
        .expect("the redrive with the tool removed ran")
        .expect("a model call built from the journaled prompt replays");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "{:?}",
        turn.errors
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the redrive's model call replays from the journal with no provider request"
    );
    calls.store(0, Ordering::SeqCst);

    let parts = DriftParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        probe: super::cell_binding_drift::Probe::Registered,
        executions,
        protocol,
    };

    // The crash, then a redrive under a changed generation setting.
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            attempt(&parts, &turn_id, None, true, None),
            attempt(&parts, &turn_id, Some(0.5), false, Some(result_tx.clone())),
        )
        .await;
    let asked = calls.load(Ordering::SeqCst);
    assert_eq!(asked, 1, "the crashed attempt asked the model once");
    let error = result_rx
        .recv()
        .await
        .expect("the drifted redrive ran")
        .expect_err("a model call whose envelope drifted refuses its replay");
    assert!(
        error.code.parks_turn(),
        "a model-call replay hash conflict parks the turn: {error:?}"
    );
    let park = store
        .load_turn_park(&session_id)
        .await
        .expect("read the park")
        .expect("the drifted turn is parked");
    assert_eq!(park.turn_id, turn_id);
    let crate::store::TurnParkReason::EffectReplayDivergence { effect_kind, .. } = &park.reason
    else {
        panic!("the park names an effect replay divergence: {park:?}");
    };
    assert_eq!(
        effect_kind, "llm_call",
        "the park names the diverged effect"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        asked,
        "the model is not asked again"
    );

    // A second drifted redrive refuses again, dispatching nothing.
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            Box::new({
                let attempt = attempt(&parts, &turn_id, Some(0.5), false, Some(result_tx.clone()));
                move |scope| attempt(scope)
            }),
        )
        .await;
    let error = result_rx
        .recv()
        .await
        .expect("the second drifted redrive ran")
        .expect_err("the drifted redrive refuses again");
    assert!(error.code.parks_turn(), "{error:?}");
    assert_eq!(calls.load(Ordering::SeqCst), asked);

    // The surface restored: the redrive replays the recorded call and
    // finishes, and the commit clears the park.
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            Box::new({
                let attempt = attempt(&parts, &turn_id, None, false, Some(result_tx));
                move |scope| attempt(scope)
            }),
        )
        .await;
    let turn = result_rx
        .recv()
        .await
        .expect("the restored redrive ran")
        .expect("the restored redrive assembles");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "restored outcome: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        asked,
        "the restored redrive answers from the recorded model call"
    );
    assert!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park")
            .is_none(),
        "the finished turn's commit clears the park"
    );
}
