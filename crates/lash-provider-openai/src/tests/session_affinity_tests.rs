use super::*;
use lash_sansio::SessionId;

#[tokio::test]
async fn host_enabled_session_affinity_works_through_a_custom_proxy_url() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = OpenAiCompatibleProvider::new("key", "https://router-proxy.example/v1")
        .with_compat(OpenAiCompat {
            cache_session_affinity: Some(true),
            ..OpenAiCompat::default()
        })
        .with_transport(transport.clone());
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let session_id = SessionId::from(format!("{}étrailing", "s".repeat(255)));
    req.scope.session_id = session_id.clone();
    let expected = req.scope.provider_session_affinity_key();

    provider.complete(req).await.expect("request succeeds");

    let requests = transport.requests.lock_recover();
    let wire_request = requests.first().expect("captured request");
    let body: Value = serde_json::from_slice(&wire_request.body).expect("request body");
    assert_eq!(body["session_id"], expected);
    assert!(
        body["session_id"]
            .as_str()
            .expect("session id")
            .chars()
            .all(|c| c.is_ascii_hexdigit()),
        "affinity session id must be an opaque hex hash"
    );
    assert!(
        !wire_request
            .body
            .windows(session_id.as_str().len())
            .any(|window| window == session_id.as_str().as_bytes()),
        "request body must not contain the raw session id"
    );
    assert!(
        wire_request
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("session_id"))
    );
    assert!(wire_request.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("x-client-request-id") && value == "session-1:request:test"
    }));
}

#[test]
fn request_bodies_carry_only_hashed_session_identity() {
    // Host-minted session ids can embed tenant and user identifiers; every
    // provider-facing cache or affinity field must carry only the opaque hash.
    let raw_session = "tenant:acme-corp:user:jane.doe@acme.com:chat:8f2c";
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.scope.session_id = SessionId::from(raw_session);
    let session_key = req.provider_session_affinity_key();
    let cache_key = req.provider_prompt_cache_key();

    let responses = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .expect("responses body");
    assert_eq!(responses["prompt_cache_key"], cache_key);
    assert!(!responses.to_string().contains(raw_session));

    let codex = crate::CodexProvider::new("access", "refresh", 0)
        .with_options(ProviderOptions {
            cache_retention: CacheRetention::Short,
            ..ProviderOptions::default()
        })
        .build_request_body(&req, false)
        .expect("codex body");
    assert_eq!(codex["prompt_cache_key"], cache_key);
    assert!(!codex.to_string().contains(raw_session));

    // The affinity `session_id` field is injected by the shared driver layer
    // for both endpoints when `cache_session_affinity` is set (OpenRouter).
    for (endpoint, kind) in [
        (CompletionEndpoint::Responses, "openai"),
        (CompletionEndpoint::ChatCompletions, "openai-compatible"),
    ] {
        let provider = openrouter_provider();
        let route =
            ProviderRouteIdentity::for_endpoint(kind, &provider.base_url, req.model.clone());
        let (body, _) = crate::driver::build_request_body(&provider, &req, endpoint, false, &route)
            .expect("openrouter-compatible body");
        assert_eq!(body["session_id"], session_key);
        assert!(
            !body.to_string().contains(raw_session),
            "{:?} body leaked the raw session id",
            endpoint
        );
    }
}
