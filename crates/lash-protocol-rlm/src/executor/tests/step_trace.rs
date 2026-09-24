use super::*;

#[derive(Default)]
struct StepSink {
    records: Mutex<Vec<lash_core::facade_support::TraceRecord>>,
    /// Cancels the token when the `n`th sleep wait (1-based) is recorded.
    cancel_on_sleep_wait: Option<(lash_core::CancellationToken, usize)>,
    sleep_waits: AtomicUsize,
}

impl TraceSink for StepSink {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        self.records.lock().unwrap().push(record.clone());
        if let Some((cancel, cancel_at)) = &self.cancel_on_sleep_wait
            && matches!(
                &record.event,
                lash_trace::TraceEvent::LanguageExecution {
                    event: lash_lashlang_runtime::TraceLanguageExecution {
                        payload:
                            lash_lashlang_runtime::TraceLanguageExecutionPayload::NodeWaiting {
                                awaited: lash_lashlang_runtime::TraceNodeAwaited::Sleep { .. },
                                ..
                            },
                        ..
                    },
                    ..
                }
            )
            && self.sleep_waits.fetch_add(1, Ordering::SeqCst) + 1 == *cancel_at
        {
            cancel.cancel();
        }
        Ok(())
    }
}

struct InboxResolver;

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for InboxResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        assert_eq!(paths, &["inbox.send_item"]);
        BTreeMap::from([("inbox.send_item".into(), lash_lashlang_runtime::Resolution::Resolved(Box::new(
            lash_lashlang_runtime::ToolGrant::new(lash_core::ToolDefinition::raw(
                "tool:send_item", "send_item", "Send an item",
                serde_json::json!({"type":"object","properties":{"body":{"type":"string"}},"required":["body"],"additionalProperties":false}),
                serde_json::json!({"type":"string"}),
            ).with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["inbox"], "send_item")))
        )))])
    }
}

async fn run_step(code: &str) -> (ExecResponse, Vec<lash_core::facade_support::TraceRecord>) {
    let sink = Arc::new(StepSink::default());
    let response = run_step_with_sink(code, sink.clone(), None).await;
    let records = sink.records.lock().unwrap().clone();
    (response, records)
}

async fn run_step_with_sink(
    code: &str,
    sink: Arc<StepSink>,
    cancellation: Option<lash_core::CancellationToken>,
) -> ExecResponse {
    let executions = Arc::new(AtomicUsize::new(0));
    let mut ctx =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            crate::testing::memory_backend_ports().await,
            Arc::new(BindingRecordingDeferredProvider {
                executions: executions.clone(),
                observed_bindings: Default::default(),
                enumerations: Default::default(),
            }),
            lash_core::ToolCatalog::default(),
            lash_core::testing::exec_code_invocation(
                "trace-session",
                "trace-turn",
                2,
                7,
                "trace-exec",
                "exec:trace",
            ),
        );
    if let Some(cancellation) = cancellation {
        ctx = ctx.with_cancellation_token(cancellation);
    }
    let response = execute_code_unbounded_for_tests(
        &mut RlmExecutionState::new(),
        ctx,
        ExecRequest {
            language: "typescript".into(),
            code: code.into(),
        },
        crate::testing::memory_artifact_store().await,
        LashlangSurface::default(),
        Some(Arc::new(InboxResolver)),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig {
            sink: Some(sink.clone()),
            trace_context: TraceContext::default(),
        },
    )
    .await;
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    response
}

#[test]
fn real_foreground_sleep_reduces_waiting_then_completed() {
    block_on(async {
        let (response, records) = Box::pin(run_step("await sleep(0); finish(null);")).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let store = lash_lashlang_runtime::TraceLashlangGraphStore::default();
        let mut awaited_node = None;
        for record in &records {
            store.append(record).expect("reduce real foreground trace");
            if let lash_trace::TraceEvent::LanguageExecution { event, .. } = &record.event
                && let lash_lashlang_runtime::TraceLanguageExecutionPayload::NodeWaiting {
                    node_id,
                    awaited: lash_lashlang_runtime::TraceNodeAwaited::Sleep { deadline_ms: None },
                    ..
                } = &event.payload
            {
                let graph = store
                    .graph(&event.identity.graph_key())
                    .expect("waiting graph");
                assert!(graph.nodes.iter().any(|node| {
                    node.id == *node_id
                        && matches!(
                            node.observation,
                            lash_lashlang_runtime::TraceLashlangNodeObservation::Waiting { .. }
                        )
                }));
                awaited_node = Some((event.identity.graph_key(), node_id.clone()));
            }
        }
        let (graph_key, node_id) = awaited_node.expect("sleep emitted a wait");
        let graph = store.graph(&graph_key).expect("completed graph");
        assert!(graph.nodes.iter().any(|node| {
            node.id == node_id
                && matches!(
                    node.observation,
                    lash_lashlang_runtime::TraceLashlangNodeObservation::Completed { .. }
                )
        }));
    });
}

