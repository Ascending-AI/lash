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

#[tokio::test]
async fn rlm_uses_runtime_scope_without_suppressing_product_replay() {
    let exported = Arc::new(StepSink::default());
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_123));
    let runtime = lash_core::trace::TraceRuntime::new(clock)
        .with_trace_sink(exported.clone())
        .with_product_observer(graphs.clone());
    let scope = lash_trace::DurableTraceScope {
        scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
            session_id: "trace-session".into(),
            turn_id: "trace-turn".into(),
        }),
        cause: lash_trace::TraceCause::Root,
        anchor: lash_trace::TraceAnchor::Untraced,
        started_at_ms: 1_700_000_000_000,
    };
    let controller = lash_core::ActorContext::unavailable()
        .scoped(lash_core::AdmittedScope::turn(
            "trace-session",
            "trace-turn",
        ))
        .expect("the fixture's turn controller");
    let context = |standing| {
        lash_core::testing::TestExecutionContextBuilder::over_controller(controller.clone())
            .runtime_parent_invocation(lash_core::testing::exec_code_invocation(
                "trace-session",
                "trace-turn",
                2,
                7,
                "trace-exec",
                "exec:trace",
            ))
            .build()
            .into_runtime()
            .with_trace_standing(standing)
    };
    let artifact =
        worker_compile_program(&lash_typescript::parse("finish(42);").expect("fixture program"))
            .await
            .expect("fixture artifact")
            .artifact;
    let live = context(runtime.unreplayed(Some(scope.clone())));
    let trace = foreground_lashlang_execution_trace(&live, &artifact, "typescript")
        .expect("the runtime observes the language");
    assert_eq!(live.trace_scope(), Some(&scope));
    emit_foreground_execution_started(&trace, &artifact);
    trace.emit(TraceLanguageExecution {
        event_key: trace.event_key("finished"),
        identity: trace.identity().clone(),
        payload: TraceLanguageExecutionPayload::ExecutionFinished {
            status: TraceLanguageExecutionStatus::Completed,
            error: None,
        },
    });
    let original = graphs.graphs();
    assert_eq!(original.len(), 1);
    assert!(original[0].conflicts.is_empty());
    let records = exported.records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].id, records[1].id);
    assert!(
        records
            .iter()
            .all(|record| record.timestamp.timestamp_millis() == 1_700_000_000_123)
    );
    graphs.clear();
    let replay = context(runtime.shift(Some(scope.clone()), &controller));
    let trace = foreground_lashlang_execution_trace(&replay, &artifact, "typescript")
        .expect("product observation stays enabled on replay");
    assert_eq!(replay.trace_scope(), Some(&scope));
    emit_foreground_execution_started(&trace, &artifact);
    trace.emit(TraceLanguageExecution {
        event_key: trace.event_key("finished"),
        identity: trace.identity().clone(),
        payload: TraceLanguageExecutionPayload::ExecutionFinished {
            status: TraceLanguageExecutionStatus::Completed,
            error: None,
        },
    });
    assert_eq!(
        graphs.graphs(),
        original,
        "replay rebuilds the product graph"
    );
    assert_eq!(
        *exported.records.lock().unwrap(),
        records,
        "replay exports no lifecycle copies"
    );
}
