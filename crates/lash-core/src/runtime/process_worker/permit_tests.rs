use super::*;

/// Dropping a future that parked the run's execution permit leaves the slot
/// released: the reacquisition sits after the `release_while` await and a
/// cancelled await never reaches it. Any resumption path that continues the
/// same process run afterwards — the tool-batch cancel grace timeout and
/// the cancelled background-session turn — must therefore reacquire the
/// slot explicitly before it resumes.
#[tokio::test]
async fn resuming_after_a_dropped_permit_release_reacquires_the_slot() {
    let semaphore = Arc::new(Semaphore::new(1));
    let permit = Arc::clone(&semaphore)
        .acquire_owned()
        .await
        .expect("initial process execution permit");
    let execution_permit = Arc::new(ProcessExecutionPermit::new(
        Arc::clone(&semaphore),
        permit,
        Arc::new(tokio::sync::Notify::new()),
    ));

    PROCESS_EXECUTION_PERMIT
        .scope(execution_permit, async move {
            assert_eq!(semaphore.available_permits(), 0, "the run holds its slot");

            let (parked_tx, mut parked_rx) = tokio::sync::mpsc::channel::<()>(1);
            let mut parked = Box::pin(release_process_execution_permit_while(async move {
                let _ = parked_tx.send(()).await;
                std::future::pending::<()>().await
            }));
            tokio::select! {
                _ = parked_rx.recv() => {}
                () = parked.as_mut() => panic!("the parked future must not complete"),
            }
            assert_eq!(
                semaphore.available_permits(),
                1,
                "parking the run releases its slot"
            );

            // The cancellation: the parked future is dropped.
            drop(parked);
            assert_eq!(
                semaphore.available_permits(),
                1,
                "a dropped release never reaches its own reacquisition"
            );

            ensure_process_execution_permit().await;
            assert_eq!(
                semaphore.available_permits(),
                0,
                "a resumed run must hold its slot again"
            );
        })
        .await;
}

/// The production cancel-grace path end to end: a tool parks the run's
/// execution slot, turn cancellation fires, the 50 ms grace expires and the
/// tool future is dropped — and the turn then continues. Without the
/// reacquisition in `execute_prepared_tool_batch` the run finishes the turn
/// holding no permit, overrunning the worker's execution concurrency.
#[tokio::test]
async fn cancelled_tool_batch_reacquires_the_process_execution_permit() {
    use crate::runtime::tests::helpers::{
        MockCall, mock_provider, named_turn_scope, runtime_with_plugins_and_tools,
    };

    struct PermitParkingTool {
        started: tokio::sync::mpsc::Sender<()>,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for PermitParkingTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![park_permit_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "park_permit").then(|| Arc::new(park_permit_tool_definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
            let _ = self.started.send(()).await;
            // Exactly what awaiting a nested process does: park the run's
            // execution slot for the duration of the wait.
            release_process_execution_permit_while(std::future::pending::<()>()).await;
            unreachable!("the parked tool never completes")
        }
    }

    fn park_permit_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:park_permit",
            "park_permit",
            "park the run's process execution permit forever",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": false }),
        )
    }

    let semaphore = Arc::new(Semaphore::new(1));
    let permit = Arc::clone(&semaphore)
        .acquire_owned()
        .await
        .expect("initial process execution permit");
    let execution_permit = Arc::new(ProcessExecutionPermit::new(
        Arc::clone(&semaphore),
        permit,
        Arc::new(tokio::sync::Notify::new()),
    ));

    PROCESS_EXECUTION_PERMIT
        .scope(execution_permit, async move {
            let transport = mock_provider(vec![
                MockCall {
                    stream_events: vec![crate::llm::types::LlmStreamEvent::Part(
                        crate::LlmOutputPart::ToolCall {
                            call_id: "park-1".to_string(),
                            tool_name: "park_permit".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        },
                    )],
                    response: Ok(crate::LlmResponse::default()),
                },
                // Safety net: the cancelled turn should not call the
                // provider again, but a second call must not panic the mock.
                MockCall {
                    stream_events: Vec::new(),
                    response: Ok(crate::LlmResponse::default()),
                },
            ]);
            let (started_tx, mut started_rx) = tokio::sync::mpsc::channel::<()>(1);
            let tools: Arc<dyn crate::ToolProvider> = Arc::new(PermitParkingTool {
                started: started_tx,
            });
            let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;

            let cancel = tokio_util::sync::CancellationToken::new();
            let cancel_trigger = cancel.clone();
            crate::task::spawn(async move {
                // Cancel only once the tool has actually parked the slot.
                let _ = started_rx.recv().await;
                cancel_trigger.cancel();
            });

            let turn = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                runtime.run_turn_assembled(
                    crate::TurnInput::text("park the run's permit"),
                    cancel,
                    named_turn_scope("root", "permit-cancel-grace-turn"),
                ),
            )
            .await
            .expect("cancelled turn must finish");
            assert!(turn.is_ok(), "cancelled turn: {turn:?}");

            assert_eq!(
                semaphore.available_permits(),
                0,
                "the run must hold its execution slot again after the cancel grace \
                 dropped the parked tool"
            );
        })
        .await;
}
