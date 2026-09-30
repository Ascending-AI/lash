use crate::ingress::*;
use bytes::Bytes;
use lash_http_transport::{
    ByteStream, HttpFailureContext, HttpResponse, HttpResponseBody, HttpTransport,
    LlmTransportError,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct FixtureTransport {
    status: u16,
    length: Option<&'static str>,
    streamed: bool,
    polls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct FixtureStream {
    chunks: VecDeque<Bytes>,
    polls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ByteStream for FixtureStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self.chunks.pop_front())
    }
}

#[async_trait::async_trait]
impl HttpTransport for FixtureTransport {
    async fn send(
        &self,
        _: lash_http_transport::HttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let body = if self.streamed {
            HttpResponseBody::streamed(FixtureStream {
                chunks: [Bytes::from_static(b"{\"n\":42"), Bytes::from_static(b"}")].into(),
                polls: self.polls.clone(),
            })
        } else {
            HttpResponseBody::buffered("{\"n\":42}")
        };
        Ok(HttpResponse {
            status: self.status,
            headers: self
                .length
                .map(|length| vec![("content-length".into(), length.into())])
                .unwrap_or_default(),
            body,
        })
    }
}

#[tokio::test]
async fn response_body_budget_restate_ingress_success_errors_and_deadline_classes() {
    for status in [200, 503] {
        for streamed in [false, true] {
            for length in [None, Some("0"), Some("100000000000")] {
                for limit in [7, 8] {
                    for attach in [false, true] {
                        let polls = Arc::new(AtomicUsize::new(0));
                        let connection = RestateConnection::with_transport_and_config(
                            "https://restate.test",
                            Arc::new(FixtureTransport {
                                status,
                                length,
                                streamed,
                                polls: polls.clone(),
                            }),
                            RestateConnectionConfig {
                                response_body_bytes: limit,
                                ..RestateConnectionConfig::default()
                            },
                        );
                        let client = RestateIngressClient::new(connection);
                        let result: Result<serde_json::Value, _> = if attach {
                            client.call_workflow_json("flow", "key", "run", &()).await
                        } else {
                            client.call_object_json("object", "key", "read", &()).await
                        };
                        if limit == 7 {
                            let error = result
                                .expect_err("ingress must refuse limit+1 before JSON decoding");
                            assert_eq!(error.classification(), RestateHttpErrorClass::Terminal);
                            let RestateHttpError::Request { source, .. } = error else {
                                panic!("typed response refusal required");
                            };
                            assert!(
                                matches!(
                                    source.context.as_ref(),
                                    HttpFailureContext::ResponseBodyTooLarge {
                                        limit: 7,
                                        received_at_least: 8
                                    }
                                ),
                                "{source:?}"
                            );
                            if streamed {
                                assert_eq!(polls.load(Ordering::SeqCst), 2);
                            }
                        } else if status == 200 {
                            assert_eq!(result.unwrap(), serde_json::json!({"n":42}));
                        } else {
                            let error = result.unwrap_err();
                            assert_eq!(error.classification(), RestateHttpErrorClass::Transient);
                            assert!(
                                matches!(error, RestateHttpError::Status { status: 503, body, .. } if body == "{\"n\":42}")
                            );
                        }
                    }
                }
            }
        }
    }
}
