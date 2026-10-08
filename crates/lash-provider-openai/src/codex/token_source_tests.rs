//! Laws for the host-owned token seam on Codex: lash asks the host's
//! `TokenSource` before every attempt and once more after a 401 that arrived
//! before any output, and resends the admitted body once.

use super::*;
use async_trait::async_trait;
use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict};
use lash_core::provider::ProviderToken;
use lash_core::provider::{
    TokenError, TokenErrorKind, TokenRequest, TokenRequestReason, TokenSource,
};
use lash_llm_transport::{
    LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport,
};
use ws_testing::spawn_scripted_websocket_rejecting_token;

/// A host source that rotates `token-N` to `token-N+1` when lash rejects the
/// token it still holds, and records every ask. `refuse` makes every ask fail.
#[derive(Debug, Default)]
struct HostTokens {
    generation: Mutex<u32>,
    asks: Mutex<Vec<TokenRequestReason>>,
    refuse: Option<TokenErrorKind>,
}

impl HostTokens {
    fn asks(&self) -> Vec<TokenRequestReason> {
        self.asks.lock_recover().clone()
    }
}

#[async_trait]
impl TokenSource for HostTokens {
    async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
        self.asks.lock_recover().push(request.reason);
        if let Some(kind) = self.refuse {
            return Err(TokenError::new(kind, "the host login was revoked"));
        }
        let mut generation = self.generation.lock_recover();
        let current = format!("token-{}", *generation + 1);
        let stale_is_current = request
            .stale
            .is_some_and(|stale| stale.secret().expose_secret() == current);
        if request.reason != TokenRequestReason::Current && stale_is_current {
            *generation += 1;
        }
        Ok(ProviderToken::new(format!("token-{}", *generation + 1)))
    }
}

/// Answers each request with the next scripted response, recording what was
/// sent.
#[derive(Debug)]
struct ScriptedHttp {
    responses: Mutex<Vec<LlmHttpResponse>>,
    sent: Mutex<Vec<LlmHttpRequest>>,
}

impl ScriptedHttp {
    fn new(responses: Vec<LlmHttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<LlmHttpRequest> {
        self.sent.lock_recover().clone()
    }

    fn authorizations(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter_map(|request| {
                lash_llm_transport::first_header_value(&request.headers, "authorization")
                    .map(str::to_string)
            })
            .collect()
    }
}

#[async_trait]
impl LlmHttpTransport for ScriptedHttp {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        self.sent.lock_recover().push(request);
        let mut responses = self.responses.lock_recover();
        assert!(!responses.is_empty(), "an unscripted HTTP request was sent");
        Ok(responses.remove(0))
    }
}

fn unauthorized() -> LlmHttpResponse {
    LlmHttpResponse {
        status: 401,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: LlmHttpBody::buffered(r#"{"detail":"token expired"}"#),
    }
}

fn completed_sse(text: &str) -> LlmHttpResponse {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_item.done","output_index":0,"item":assistant_item("msg_1", text)}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[assistant_item("msg_1", text)],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})
    );
    LlmHttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: LlmHttpBody::buffered(body),
    }
}

/// Streams one text delta, then fails the read with a 401-shaped error.
#[derive(Debug)]
struct OutputThenUnauthorized {
    sent_delta: bool,
}

#[async_trait]
impl LlmByteStream for OutputThenUnauthorized {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        if !self.sent_delta {
            self.sent_delta = true;
            let added = json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1","status":"in_progress"}});
            let delta =
                json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Hel"});
            return Ok(Some(format!("data: {added}\n\ndata: {delta}\n\n").into()));
        }
        Err(
            LlmTransportError::response_read("the gateway revoked the session")
                .with_http_status(401),
        )
    }
}

fn sse_provider(tokens: Arc<HostTokens>, http: Arc<ScriptedHttp>) -> CodexProvider {
    CodexProvider::new(tokens)
        .force_sse_transport()
        .with_http_transport(http)
}