fn reduce(
    records: &[lash_core::facade_support::TraceRecord],
) -> lash_lashlang_runtime::TraceLashlangGraph {
    let store = lash_lashlang_runtime::TraceLashlangGraphStore::default();
    for record in records {
        store.append(record).expect("reduce foreground trace");
    }
    store.graphs().into_iter().next().expect("execution graph")
}

fn observations_of_kind(
    graph: &lash_lashlang_runtime::TraceLashlangGraph,
    kind: lash_sansio::ExecutionNodeKind,
) -> Vec<(String, lash_lashlang_runtime::TraceLashlangNodeObservation)> {
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == kind)
        .map(|node| (node.id.clone(), node.observation.clone()))
        .collect()
}

/// Cancellation after partial completion: the first sleep completed, the
/// second is parked when the cell is cancelled, and the third never starts.
#[test]
fn real_foreground_cancel_after_partial_completion_keeps_each_occurrence_honest() {
    use lash_lashlang_runtime::TraceLashlangNodeObservation as Observation;
    block_on(async {
        let cancellation = lash_core::CancellationToken::new();
        let sink = Arc::new(StepSink {
            cancel_on_sleep_wait: Some((cancellation.clone(), 2)),
            ..StepSink::default()
        });
        let _response = Box::pin(run_step_with_sink(
            "await sleep(0); await sleep(0); await sleep(0); finish(null);",
            sink.clone(),
            Some(cancellation.clone()),
        ))
        .await;
        assert!(
            cancellation.is_cancelled(),
            "the second sleep wait must trigger cancellation"
        );
        let records = sink.records.lock().unwrap().clone();
        let graph = reduce(&records);
        assert!(graph.conflicts.is_empty(), "{:?}", graph.conflicts);
        let sleeps = observations_of_kind(&graph, lash_sansio::ExecutionNodeKind::Sleep);
        assert_eq!(sleeps.len(), 3, "{sleeps:#?}");
        let count = |matches: fn(&Observation) -> bool| {
            sleeps
                .iter()
                .filter(|(_, observation)| matches(observation))
                .count()
        };
        assert_eq!(
            count(|o| matches!(o, Observation::Completed { .. })),
            1,
            "{sleeps:#?}"
        );
        assert_eq!(
            count(|o| matches!(o, Observation::Cancelled { .. })),
            1,
            "{sleeps:#?}"
        );
        assert_eq!(
            count(|o| matches!(o, Observation::Unobserved)),
            1,
            "{sleeps:#?}"
        );
        assert!(records.iter().any(|record| matches!(
            &record.event,
            lash_trace::TraceEvent::LanguageExecution { event, .. }
                if matches!(
                    event.payload,
                    lash_lashlang_runtime::TraceLanguageExecutionPayload::NodeResumed {
                        resolution: lash_lashlang_runtime::TraceNodeWaitResolution::Cancelled,
                        ..
                    }
                )
        )));
    });
}

#[test]
fn rejected_deferred_contract_step_is_visible_in_trace_sink() {
    block_on(async {
        let (response, records) = Box::pin(run_step("await inbox.send_item({});")).await;
        let diagnostic = response
            .error
            .expect("contract must reject the program")
            .message;
        assert!(diagnostic.contains("body"), "{diagnostic}");
        let step = records
            .iter()
            .find(|r| r.event.kind() == "rlm_step")
            .expect("rejected step must emit trace evidence");
        let event = serde_json::to_value(&step.event).unwrap();
        assert_eq!(event["step_index"], 7);
        assert_eq!(event["outcome"], "failure");
        assert_eq!(event["diagnostic"], diagnostic);
        assert_eq!(step.context.session_id.as_deref(), Some("trace-session"));
        assert_eq!(step.context.turn_id.as_deref(), Some("trace-turn"));
        assert!(step.event.is_failed());
    });
}

#[test]
fn successful_compile_step_is_visible_in_trace_sink() {
    block_on(async {
        let (response, records) = Box::pin(run_step("finish(42);")).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let steps: Vec<_> = records
            .iter()
            .filter(|r| r.event.kind() == "rlm_step")
            .collect();
        assert_eq!(steps.len(), 1);
        let event = serde_json::to_value(&steps[0].event).unwrap();
        assert_eq!(event["step_index"], 7);
        assert_eq!(event["outcome"], "ok");
        assert!(event.get("diagnostic").is_none());
        assert!(!steps[0].event.is_failed());
    });
}

#[test]
fn oversized_link_failure_diagnostic_is_bounded_without_changing_feedback() {
    block_on(async {
        let code = format!("finish(missing_{});", "x".repeat(8000));
        let (response, records) = Box::pin(run_step(&code)).await;
        let diagnostic = response.error.expect("unknown name must fail").message;
        assert!(diagnostic.chars().count() > 4000);
        let steps: Vec<_> = records
            .iter()
            .filter(|r| r.event.kind() == "rlm_step")
            .collect();
        assert_eq!(steps.len(), 1);
        let event = serde_json::to_value(&steps[0].event).unwrap();
        assert_eq!(event["outcome"], "failure");
        assert_eq!(
            event["diagnostic"],
            lash_sansio::session_model::truncate_raw_error(&diagnostic)
        );
        assert!(event["diagnostic"].as_str().unwrap().chars().count() < 4100);
    });
}
