//! HTTP byte refusals hold on the durable turn path over SQLite memory and a
//! SQLite file: a provider response past its configured body budget fails
//! the turn with `http_response_body_too_large` and stops the retry ladder,
//! and a refused status within the budget fails it without a retry. The
//! budget is the transport's, so no other store tier adds to the proof.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use lash_core::provider::{ProviderHandle, ProviderOptions};
use lash_llm_transport::{
    LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport,
};
use lash_provider_openai::OpenAiCompatibleProvider;
use lash_transport_failure::LlmTransportError;

#[derive(Debug)]
struct ResponseTransport {
    status: u16,
    body: &'static str,
    streamed: bool,
    calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct ResponseStream(Option<Bytes>);

#[async_trait]
impl LlmByteStream for ResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        Ok(self.0.take())
    }
}

#[async_trait]
impl LlmHttpTransport for ResponseTransport {
    async fn send(
        &self,
        _: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bytes = Bytes::from_static(self.body.as_bytes());
        Ok(LlmHttpResponse {
            status: self.status,
            headers: Vec::new(),
            body: if self.streamed {
                LlmHttpBody::streamed(ResponseStream(Some(bytes)))
            } else {
                LlmHttpBody::buffered(bytes)
            },
        })
    }
}

/// What a witness asserts of a refused turn besides its refusal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Witness {
    /// The refusal and the transport attempts it spent.
    Refusal,
    /// Also its report: the typed code and the one model call it records.
    Report,
}

async fn witness(
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    lane: &str,
    witness: Witness,
) {
    for streamed in [false, true] {
        for status in [200, 429] {
            for excess in [false, true] {
                let body = if status == 200 {
                    r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#
                } else {
                    r#"{"error":{"message":"unavailable"}}"#
                };
                let calls = Arc::new(AtomicUsize::new(0));
                let provider = OpenAiCompatibleProvider::new("key", "https://provider.test/v1")
                    .with_options(ProviderOptions {
                        reliability: lash_core::provider::ProviderReliability::default()
                            .max_attempts(Some(2))
                            .base_delay_ms(0)
                            .max_delay_ms(0),
                        response_body_bytes: Some((body.len() - usize::from(excess)) as u64),
                        ..Default::default()
                    })
                    .with_transport(Arc::new(ResponseTransport {
                        status,
                        body,
                        streamed,
                        calls: calls.clone(),
                    }));
                let core = lash::LashCore::standard_builder(lash_conformance::backend_over(
                    Arc::clone(&stores),
                ))
                .serve_test_llm_profile(
                    ProviderHandle::new(provider.into_components()),
                    lash::LlmProfileMetadata::builder("budget-model")
                        .cache_retention(lash::provider::CacheRetention::Short)
                        .context_window_tokens(16_000)
                        .build()
                        .unwrap(),
                )
                .data_retention(lash::DataRetention::standard())
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
                .execution_budgets(lash::ExecutionBudgets::recommended())
                .delta_coalescing(lash::DeltaCoalescing::recommended())
                .build(crate::sim_process_owner())
                .unwrap();
                let session_id = format!("{lane}-{streamed}-{status}-{excess}");
                let session = crate::open_created_session(
                    "budget-model",
                    &core,
                    lash_kernel_value::identity::SessionId::fixture(session_id),
                )
                .await
                .unwrap();
                let output = session
                    .send(lash::TurnInput::text("hello"))
                    .output()
                    .await
                    .unwrap();
                if !excess && status == 200 {
                    assert!(
                        output.is_success(),
                        "{lane}: {:?}",
                        output.outcome.error_cause()
                    );
                    assert_eq!(output.assistant_message(), Some("done"));
                } else {
                    assert!(!output.is_success(), "{lane} accepted a refused response");
                    if excess {
                        assert_eq!(
                            calls.load(Ordering::SeqCst),
                            1,
                            "oversize must stop the configured retry ladder"
                        );
                    }
                    if witness == Witness::Report {
                        let output_model_calls = lash::testing::model_calls_for_activity(
                            &session.durable(),
                            &output.activities,
                        )
                        .await;
                        if excess {
                            let cause = output.outcome.error_cause().expect("typed terminal stop");
                            assert_eq!(
                                cause.provider_validation,
                                Some(lash::turn::TurnFailureCode::HttpResponseBodyTooLarge),
                                "{lane}: {cause:?}",
                            );
                        }
                        assert_eq!(
                            output_model_calls.len(),
                            1,
                            "{lane}: refusal must not retry"
                        );
                        let call = &output_model_calls[0];
                        assert!(output.activities.iter().any(|activity| matches!(
                            &activity.event,
                            lash::TurnEvent::ModelCallRecorded { call_id } if call_id == &call.call_id
                        )), "the output identifies its retained failed call");
                        if excess {
                            assert_eq!(call.attempts.len(), 1);
                        }
                        assert_eq!(call.attempts.len(), calls.load(Ordering::SeqCst));
                        for attempt in &call.attempts {
                            assert_eq!(attempt.outcome, lash::provider::AttemptOutcome::Failed);
                            if excess {
                                assert_eq!(
                                    attempt
                                        .error
                                        .as_ref()
                                        .and_then(|error| error.code.as_ref())
                                        .map(ToString::to_string)
                                        .as_deref(),
                                    Some("lash:http_response_body_too_large"),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn response_body_budget_current_turn_path_sqlite_memory_and_file() {
    let memory: Arc<dyn lash_execution_contracts::backend::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a memory store set"),
    );
    witness(memory, "sqlite-memory", Witness::Refusal).await;
    let root = tempfile::tempdir().unwrap();
    let file: Arc<dyn lash_execution_contracts::backend::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            root.path().join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open a file store set"),
    );
    witness(file, "sqlite-file", Witness::Refusal).await;
}

/// A refused response's report names its typed code, and records the one
/// model call the refusal spent.
#[tokio::test]
async fn a_refused_response_s_report_names_its_typed_code_on_sqlite_memory() {
    let memory: Arc<dyn lash_execution_contracts::backend::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a memory store set"),
    );
    witness(memory, "sqlite-memory", Witness::Report).await;
}
