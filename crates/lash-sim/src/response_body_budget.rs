//! HTTP byte refusals survive the current Restate turn path over each store.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use lash_core::facade_support::LlmTransportError;
use lash_core::provider::{ProviderHandle, ProviderOptions};
use lash_llm_transport::{
    HttpFailureContext, LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse,
    LlmHttpTransport, read_http_body_bytes,
};
use lash_provider_openai::OpenAiCompatibleProvider;
use lash_restate::{
    RestateAdminClient, RestateConnection, RestateConnectionConfig, RestateHttpError,
    RestateHttpErrorClass, RestateIngressClient,
};

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

async fn witness(
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    lane: &str,
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
                            .max_attempts(2)
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
                let core = lash::LashCore::standard_builder(double.lash_backend())
                    .serve_test_llm_profile(
                        ProviderHandle::new(provider.into_components()),
                        lash::LlmProfileMetadata::builder("budget-model")
                            .context_window_tokens(16_000)
                            .build()
                            .unwrap(),
                    )
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
                    .build(crate::sim_process_owner())
                    .unwrap();
                let session_id = format!("{lane}-{streamed}-{status}-{excess}");
                let session = crate::open_created_session(
                    "budget-model",
                    &core,
                    lash_core::SessionId::fixture(session_id),
                )
                .await
                .unwrap();
                let output = session
                    .send(lash::TurnInput::text("hello"))
                    .output()
                    .await
                    .unwrap();
                if !excess && status == 200 {
                    assert!(output.is_success(), "{lane}: {:?}", output.result.errors);
                    assert_eq!(output.assistant_message(), Some("done"));
                } else {
                    assert!(!output.is_success(), "{lane} accepted a refused response");
                    let expected = if excess {
                        "lash:http_response_body_too_large"
                    } else {
                        "lash:provider_http_error"
                    };
                    if excess {
                        assert_eq!(
                            calls.load(Ordering::SeqCst),
                            1,
                            "oversize must stop the configured retry ladder"
                        );
                        assert!(
                            output.result.errors.iter().any(|error| error
                                .code
                                .as_ref()
                                .is_some_and(|code| code.to_string() == expected)),
                            "{lane}: {:?}",
                            output.result.errors
                        );
                    }
                    assert_eq!(
                        output.result.llm_calls.len(),
                        1,
                        "{lane}: refusal must not retry"
                    );
                }
            }
        }
    }

    // The native Restate admin and ingress clients reach this same server.
    // Probe a stable empty query and a nonexistent handler to get exact wire
    // sizes, then repeat each through its production decoding/error path.
    let server = double.server();
    let transport = server.transport();
    let url = server.ingress_url();
    let query = "SELECT id FROM sys_invocation WHERE id = 'missing-budget-witness'";
    let query_body = serde_json::to_vec(&serde_json::json!({"query":query})).unwrap();
    let response = transport
        .send(
            LlmHttpRequest::post(format!("{url}/query"), query_body),
            None,
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    let query_bytes = read_http_body_bytes(response.body, 16 * 1024 * 1024, None, "query witness")
        .await
        .unwrap()
        .len();
    let response = transport
        .send(
            LlmHttpRequest::post(format!("{url}/MissingBudgetService/handler/send"), "null"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(response.status, 404);
    let error_bytes = read_http_body_bytes(response.body, 16 * 1024 * 1024, None, "error witness")
        .await
        .unwrap()
        .len();
    for excess in [false, true] {
        let connection = RestateConnection::with_transport_and_config(
            url,
            transport.clone(),
            RestateConnectionConfig {
                response_body_bytes: query_bytes - usize::from(excess),
                ..Default::default()
            },
        );
        let result = RestateAdminClient::new(connection)
            .query_json::<serde_json::Value>(query)
            .await;
        if excess {
            assert_refusal(result.unwrap_err());
        } else {
            assert!(result.unwrap().is_empty());
        }
        let connection = RestateConnection::with_transport_and_config(
            url,
            transport.clone(),
            RestateConnectionConfig {
                response_body_bytes: error_bytes - usize::from(excess),
                ..Default::default()
            },
        );
        let error = RestateIngressClient::new(connection)
            .send_json_path("MissingBudgetService/handler/send", &())
            .await
            .unwrap_err();
        if excess {
            assert_refusal(error);
        } else {
            assert!(matches!(
                error,
                RestateHttpError::Status { status: 404, .. }
            ));
        }
    }
}

fn assert_refusal(error: RestateHttpError) {
    assert_eq!(error.classification(), RestateHttpErrorClass::Terminal);
    let RestateHttpError::Request { source, .. } = error else {
        panic!("typed refusal required: {error:?}");
    };
    assert!(matches!(
        source.context.as_ref(),
        HttpFailureContext::ResponseBodyTooLarge { .. }
    ));
}

#[tokio::test]
async fn response_body_budget_current_turn_path_sqlite_memory_and_file() {
    let config = lash_restate_test::ServerConfig::default();
    let memory = lash_restate_test::backend(0x4291, config.clone())
        .await
        .unwrap()
        .erase_store_type();
    witness(memory, "sqlite-memory").await;
    let root = tempfile::tempdir().unwrap();
    let file = lash_restate_test::backend_with_store_set(
        0x4292,
        config.clone(),
        lash_restate_test::DeploymentHooks::default(),
        |clock| async {
            Ok(Arc::new(
                lash_sqlite_store::SqliteStoreSet::open_with_clock(
                    root.path().join("lash.db"),
                    clock,
                )
                .await
                .unwrap(),
            ) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .unwrap();
    witness(file, "sqlite-file").await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; select inside a pg16 gate"]
async fn response_body_budget_current_turn_path_postgres() {
    let config = lash_restate_test::ServerConfig::default();
    let database = crate::postgres_test_isolation::isolated_database().await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .unwrap();
    let attachments = tempfile::tempdir().unwrap();
    let postgres = lash_restate_test::backend_with_store_set(
        0x4293,
        config,
        lash_restate_test::DeploymentHooks::default(),
        |clock| async {
            Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    attachments.path(),
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock,
            )) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .unwrap();
    witness(postgres, "postgres").await;
}

#[test]
fn postgres_variants_never_pass_without_a_database_url() {
    crate::postgres_test_isolation::assert_requires_database_url(
        "response_body_budget::response_body_budget_current_turn_path_postgres",
    );
}
