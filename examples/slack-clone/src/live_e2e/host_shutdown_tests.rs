use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct ShutdownOrderWitness {
    turn_joined: Arc<AtomicBool>,
    shutdown_called: Arc<AtomicBool>,
}

#[async_trait]
impl lash::plugins::PluginFactory for ShutdownOrderWitness {
    fn id(&self) -> &'static str {
        "slack_live_e2e_shutdown_order_witness"
    }

    fn build(
        &self,
        _ctx: &lash::plugins::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError>
    {
        Ok(Arc::new(ShutdownOrderSession))
    }

    async fn shutdown(&self) -> std::result::Result<(), lash::plugins::PluginError> {
        if !self.turn_joined.load(Ordering::SeqCst) {
            return Err(lash::plugins::PluginError::Session(
                "factory shutdown ran before the timed-out TurnStream was joined".to_string(),
            ));
        }
        self.shutdown_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct ShutdownOrderSession;

impl lash::plugins::SessionPlugin for ShutdownOrderSession {
    fn id(&self) -> &'static str {
        "slack_live_e2e_shutdown_order_witness"
    }

    fn register(
        &self,
        _registrar: &mut lash::plugins::PluginRegistrar,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        Ok(())
    }
}

struct CountingEchoTool {
    executed: Arc<AtomicUsize>,
    all_executed: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl StaticToolExecute for CountingEchoTool {
    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        if self.executed.fetch_add(1, Ordering::SeqCst) + 1 == 256 {
            self.all_executed.notify_one();
        }
        ToolOutcome::ok(json!({"value": "ok"}))
    }
}

#[tokio::test]
async fn smoke_stream_timeout_drains_full_channel_before_factory_shutdown() {
    let provider_entered = Arc::new(tokio::sync::Notify::new());
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::clone(&provider_entered);
    let calls = Arc::clone(&provider_calls);
    let provider = lash::testing::TestProvider::builder()
        .kind("slack-live-e2e-full-channel")
        .complete(move |_request| {
            let entered = Arc::clone(&entered);
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                entered.notify_one();
                if call == 0 {
                    Ok(LlmResponse {
                        parts: (0..256)
                            .map(|index| lash::direct::LlmOutputPart::ToolCall {
                                call_id: format!("full-channel-{index}"),
                                tool_name: "structural_echo".to_string(),
                                input_json: json!({"value": index.to_string()}).to_string(),
                                replay: None,
                            })
                            .collect(),
                        ..LlmResponse::default()
                    })
                } else {
                    Ok(LlmResponse {
                        parts: vec![lash::direct::LlmOutputPart::Text {
                            text: "done".to_string(),
                            response_meta: None,
                        }],
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let executed = Arc::new(AtomicUsize::new(0));
    let all_executed = Arc::new(tokio::sync::Notify::new());
    let tools = Arc::new(StaticToolProvider::new(
        vec![ToolDefinition::typed::<EchoArgs, EchoOutput>(
            "tool:slack_clone.structural_echo",
            "structural_echo",
            "Return the supplied value unchanged.",
        )],
        CountingEchoTool {
            executed: Arc::clone(&executed),
            all_executed: Arc::clone(&all_executed),
        },
    )) as Arc<dyn ToolProvider>;
    let turn_joined = Arc::new(AtomicBool::new(false));
    let shutdown_called = Arc::new(AtomicBool::new(false));
    let witness = Arc::new(ShutdownOrderWitness {
        turn_joined: Arc::clone(&turn_joined),
        shutdown_called: Arc::clone(&shutdown_called),
    });
    let temp = tempfile::tempdir().expect("temporary trace directory");
    let core = standard_core(
        provider,
        model_spec(DEFAULT_STANDARD_MODEL, 128).expect("model metadata"),
        128,
        2,
        "Exercise activity-channel backpressure.",
        Some(tools),
        temp.path().join("trace.jsonl"),
        Some(witness),
    )
    .expect("build core");
    let session = core
        .session("slack-live-e2e-full-channel")
        .open()
        .await
        .expect("open session");
    let stream = session
        .turn(TurnInput::text("fill activity channel"))
        .stream()
        .expect("start stream");
    provider_entered.notified().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), all_executed.notified())
            .await
            .is_err(),
        "all tools executed without a receiver, so the bounded channel did not backpressure"
    );

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        finish_smoke_stream_with_timeout(&session, stream, Duration::ZERO),
    )
    .await
    .expect("timeout cleanup remained finite");
    assert!(matches!(result, Err(FailureReason::TurnTimedOut { .. })));
    turn_joined.store(true, Ordering::SeqCst);
    finish_live_core(&core, "full-channel-test", Ok(()))
        .await
        .expect("shutdown after joined turn");
    assert!(shutdown_called.load(Ordering::SeqCst));
    assert_eq!(
        executed.load(Ordering::SeqCst),
        256,
        "draining did not release the full backpressured tool batch"
    );
}
