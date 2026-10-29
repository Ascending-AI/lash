//! Shared HTTP progress observation for every model provider.
use crate::{LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_core::provider::ProviderAttemptControl;
use lash_transport_failure::LlmTransportError;
use std::time::Duration;

pub async fn send_observed(
    transport: &dyn LlmHttpTransport,
    request: LlmHttpRequest,
    timeout: Option<Duration>,
    attempt: &ProviderAttemptControl,
) -> Result<LlmHttpResponse, LlmTransportError> {
    attempt.begin_transport();
    let mut response = transport.send(request, timeout).await?;
    attempt.observe_response_headers(response.status);
    response.body = match response.body {
        LlmHttpBody::Buffered(bytes) => {
            attempt.observe_body_chunk(bytes.len());
            LlmHttpBody::Buffered(bytes)
        }
        LlmHttpBody::Streamed(inner) => LlmHttpBody::Streamed(Box::new(ObservedBody {
            inner,
            attempt: attempt.clone(),
        })),
    };
    Ok(response)
}

#[derive(Debug)]
struct ObservedBody {
    inner: Box<dyn LlmByteStream>,
    attempt: ProviderAttemptControl,
}

#[async_trait::async_trait]
impl LlmByteStream for ObservedBody {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        let chunk = self.inner.next_chunk().await?;
        if let Some(bytes) = &chunk {
            self.attempt.observe_body_chunk(bytes.len());
        }
        Ok(chunk)
    }
}
