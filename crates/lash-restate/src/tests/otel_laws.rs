//! Golden law P3 of the tracing design (FIG-4830): a turn's trace is the same
//! whether its handler runs once or replays its journal from the start at
//! every await.
//!
//! The turn runs through the endpoint's real turn handler on the in-process
//! server double, over a SQLite memory store set. Its drive observes around
//! each recorded step and each step's body observes from inside, the way the
//! engine's turn loop and its effect bodies do. Under always-replay every
//! step suspends the handler, and every resumption replays the journal up to
//! the next step, which then runs live: a new live suffix after each replay.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use super::*;

/// How many recorded steps the turn issues.
const STEPS: usize = 3;

fn observation(label: String) -> (lash_trace::TraceContext, lash_trace::TraceEvent) {
    (
        lash_trace::TraceContext::default(),
        lash_trace::TraceEvent::ProtocolStep {
            plugin_id: "golden-tree".to_string(),
            payload: serde_json::json!(label),
        },
    )
}

/// What one run of the turn left behind.
struct Observed {
    /// Each record's label, in emission order.
    labels: Vec<String>,
    /// Each record's id.
    ids: Vec<String>,
    /// How many times the handler ran the turn from its start.
    handler_runs: usize,
    /// How many times a step body recorded its live-class metric.
    metric: usize,
}

async fn run_golden_turn(always_replay: bool) -> Observed {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("in_process names the server double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await;
    let sink = Arc::new(RecordingTraceSink::default());
    let sink_dyn: Arc<dyn lash_trace::TraceSink> = sink.clone();
    let tracing = lash_core::facade_support::TraceRuntime::default().with_trace_sink(sink_dyn);
    let session_id = SessionId::fixture(format!("golden-tree-{}", harness.run_nonce()));
    let turn_id = TurnId::from("golden-tree-turn");
    let handler_runs = Arc::new(AtomicUsize::new(0));
    let metric = Arc::new(AtomicUsize::new(0));

    let attempt: lash_conformance::ConformanceTurnAttempt = {
        let tracing = tracing.clone();
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let handler_runs = Arc::clone(&handler_runs);
        let metric = Arc::clone(&metric);
        Arc::new(move |controller| {
            let tracing = tracing.clone();
            let session_id = session_id.clone();
            let turn_id = turn_id.clone();
            let handler_runs = Arc::clone(&handler_runs);
            let metric = Arc::clone(&metric);
            Box::pin(async move {
                handler_runs.fetch_add(1, Ordering::SeqCst);
                let turn = tracing.turn_drive(&session_id, &turn_id, &controller);
                turn.observe(|| observation("turn started".to_string()));
                for step in 0..STEPS {
                    turn.observe(|| observation(format!("step {step} issued")));
                    let effect_id = format!("golden-tree-step-{step}");
                    let envelope = RuntimeEffectEnvelope::new(
                        lash_core::RuntimeEffectInvocation::new(
                            lash_core::EffectAddress::new(
                                ExecutionScope::turn(&session_id, &turn_id),
                                effect_id.clone(),
                            )
                            .expect("valid turn effect address"),
                            lash_core::RuntimeAttribution::for_turn(&session_id, &turn_id, 0, step),
                            effect_id.clone(),
                        ),
                        RuntimeEffectCommand::ToolAttempt {
                            call: Box::new(prepared_tool_call_with(&effect_id, "golden_tree_tool")),
                            execution_grant: None,
                            attempt: 1,
                            max_attempts: 1,
                        },
                    );
                    let body_tracing = tracing.clone();
                    let body_metric = Arc::clone(&metric);
                    controller
                        .execute_effect(
                            envelope,
                            RuntimeEffectLocalExecutor::testing_in_step(
                                move |envelope, live| async move {
                                    let body =
                                        body_tracing.effect_body(&envelope.invocation, &live);
                                    // A live-class metric is recorded under
                                    // the body's permit, which only a body
                                    // that really runs holds.
                                    if body.body_permit().is_some() {
                                        body_metric.fetch_add(1, Ordering::SeqCst);
                                    }
                                    body.observe(|| observation(format!("step {step} ran")));
                                    Ok(restate_segment_tool_attempt_outcome(step as u64))
                                },
                            ),
                        )
                        .await
                        .expect("the recorded step answers");
                    turn.observe(|| observation(format!("step {step} answered")));
                }
                turn.observe(|| observation("turn completed".to_string()));
                turn.conclude();
                lash_conformance::ConformanceTurnEnd::Settled
            })
        })
    };
    harness
        .turn_runner()
        .run_turn(
            lash_core::AdmittedScope::turn(&session_id, &turn_id),
            attempt,
        )
        .await;

    let records = sink.records.lock_recover();
    let labels = records
        .iter()
        .map(|record| match &record.event {
            lash_trace::TraceEvent::ProtocolStep { payload, .. } => payload
                .as_str()
                .expect("the law's records carry a label")
                .to_string(),
            other => panic!("the law emits only its own records, got {}", other.kind()),
        })
        .collect();
    let ids = records.iter().map(|record| record.id.clone()).collect();
    Observed {
        labels,
        ids,
        handler_runs: handler_runs.load(Ordering::SeqCst),
        metric: metric.load(Ordering::SeqCst),
    }
}

fn golden_tree() -> Vec<String> {
    let mut tree = vec!["turn started".to_string()];
    for step in 0..STEPS {
        tree.push(format!("step {step} issued"));
        tree.push(format!("step {step} ran"));
        tree.push(format!("step {step} answered"));
    }
    tree.push("turn completed".to_string());
    tree
}

#[tokio::test]
async fn golden_tree_survives_replay_and_redrive() {
    let once = run_golden_turn(false).await;
    assert_eq!(
        once.labels,
        golden_tree(),
        "a handler that runs once observes the whole tree in order"
    );
    assert_eq!(once.metric, STEPS, "each body records its metric once");

    let replayed = run_golden_turn(true).await;
    assert!(
        replayed.handler_runs > once.handler_runs,
        "always-replay re-ran the handler from its start ({} runs against {})",
        replayed.handler_runs,
        once.handler_runs
    );
    assert_eq!(
        replayed.labels,
        golden_tree(),
        "a replay observes nothing an earlier attempt observed, and every live suffix after a \
         replay is observed once, in order"
    );
    assert_eq!(
        replayed.metric, STEPS,
        "a replayed step records no metric; each body recorded its metric once"
    );
    let distinct = replayed.ids.iter().collect::<HashSet<_>>();
    assert_eq!(
        distinct.len(),
        replayed.ids.len(),
        "every record names itself once"
    );
}
