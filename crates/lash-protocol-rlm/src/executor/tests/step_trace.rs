use super::*;

#[derive(Default)]
struct StepSink(Mutex<Vec<lash_core::facade_support::TraceRecord>>);

impl TraceSink for StepSink {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        self.0.lock().unwrap().push(record.clone());
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
    let executions = Arc::new(AtomicUsize::new(0));
    let ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
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
    let response = execute_code_unbounded_for_tests(
        &mut RlmExecutionState::new(),
        ctx,
        ExecRequest {
            language: "lashlang".into(),
            code: code.into(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
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
    let records = sink.0.lock().unwrap().clone();
    (response, records)
}

#[test]
fn rejected_deferred_contract_step_is_visible_in_trace_sink() {
    block_on(async {
        let (response, records) = Box::pin(run_step("inbox.send_item({})")).await;
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
        let (response, records) = Box::pin(run_step("finish 42")).await;
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
        let code = format!("finish missing_{}", "x".repeat(8000));
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
