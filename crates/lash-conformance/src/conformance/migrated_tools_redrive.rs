//! FIG-1293: the public migrated tools settle to literal outcomes across a
//! crash-and-redrive of the turn that called them.
//!
//! One model response calls three migrated public tools in one turn:
//! `cancel_process` (a process-control intent), `spawn_agent` (an
//! orchestrating start whose child session runs as a process) and `batch`
//! (the protocol's nested batch of two echo calls). The turn crashes after
//! its tool calls settled and before it commits, and the tier redelivers it
//! the way it recovers a crashed turn. The redriven turn finishes with the
//! literal outcomes the first attempt produced, without asking the model
//! again: every call settled once, as a group child, and the redrive reads
//! the settlements back.
//!
//! `spawn_agent` and `cancel_process` come from plugins that sit above this
//! crate in the dependency graph, so the tier supplies them as
//! `orchestration` factories — the same way producers from higher crates
//! reach the tool-batch parallelism law.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{ProcessId, SessionId, TurnId};
use pretty_assertions::assert_eq;

/// Panics when the effect loop ends: every tool call has settled and the turn
/// has not committed.
struct PanicBeforeTurnCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicBeforeTurnCommit {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            panic!("injected crash after the tool calls settled and before the turn commit");
        }
    }

    fn begin_named(&self, _phase: &str) {}
}

fn migrated_model(
    prefix: &str,
    target: &ProcessId,
) -> (crate::testing::TestProvider, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let prefix = prefix.to_string();
    let target = target.to_string();
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let prefix = prefix.clone();
                let target = target.clone();
                async move {
                    let tool_call = |call_id: String, tool_name: &str, input: serde_json::Value| {
                        crate::LlmOutputPart::ToolCall {
                            call_id,
                            tool_name: tool_name.to_string(),
                            input_json: input.to_string(),
                            replay: None,
                        }
                    };
                    let text = |text: &str| crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: text.to_string(),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    };
                    Ok(match index {
                        0 => crate::LlmResponse {
                            parts: vec![
                                tool_call(
                                    format!("{prefix}-process-cancel"),
                                    "cancel_process",
                                    serde_json::json!({ "process_id": target }),
                                ),
                                tool_call(
                                    format!("{prefix}-spawn-agent"),
                                    "spawn_agent",
                                    serde_json::json!({
                                        "capability": "default",
                                        "task": "Return the literal child result.",
                                    }),
                                ),
                                tool_call(
                                    format!("{prefix}-batch"),
                                    "batch",
                                    serde_json::json!({
                                        "tool_calls": [
                                            {"tool": crate::testing::FIXTURE_ECHO_TOOL, "parameters": {"value": "alpha"}},
                                            {"tool": crate::testing::FIXTURE_ECHO_TOOL, "parameters": {"value": "beta"}},
                                        ]
                                    }),
                                ),
                            ],
                            ..crate::LlmResponse::default()
                        },
                        1 => text("child literal"),
                        2 => text("migrated tools complete"),
                        index => panic!("unexpected migrated-tools model call {index}"),
                    })
                }
            }
        })
        .build();
    (model, calls)
}

/// Everything a runtime for this law is built from, shared by the crashing
/// attempt and the redrive so the redrive is the same turn on the same state.
#[derive(Clone)]
struct MigratedRuntimeParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    store: Arc<dyn crate::RuntimePersistence>,
    registry: Arc<dyn crate::ProcessRegistry>,
    process_work: crate::ProcessWorkWiring,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_migrated_runtime(parts: MigratedRuntimeParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
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
            .with_plugin_factories(parts.factories)
            .with_store(parts.store)
            .with_process_registry(parts.registry)
            .with_process_work(parts.process_work)
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the migrated-tools conformance runtime")
}

fn migrated_input(turn_id: &TurnId) -> crate::TurnInput {
    let mut input = crate::TurnInput::text("finish once");
    input.trace_turn_id = Some(turn_id.clone());
    input
}

/// The recorded outputs, with batch child durations dropped: a duration is
/// wall-clock, not part of the literal outcome.
fn literal_outputs(turn: &crate::AssembledTurn) -> Vec<(String, serde_json::Value)> {
    turn.tool_calls
        .iter()
        .map(|record| {
            let mut value = record.output.value_for_projection();
            if let Some(results) = value
                .get_mut("results")
                .and_then(serde_json::Value::as_array_mut)
            {
                for result in results {
                    if let Some(result) = result.as_object_mut() {
                        result.remove("duration_ms");
                    }
                }
            }
            (record.tool.clone(), value)
        })
        .collect()
}

