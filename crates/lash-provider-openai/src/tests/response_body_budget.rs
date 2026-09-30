use super::*;
use lash_llm_transport::{HttpFailureContext, LlmByteStream, LlmHttpResponse};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct BodyTransport {
    status: u16,
    headers: Vec<(String, String)>,
    bytes: bytes::Bytes,
    streamed: bool,
    polls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct BodyStream {
    chunks: VecDeque<bytes::Bytes>,
    polls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmByteStream for BodyStream {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self.chunks.pop_front())
    }
}

#[async_trait]
impl LlmHttpTransport for BodyTransport {
    async fn send(
        &self,
        _: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let body = if self.streamed {
            LlmHttpBody::streamed(BodyStream {
                chunks: [
                    self.bytes.slice(..self.bytes.len() - 1),
                    self.bytes.slice(self.bytes.len() - 1..),
                ]
                .into(),
                polls: self.polls.clone(),
            })
        } else {
            LlmHttpBody::buffered(self.bytes.clone())
        };
        Ok(LlmHttpResponse {
            status: self.status,
            headers: self.headers.clone(),
            body,
        })
    }
}

#[tokio::test]
async fn response_body_budget_preserves_provider_success_and_error_boundaries() {
    for endpoint in [
        CompletionEndpoint::Responses,
        CompletionEndpoint::ChatCompletions,
    ] {
        let success = match endpoint {
            CompletionEndpoint::Responses => {
                r#"{"id":"resp-budget","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]}"#
            }
            CompletionEndpoint::ChatCompletions => {
                r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#
            }
        };
        for (status, body) in [
            (200, success),
            (429, r#"{"error":{"message":"unavailable"}}"#),
        ] {
            for streamed in [false, true] {
                for length in [None, Some("0"), Some("999999999999")] {
                    for excess in [false, true] {
                        let limit = body.len() - usize::from(excess);
                        let polls = Arc::new(AtomicUsize::new(0));
                        let transport = BodyTransport {
                            status,
                            headers: length
                                .map(|length| vec![("content-length".into(), length.into())])
                                .unwrap_or_default(),
                            bytes: bytes::Bytes::from_static(body.as_bytes()),
                            streamed,
                            polls: polls.clone(),
                        };
                        let mut provider =
                            OpenAiCompatibleProvider::new("key", "https://example.test/v1")
                                .with_options(ProviderOptions {
                                    response_body_bytes: Some(limit as u64),
                                    ..ProviderOptions::default()
                                })
                                .with_transport(Arc::new(transport));
                        let result = crate::driver::complete(
                            &mut provider,
                            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
                            endpoint,
                        )
                        .await;
                        if excess {
                            let error =
                                result.expect_err("limit+1 must refuse before provider parsing");
                            assert!(
                                matches!(error.context.as_ref(), HttpFailureContext::ResponseBodyTooLarge { limit: actual, received_at_least } if *actual == limit && *received_at_least == body.len()),
                                "{error:?}"
                            );
                            assert_eq!(
                                error.code.as_ref().map(ToString::to_string).as_deref(),
                                Some("lash:http_response_body_too_large")
                            );
                            assert!(!error.is_retryable());
                            if status != 200 {
                                assert_eq!(error.http_status, Some(status));
                            }
                            if streamed {
                                assert_eq!(polls.load(Ordering::SeqCst), 2);
                            }
                        } else if status == 200 {
                            assert_eq!(result.unwrap().full_text(), "done");
                        } else {
                            let error = result.unwrap_err();
                            assert_eq!(error.http_status, Some(status));
                            assert!(error.message.contains("unavailable"));
                            assert!(matches!(error.context.as_ref(), HttpFailureContext::Other));
                        }
                    }
                }
            }
        }
    }
}
