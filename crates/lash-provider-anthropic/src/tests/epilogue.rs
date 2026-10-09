use super::*;
use lash_llm_transport::conformance::epilogue::{
    EpilogueScenario, EpilogueTransport, completion_epilogue_conformance,
};

#[tokio::test]
async fn anthropic_completion_epilogue_conformance() {
    completion_epilogue_conformance(|scenario| async move {
        let body = match scenario {
        EpilogueScenario::HttpFailure => r#"{"error":{"message":"upstream failed"}}"#,
            EpilogueScenario::EmptyTruncated => "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            EpilogueScenario::TextTruncated => concat!(
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            ),
            EpilogueScenario::EmptyOutputLimit => concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
        };
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model.metadata_mut().capability.stream_termination = Some(StreamTermination::RequireTerminalEvidence);
        req.stream_events = Some(LlmEventSender::new(|_| {}));
        AnthropicProvider::new("key").with_transport(Arc::new(EpilogueTransport::new(body, true, scenario))).complete(req, &lash_core::provider::NoSlotDeliveries, &lash_core::provider::LiveCallHorizon::fixture()).await
    }).await;
}

// FIG-5743: the HTTP refusal remains evidence when its body exceeds the bound;
// a 401 still replaces credentials once before surfacing the second refusal.
#[tokio::test]
async fn refused_http_body_retains_evidence_and_auth_replacement() {
    use lash_core::facade_support::LlmTransportError;
    use lash_core::provider::{
        ProviderOptions, ProviderToken, TokenError, TokenRequest, TokenRequestReason, TokenSource,
    };
    use lash_llm_transport::{
        HttpFailureContext, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Debug, Default)]
    struct Source {
        replacements: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl TokenSource for Source {
        async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
            if request.reason == TokenRequestReason::Rejected {
                self.replacements.fetch_add(1, Ordering::SeqCst);
                Ok(ProviderToken::new("fresh"))
            } else {
                Ok(ProviderToken::new("stale"))
            }
        }
    }
    #[derive(Debug)]
    struct Refused {
        status: u16,
        sends: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl LlmHttpTransport for Refused {
        async fn send(
            &self,
            request: LlmHttpRequest,
            _: Option<std::time::Duration>,
        ) -> Result<LlmHttpResponse, LlmTransportError> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            let mut response =
                EpilogueTransport::new("123456789", true, EpilogueScenario::HttpFailure)
                    .send(request, None)
                    .await?;
            response.status = self.status;
            response.headers = vec![
                ("request-id".into(), "refused-id".into()),
                ("retry-after".into(), "7".into()),
            ];
            Ok(response)
        }
    }
    for status in [429, 401] {
        let source = Arc::new(Source::default());
        let transport = Arc::new(Refused {
            status,
            sends: AtomicUsize::new(0),
        });
        let mut provider = AnthropicProvider::with_token_source(source.clone())
            .with_transport(transport.clone())
            .with_options(ProviderOptions {
                response_body_bytes: Some(8),
                ..Default::default()
            });
        let error = provider
            .complete(
                request(vec![LlmMessage::text(LlmRole::User, "hello")]),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status, Some(status));
        assert_eq!(
            lash_llm_transport::first_header_value(&error.headers, "x-request-id"),
            Some("refused-id")
        );
        assert_eq!(
            lash_llm_transport::first_header_value(&error.headers, "retry-after"),
            Some("7")
        );
        assert!(matches!(
            *error.context,
            HttpFailureContext::ResponseBodyTooLarge {
                limit: 8,
                received_at_least: 9
            }
        ));
        assert_eq!(
            error.code.as_ref().unwrap().spelling(),
            "http_response_body_too_large"
        );
        assert_eq!(
            source.replacements.load(Ordering::SeqCst),
            usize::from(status == 401)
        );
        assert_eq!(
            transport.sends.load(Ordering::SeqCst),
            if status == 401 { 2 } else { 1 }
        );
    }
}
