//! Production-adapter scenarios for the completion epilogue contract.
use crate::{LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_core::{ProviderFailureKind, facade_support::LlmTransportError};
use lash_sansio::llm::types::{LlmResponse, LlmTerminalReason};

/// Observable outcomes that constrain completion-check ordering.
#[derive(Clone, Copy, Debug)]
pub enum EpilogueScenario {
    /// An upstream HTTP error must survive later validation.
    HttpFailure,
    /// Empty content without required terminal evidence is truncation.
    EmptyTruncated,
    /// Partial text survives a missing-terminal failure.
    TextTruncated,
    /// A terminal output cap can legitimately produce no content.
    EmptyOutputLimit,
}

/// Drive real adapters: terminal-evidence failure wins over an empty response;
/// a terminal output cap remains an output cap even when no text was produced.
pub async fn completion_epilogue_conformance<F, Fut>(mut complete: F)
where
    F: FnMut(EpilogueScenario) -> Fut,
    Fut: std::future::Future<Output = Result<LlmResponse, LlmTransportError>>,
{
    for scenario in [
        EpilogueScenario::HttpFailure,
        EpilogueScenario::EmptyTruncated,
        EpilogueScenario::TextTruncated,
        EpilogueScenario::EmptyOutputLimit,
    ] {
        let result = complete(scenario).await;
        match scenario {
            EpilogueScenario::HttpFailure => {
                let error =
                    result.expect_err("HTTP failure must precede terminal and content checks");
                assert_eq!(
                    error.status,
                    Some(503),
                    "upstream HTTP evidence must survive the epilogue"
                );
            }
            EpilogueScenario::EmptyTruncated | EpilogueScenario::TextTruncated => {
                let error = result
                    .expect_err("missing terminal evidence must fail before content validation");
                assert_eq!(
                    error.kind,
                    ProviderFailureKind::Stream,
                    "{scenario:?}: {error:?}"
                );
                let partial = error
                    .partial_response
                    .expect("truncation retains a partial response even when empty");
                let expected = match scenario {
                    EpilogueScenario::TextTruncated => "partial",
                    _ => "",
                };
                assert_eq!(partial.full_text(), expected, "{scenario:?}");
            }
            EpilogueScenario::EmptyOutputLimit => {
                let response =
                    result.expect("terminal output cap must precede empty-content validation");
                assert_eq!(response.terminal_reason, LlmTerminalReason::OutputLimit);
                assert_eq!(response.full_text(), "");
            }
        }
    }
}

/// Single-response HTTP fixture, selectable as buffered or streamed transport.
#[derive(Debug)]
pub struct EpilogueTransport {
    body: String,
    streamed: bool,
    status: u16,
}

impl EpilogueTransport {
    /// Build a deterministic response for one scenario and transport mode.
    pub fn new(body: impl Into<String>, streamed: bool, scenario: EpilogueScenario) -> Self {
        Self {
            body: body.into(),
            streamed,
            status: if matches!(scenario, EpilogueScenario::HttpFailure) {
                503
            } else {
                200
            },
        }
    }
}

#[derive(Debug)]
struct OneChunk(Option<bytes::Bytes>);

#[async_trait::async_trait]
impl LlmByteStream for OneChunk {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        Ok(self.0.take())
    }
}

#[async_trait::async_trait]
impl LlmHttpTransport for EpilogueTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        Ok(LlmHttpResponse {
            status: self.status,
            headers: vec![(
                "content-type".into(),
                if self.streamed {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .into(),
            )],
            body: if self.streamed {
                LlmHttpBody::streamed(OneChunk(Some(bytes::Bytes::from(self.body.clone()))))
            } else {
                LlmHttpBody::buffered(self.body.clone())
            },
        })
    }
}