fn streaming_request() -> LlmRequest {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.stream_events = Some(lash_core::llm::types::LlmEventSender::new(|_| {}));
    req
}

#[tokio::test]
async fn a_pre_output_401_asks_the_source_once_and_resends_the_admitted_body() {
    let tokens = Arc::new(HostTokens::default());
    let http = ScriptedHttp::new(vec![unauthorized(), completed_sse("Hello")]);
    let mut provider = sse_provider(Arc::clone(&tokens), Arc::clone(&http));

    let response = provider
        .complete(streaming_request())
        .await
        .expect("the resend with the fresh token completes");

    assert_eq!(response.full_text(), "Hello");
    assert_eq!(
        tokens.asks(),
        vec![TokenRequestReason::Current, TokenRequestReason::Rejected]
    );
    assert_eq!(
        http.authorizations(),
        vec!["Bearer token-1".to_string(), "Bearer token-2".to_string()]
    );
    let sent = http.sent();
    assert_eq!(
        sent[0].body, sent[1].body,
        "the admitted body is resent as is"
    );
}

#[tokio::test]
async fn a_401_after_output_started_is_surfaced_and_not_retried() {
    let tokens = Arc::new(HostTokens::default());
    let http = ScriptedHttp::new(vec![LlmHttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: LlmHttpBody::streamed(OutputThenUnauthorized { sent_delta: false }),
    }]);
    let mut provider = sse_provider(Arc::clone(&tokens), Arc::clone(&http));

    let error = provider
        .complete(streaming_request())
        .await
        .expect_err("a 401 after output must surface");

    assert!(error.output_started);
    assert_eq!(error.http_status, Some(401));
    assert_eq!(tokens.asks(), vec![TokenRequestReason::Current]);
    assert_eq!(http.sent().len(), 1);
}

#[tokio::test]
async fn a_host_that_needs_a_new_sign_in_fails_the_call_with_its_typed_code() {
    let tokens = Arc::new(HostTokens {
        refuse: Some(TokenErrorKind::ReauthRequired),
        ..HostTokens::default()
    });
    let http = ScriptedHttp::new(Vec::new());
    let mut provider = sse_provider(Arc::clone(&tokens), Arc::clone(&http));

    let error = provider
        .complete(streaming_request())
        .await
        .expect_err("no token, no call");

    assert_eq!(
        error.code.as_ref().map(ToString::to_string).as_deref(),
        Some("lash:credential_reauth_required")
    );
    assert_eq!(error.kind, ProviderFailureKind::Auth);
    assert_eq!(error.retry_verdict, TransportRetryVerdict::Forbidden);
    assert!(http.sent().is_empty(), "nothing is sent without a token");
}

/// A WebSocket handshake rejected with 401 goes to the token source, not to
/// an SSE fallback that would send the same token again.
#[tokio::test]
async fn a_rejected_websocket_handshake_replaces_the_token_instead_of_falling_back() {
    let server = spawn_scripted_websocket_rejecting_token(
        vec![ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "Hello",
        }],
        "token-1",
    )
    .await;
    let tokens = Arc::new(HostTokens::default());
    let http = ScriptedHttp::new(Vec::new());
    let mut provider = CodexProvider::new(Arc::clone(&tokens) as Arc<dyn TokenSource>)
        .with_transport(CodexTransport::Auto)
        .with_endpoint_urls("http://unused.test/codex/responses", server.url.clone())
        .with_http_transport(Arc::clone(&http) as Arc<dyn LlmHttpTransport>);

    let response = provider
        .complete(streaming_request())
        .await
        .expect("the WebSocket resend with the fresh token completes");

    assert_eq!(response.full_text(), "Hello");
    assert_eq!(
        tokens.asks(),
        vec![TokenRequestReason::Current, TokenRequestReason::Rejected]
    );
    let bearers = server
        .handshakes()
        .iter()
        .filter_map(|headers| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(bearers, vec!["Bearer token-1", "Bearer token-2"]);
    assert!(
        http.sent().is_empty(),
        "no SSE fallback with the rejected token"
    );
}
