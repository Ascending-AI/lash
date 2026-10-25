//! A retry already inside its reservation must settle before the enclosing outcome.
use super::*;
use lash_core::runtime::durable::session::SessionActivation;
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

struct NestedCompletion;
#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for NestedCompletion {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let result = call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text("nested retry"),
                "nested-retry-law",
            )
            .await;
        match result {
            Ok(_) => lash_core::ToolOutcome::ok(serde_json::json!({"done":true})).into(),
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

#[tokio::test]
async fn a_nested_retry_reservation_settles_with_its_cancelled_tool_outcome() {
    let clock = SimClock::new();
    sim::wait_out_renders(&clock);
    let keep = std::sync::Mutex::new(Vec::new());
    let (stores, database) =
        dialect::open(dialect::Dialect::SqliteMemory, None, clock.clone(), &keep).await;
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let sent = calls.clone();
    let mut options = lash_core::facade_support::ProviderOptions::default();
    options.reliability.retry.max_attempts = Some(2);
    options.reliability.retry.base_delay_ms = 0;
    options.reliability.retry.max_delay_ms = 0;
    options.reliability.retry.jitter_ms = 0;
    let provider = lash_core::testing::TestProvider::builder()
        .kind("nested-retry-law")
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(options)
        .complete(move |request: lash_core::LlmRequest| {
            let sent = sent.clone();
            async move {
                if serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains("nested retry")
                {
                    sent.fetch_add(1, Ordering::SeqCst);
                    return Err(lash::provider::LlmTransportError::new("retry nested call")
                        .with_kind(lash_core::ProviderFailureKind::Transport)
                        .with_headers([("x-request-id", "nested-retry-failed")])
                        .with_retry_verdict(
                            lash::provider::TransportRetryVerdict::RetryableTransient,
                        ));
                }
                Ok(lash_core::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::ToolCall {
                        call_id: "nested-tool".into(),
                        tool_name: "tools__nested".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        "nested",
        "tools.nested",
        "Nested completion",
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"object","properties":{"done":{"type":"boolean"}},"required":["done"]}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(120));
    let core = lash::LashCore::standard_builder(backend.clone())
        .serve_sessions(false)
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .data_retention(lash::DataRetention::standard())
        .serve_test_llm_profile(provider, served::metadata())
        .plugin(packages::tools(
            lash::tool_provider_key!("tools"),
            Arc::new(lash::tools::StaticToolProvider::new(
                vec![definition],
                NestedCompletion,
            )),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("nested-retry"),
            lash::persistence::LeaseIncarnationId::new("boot"),
        ))
        .unwrap();
    let entered = Arc::new(Semaphore::new(0));
    let settling = Arc::new(Semaphore::new(0));
    let gate = entered.clone();
    let activation = SessionActivation::new(
        backend.clone(),
        lash::testing::session_turn_services(&core),
        Arc::new(Tripwire::default()),
    )
    .observe_context_for_testing(move |cx| {
        cx.interleave_model_retry_for_testing(gate.clone(), settling.clone())
    });
    let nodes = SimNodes::new(
        database,
        clock,
        Script::new(),
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(&core),
            max_active: 1,
        },
        Arc::new(activation),
    );
    nodes.start("owner");
    let session = core
        .session(lash::SessionId::from("nested-retry"))
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .unwrap();
    let handle = session
        .send(lash::TurnInput::text("call the nested tool"))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if let Ok(permit) = entered.try_acquire() {
                permit.forget();
                break;
            }
            nodes.step().await;
        }
    })
    .await
    .expect("the nested provider must enter its retry reservation");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    eprintln!("nested retry is inside its reservation; cancelling the enclosing tool");
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        handle
            .cancel()
            .mode(lash::TurnCancelMode::Immediate)
            .await
            .unwrap();
        for _ in 0..20 {
            nodes.step().await;
            if nodes
                .database()
                .turn(&lash::SessionId::from("nested-retry"))
                .await
                .unwrap()
                .is_none()
            {
                break;
            }
        }
        let output = handle.output().await.unwrap();
        assert_eq!(
            lash::TurnStatus::of_outcome(&output.outcome),
            lash::TurnStatus::Cancelled
        );
        let run = output
            .activities
            .iter()
            .find_map(|activity| match &activity.event {
                lash::TurnEvent::TurnStarted { run_id, .. } => Some(run_id.turn_id().clone()),
                _ => None,
            })
            .unwrap();
        let scope = lash_core::ExecutionScope::turn(session.session_id().clone(), run);
        let owner = format!("owned:{}", scope.id());
        let calls = nodes
            .database()
            .model_calls(session.session_id(), &owner)
            .await
            .unwrap();
        assert!(
            calls
                .iter()
                .any(|call| call.record.attempts.iter().any(|attempt| attempt
                    .error
                    .as_ref()
                    .is_some_and(
                        |error| error.provider_request_id.as_deref() == Some("nested-retry-failed")
                    ))),
            "the enclosing outcome must persist the sealed nested call: {calls:?}"
        );
    })
    .await
    .expect(
        "nested reservation and cancelled outcome must not wait on each other's projection lock",
    );
    nodes.stop("owner");
    nodes.quiesce().await;
}