/// The public migrated tools settle once and redrive to literal outcomes.
///
/// The first attempt panics after its tool calls settled and before its turn
/// commits; the tier's runner redelivers the turn, and the redriven turn
/// finishes with the literal cancel, child and batch outcomes, having asked
/// the model exactly the three times the first attempt did.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn public_migrated_tools_redrive_to_literal_outcomes(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    orchestration: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) {
    let registry = stores.process_registry();
    let session_id = SessionId::from(format!("{prefix}-session"));
    let turn_id = TurnId::from(format!("{prefix}-turn"));
    let target = registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                crate::ProcessInput::External {
                    metadata: serde_json::json!({ "fixture": "migrated-tools" }),
                },
                // A fixture-owned external process: the worker must not try to
                // recover an input it does not own.
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ),
            std::slice::from_ref(&session_id),
        )
        .await
        .expect("register the cancel_process target")
        .id;

    let (model, model_calls) = migrated_model(prefix, &target);
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let echo: Arc<dyn crate::ToolProvider> = Arc::new(crate::testing::FixtureTools);
    let factories = crate::testing::test_standard_protocol_factories()
        .into_iter()
        .chain(orchestration)
        .chain([Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-migrated-echo",
            crate::facade_support::PluginSpec::new().with_tool_provider(echo),
        )) as Arc<dyn crate::facade_support::PluginFactory>])
        .collect::<Vec<_>>();
    // One watch, two consumers: the runtime's process port and the worker
    // observe the same registry handle.
    let watched = crate::facade_support::watch_process_registry(Arc::clone(&registry));
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(crate::facade_support::PluginHost::new(factories.clone())),
            host.clone(),
            lash_core_worker::WorkerProcessWork::SelfNative(watched.clone()),
            Arc::new(crate::NoSessionWork::new()),
            crate::testing::runtime_lease_owner(),
        ),
    )
    .expect("build the migrated-tools process worker");
    let parts = MigratedRuntimeParts {
        session_id: session_id.clone(),
        host,
        factories,
        store: crate::conformance::law_session_store(stores.as_ref(), &session_id).await,
        registry: Arc::clone(watched.registry()),
        process_work: runner.process_work(watched, worker),
    };

    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let turn_id = turn_id.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let turn_id = turn_id.clone();
            Box::pin(async move {
                let mut runtime = build_migrated_runtime(parts).await;
                runtime.set_turn_phase_probe(Arc::new(PanicBeforeTurnCommit));
                let turn = runtime
                    .stream_turn(
                        migrated_input(&turn_id),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                panic!("the crash probe did not fire before the turn commit: {turn:?}");
            })
        })
    };
    let (turn_tx, turn_rx) = tokio::sync::oneshot::channel();
    let turn_tx = Arc::new(std::sync::Mutex::new(Some(turn_tx)));
    let redrive: crate::ConformanceTurnAttempt = {
        let turn_id = turn_id.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let turn_id = turn_id.clone();
            let turn_tx = Arc::clone(&turn_tx);
            Box::pin(async move {
                let mut runtime = build_migrated_runtime(parts).await;
                let turn = runtime
                    .stream_turn(
                        migrated_input(&turn_id),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                if let Some(turn_tx) = turn_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = turn_tx.send(turn);
                }
                end
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            crashing,
            redrive,
        )
        .await;
    let turn = turn_rx
        .await
        .expect("the tier's runner ran the redriven turn")
        .expect("the redriven turn assembles");

    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "redriven outcome: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        literal_outputs(&turn),
        vec![
            (
                "cancel_process".to_string(),
                serde_json::json!({
                    "process_id": target.to_string(),
                    "status": "cancelled",
                }),
            ),
            (
                "spawn_agent".to_string(),
                serde_json::json!("child literal")
            ),
            (
                "batch".to_string(),
                serde_json::json!({
                    "results": [
                        {
                            "index": 0,
                            "result": {"echo": "alpha"},
                            "success": true,
                            "tool": crate::testing::FIXTURE_ECHO_TOOL,
                        },
                        {
                            "index": 1,
                            "result": {"echo": "beta"},
                            "success": true,
                            "tool": crate::testing::FIXTURE_ECHO_TOOL,
                        },
                    ]
                }),
            ),
        ]
    );
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        3,
        "the redrive reads every settled call and model response back instead of asking again"
    );
}
