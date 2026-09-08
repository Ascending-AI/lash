//! Durable failed-generation evidence shared by every session-store backend.

use super::session_store_factory::session_store_request;
use std::sync::Arc;

/// Prove that a real mid-stream provider failure settles durable evidence that
/// survives closing the runtime and reopening through the backend's read view.
pub async fn session_store_factory_mid_stream_failure_evidence(
    factory: Arc<dyn crate::SessionStoreFactory>,
    advance_commit_clock: impl FnOnce(),
) {
    const SESSION_ID: &str = "failure-evidence-session";
    const PARTIAL_TEXT: &str = "provider-visible prefix before the stream failed";

    let request = session_store_request(
        SESSION_ID,
        "failure-evidence-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create failure-evidence store");
    let provider = crate::testing::TestProvider::builder()
        .kind("conformance-provider")
        .requires_streaming(true)
        .options(crate::ProviderOptions {
            reliability: crate::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..crate::ProviderOptions::default()
        })
        .complete(|request| async move {
            let stream = request.stream_events.expect("stream events");
            stream.send(lash_sansio::llm::types::LlmStreamEvent::Delta(
                PARTIAL_TEXT.to_string(),
            ));
            let usage = crate::llm::types::LlmUsage {
                input_tokens: 13,
                output_tokens: 5,
                ..Default::default()
            };
            stream.send(lash_sansio::llm::types::LlmStreamEvent::Usage(
                usage.clone(),
            ));
            Err(
                crate::LlmTransportError::new("stream ended before terminal evidence")
                    .with_kind(crate::ProviderFailureKind::Stream)
                    .with_code("stream_ended_before_terminal_response")
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::RetryableTransient,
                    )
                    .with_partial_response(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: PARTIAL_TEXT.to_string(),
                            response_meta: None,
                        }],
                        usage,
                        response_metadata: Default::default(),
                        ..Default::default()
                    }),
            )
        })
        .build()
        .into_handle();
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut host = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.control.effect_host = Arc::clone(&effect_host);
    host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(provider));
    let mut policy = request.policy.clone();
    policy.session_id = Some(SESSION_ID.to_string());
    let state = crate::RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut runtime = Box::pin(
        crate::LashRuntime::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            crate::testing::runtime_lease_owner(),
        )
        .with_session_id(SESSION_ID)
        .with_policy(policy)
        .with_initial_state(state)
        .with_runtime_host(host)
        .with_plugin_factories(crate::testing::test_standard_protocol_factories())
        .with_store(store)
        .build(),
    )
    .await
    .expect("build failure-evidence conformance runtime");
    let turn_id = "z-failure-evidence-turn-early";
    let scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, turn_id))
        .expect("scope failure-evidence conformance turn");
    let mut input = crate::TurnInput::text("trigger a paid mid-stream failure");
    input.trace_turn_id = Some(turn_id.to_string());
    let turn = runtime
        .stream_turn(
            input,
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect("provider failure returns a settled turn");
    assert_eq!(turn.failure_evidence.len(), 1);

    advance_commit_clock();
    let later_turn_id = "a-failure-evidence-turn-late";
    let later_scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, later_turn_id))
        .expect("scope later failure-evidence conformance turn");
    let mut later_input = crate::TurnInput::text("trigger a later paid mid-stream failure");
    later_input.trace_turn_id = Some(later_turn_id.to_string());
    let later_turn = runtime
        .stream_turn(
            later_input,
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), later_scope),
        )
        .await
        .expect("later provider failure returns a settled turn");
    assert_eq!(later_turn.failure_evidence.len(), 1);
    drop(runtime);

    let reopened = factory
        .read_session(SESSION_ID)
        .await
        .expect("reopen failure-evidence session")
        .expect("failed turn remains readable");
    let early_storage_key =
        crate::OperationId::new(crate::ExecutionScope::turn(SESSION_ID, turn_id), "final")
            .storage_key()
            .expect("early failure-evidence storage key");
    let later_storage_key = crate::OperationId::new(
        crate::ExecutionScope::turn(SESSION_ID, later_turn_id),
        "final",
    )
    .storage_key()
    .expect("later failure-evidence storage key");
    assert_eq!(
        reopened.turn_failure_settlements().len(),
        2,
        "both failed turns own durable settlement components"
    );
    assert_eq!(
        reopened
            .turn_failure_settlements()
            .iter()
            .map(|settlement| settlement.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![early_storage_key.as_str(), later_storage_key.as_str()],
        "settlements are ordered by committed timestamp before turn id"
    );
    let settlement = &reopened.turn_failure_settlements()[0];
    assert!(
        !settlement.turn_id.is_empty(),
        "the settlement names its owning turn"
    );
    assert_eq!(settlement.evidence, turn.failure_evidence);
    assert_eq!(settlement.evidence[0].billed_usage.output_tokens, 5);
    assert_eq!(
        settlement.evidence[0]
            .partial_output
            .as_ref()
            .map(crate::TurnFailurePartialOutput::text),
        Some(PARTIAL_TEXT)
    );
    assert_eq!(
        reopened.turn_failure_settlements()[1].evidence,
        later_turn.failure_evidence
    );
    assert!(
        reopened.messages().iter().all(|message| message
            .parts
            .iter()
            .all(|part| !part.content.contains(PARTIAL_TEXT))),
        "failure evidence remains outside the reopened transcript"
    );
}
