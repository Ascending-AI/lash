use super::*;

#[derive(Debug)]
struct ScriptedIngress {
    responses: Mutex<VecDeque<Result<HttpResponse, LlmTransportError>>>,
    requests: Mutex<Vec<HttpRequest>>,
    request_times: Mutex<Vec<tokio::time::Instant>>,
}

impl ScriptedIngress {
    fn new(responses: Vec<Result<HttpResponse, LlmTransportError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
            request_times: Mutex::new(Vec::new()),
        })
    }

    fn connection(self: &Arc<Self>) -> RestateConnection {
        RestateConnection::with_transport("https://restate.invalid", self.clone())
    }
}

#[async_trait::async_trait]
impl HttpTransport for ScriptedIngress {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        self.request_times
            .lock_recover()
            .push(tokio::time::Instant::now());
        self.responses
            .lock_recover()
            .pop_front()
            .expect("the wait must issue only the scripted requests")
    }
}

fn response(status: u16, body: impl Into<Bytes>) -> Result<HttpResponse, LlmTransportError> {
    Ok(HttpResponse {
        status,
        headers: Vec::new(),
        body: HttpResponseBody::buffered(body),
    })
}

fn reply<T: Serialize>(body: T) -> Result<HttpResponse, LlmTransportError> {
    response(
        200,
        serde_json::to_vec(&crate::Reply::at(crate::compat::RESTATE_WIRE_VERSION, body))
            .expect("encode the real terminal reply"),
    )
}

fn transport_failure(message: &str) -> Result<HttpResponse, LlmTransportError> {
    Err(LlmTransportError::new(message).with_kind(lash_core::ProviderFailureKind::Transport))
}

#[tokio::test]
async fn process_wait_reattaches_after_ingress_failure_to_the_real_outcome() {
    let failures = vec![
        transport_failure("connection refused"),
        transport_failure("connection reset"),
        Err(LlmTransportError::response_read("unexpected EOF")),
        Err(LlmTransportError::new("attach ceiling elapsed")
            .with_kind(lash_core::ProviderFailureKind::Timeout)),
        response(200, r#"{"wire":1,"body":"#),
        response(408, "request timeout"),
        response(429, "temporarily overloaded"),
        response(500, "internal server error"),
        response(502, "bad gateway"),
        response(503, "node unavailable"),
        response(504, "gateway timeout"),
    ];
    for failure in failures {
        let expected = legacy_process_success(serde_json::json!({"answer": 42}));
        let ingress = ScriptedIngress::new(vec![failure, reply(expected.clone())]);
        let registry = process_registry();
        let process = registry
            .register_process(external_registration())
            .await
            .expect("register the process whose outcome remains unknown");
        let runner = RestateProcessIngressRunner::new(
            ingress.connection(),
            registry.clone(),
            continuation_store(),
            lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
        );

        assert_eq!(
            runner.await_process_terminal(&process.id).await.expect(
                "an unavailable ingress cannot turn an unknown process outcome into a failure"
            ),
            lash_core::ProcessTerminalWait::Reattach
        );
        let still_running = registry
            .get_process(&process.id)
            .await
            .expect("read after the failed connection")
            .expect("the process remains retained");
        assert_eq!(still_running.status(), process.status());
        assert!(still_running.outcome().is_none());
        assert_eq!(
            runner
                .await_process_terminal(&process.id)
                .await
                .expect("reattach"),
            lash_core::ProcessTerminalWait::Terminal(expected)
        );
        let requests = ingress.requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, requests[1].url);
        assert_eq!(requests[0].body, requests[1].body);
        assert!(requests[0].url.ends_with(&format!(
            "/LashProcessWorkflow/{}/await_terminal",
            process.id
        )));
    }
}

#[tokio::test]
async fn a_definitive_process_target_failure_keeps_its_typed_error() {
    for (status, body) in [
        (400, "definitive refusal"),
        (404, "definitive refusal"),
        (409, "definitive refusal"),
        (422, "definitive refusal"),
        (
            500,
            r#"{"code":500,"message":"typed invocation refusal","source":"invocation"}"#,
        ),
        (
            503,
            r#"{"code":503,"message":"typed invocation refusal","source":"invocation"}"#,
        ),
    ] {
        let ingress = ScriptedIngress::new(vec![response(status, body)]);
        let registry = process_registry();
        let process = registry
            .register_process(external_registration())
            .await
            .expect("register the target");
        let runner = RestateProcessIngressRunner::new(
            ingress.connection(),
            registry.clone(),
            continuation_store(),
            lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
        );

        let error = runner
            .await_process_terminal(&process.id)
            .await
            .expect_err("a definitive refusal must not request reattachment");
        let PluginError::Runtime(error) = error else {
            panic!("expected the existing typed ingress error");
        };
        assert_eq!(error.code, lash_core::RuntimeErrorCode::EngineProcessAwait);
        if status == 404 {
            assert!(error.message.contains("Restate could not address"));
            assert!(error.message.contains("LashProcessWorkflow/await_terminal"));
        }
        assert_eq!(ingress.requests.lock_recover().len(), 1);
        assert!(
            registry
                .get_process(&process.id)
                .await
                .expect("read")
                .expect("retained process")
                .outcome()
                .is_none()
        );
    }
}

#[tokio::test]
async fn caller_departure_refuses_the_wait_without_contacting_ingress() {
    let ingress = ScriptedIngress::new(Vec::new());
    let registry = process_registry();
    let process = registry
        .register_process(external_registration())
        .await
        .expect("register");
    registry
        .record_caller_departure(&process.id)
        .await
        .expect("record the durable departure");
    let runner = RestateProcessIngressRunner::new(
        ingress.connection(),
        registry.clone(),
        continuation_store(),
        lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
    );

    assert!(matches!(runner.await_process_terminal(&process.id).await,
        Err(PluginError::ProcessCallerDeparted { process_id }) if process_id == process.id));
    assert!(ingress.requests.lock_recover().is_empty());
    let record = registry
        .get_process(&process.id)
        .await
        .expect("read")
        .expect("retained");
    assert_eq!(record.status(), lash_core::ProcessStatus::CallerDeparted);
    assert!(record.outcome().is_none());
}

#[tokio::test]
async fn process_cancel_watch_definitive_refusal_ends_without_retry() {
    let ingress = ScriptedIngress::new(vec![response(400, "invalid cancel-watch request")]);
    let delivery = crate::process_stop::ProcessStopDelivery::watch(
        RestateIngressClient::new(ingress.connection()),
        crate::services::DEFAULT_NAMESPACE.stable(crate::LashService::ProcessWorkflow),
        ProcessId::fixture("refused-cancel-watch"),
        "refused-cancel-watch".to_string(),
        tokio_util::sync::CancellationToken::new(),
    );
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        delivery.shift(std::future::pending::<()>()),
    )
    .await
    .expect("a definitive answer ends the attempt")
    .expect_err("the request was refused");
    let source: &(dyn std::error::Error + Send + Sync) = error.as_ref();
    assert!(
        source.to_string().starts_with("Terminal error [400]"),
        "{error:?}"
    );
    assert_eq!(ingress.requests.lock_recover().len(), 1);
}

#[test]
fn ingress_error_classification_distinguishes_availability_from_definitive_answers() {
    use crate::RestateHttpErrorClass::{Terminal, Transient};
    let status_error = |status, body: &str| crate::RestateHttpError::Status {
        operation: "Restate workflow call",
        url: "https://restate.invalid/LashProcessWorkflow/k/await_terminal".to_string(),
        status,
        body: body.to_string(),
    };
    for status in 100..600 {
        let expected = match status {
            408 | 429 | 500..=599 => Transient,
            _ => Terminal,
        };
        assert_eq!(
            status_error(status, "ingress failure").classification(),
            expected
        );
        assert_eq!(
            status_error(status, r#"{"source":"invocation","message":"refused"}"#).classification(),
            Terminal,
            "an invocation's answer is definitive regardless of its HTTP code"
        );
    }
    for verdict in [
        lash_http_transport::TransportRetryVerdict::NotRetryable,
        lash_http_transport::TransportRetryVerdict::RetryableTransient,
        lash_http_transport::TransportRetryVerdict::RetryableThrottle { retry_after: None },
        lash_http_transport::TransportRetryVerdict::Forbidden,
    ] {
        let error = crate::RestateHttpError::Request {
            operation: "Restate workflow call",
            url: "https://restate.invalid".to_string(),
            source: LlmTransportError::new("host transport failure").with_retry_verdict(verdict),
        };
        assert_eq!(
            error.classification(),
            if verdict == lash_http_transport::TransportRetryVerdict::Forbidden {
                Terminal
            } else {
                Transient
            }
        );
    }
    for (json, expected) in [(r#"{"wire":"#, Transient), ("invalid JSON", Terminal)] {
        let source = serde_json::from_str::<serde_json::Value>(json).expect_err("invalid reply");
        let error = crate::RestateHttpError::Decode {
            operation: "Restate workflow call",
            url: "https://restate.invalid".to_string(),
            source,
        };
        assert_eq!(error.classification(), expected);
    }
    let source =
        serde_json::from_str::<serde_json::Value>("invalid JSON").expect_err("encode fixture");
    let error = crate::RestateHttpError::Encode {
        operation: "Restate workflow call",
        url: "https://restate.invalid".to_string(),
        source,
    };
    assert_eq!(error.classification(), Terminal);
    let error = crate::RestateHttpError::UnexpectedSendStatus {
        url: "https://restate.invalid".to_string(),
        status: "not an acceptance".to_string(),
    };
    assert_eq!(error.classification(), Terminal);

    let refusal = lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::EngineObjectStateFormatUnsupported,
        "stored format is unsupported",
    );
    let body = serde_json::json!({"message": refusal.to_record()});
    assert_eq!(
        status_error(500, &body.to_string()).classification(),
        Terminal
    );
}

#[tokio::test]
async fn bounded_idempotent_send_retries_only_transient_failures_under_the_same_key() {
    let ingress = ScriptedIngress::new(vec![
        transport_failure("connection refused during restart"),
        response(
            202,
            r#"{"invocationId":"inv_recovered","status":"Accepted"}"#,
        ),
    ]);
    let client = RestateIngressClient::new(ingress.connection());
    let id = client
        .send_object_json_idempotent_bounded(
            "LashSession",
            "session",
            "shift",
            &serde_json::json!({"request": "shift-1"}),
            "shift-1",
        )
        .await
        .expect("an idempotent send survives an unavailable ingress");
    assert_eq!(id.as_str(), "inv_recovered");
    {
        let requests = ingress.requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, requests[1].url);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(requests[0].headers, requests[1].headers);
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "idempotency-key" && value == "shift-1")
        );
    }

    for refusal in [
        response(
            500,
            r#"{"code":500,"message":"refused","source":"invocation"}"#,
        ),
        response(202, "complete but invalid JSON"),
    ] {
        let ingress = ScriptedIngress::new(vec![refusal]);
        let error = RestateIngressClient::new(ingress.connection())
            .send_object_json_idempotent_bounded("LashSession", "session", "shift", &(), "shift-1")
            .await
            .expect_err("a definitive refusal cannot improve with a retry");
        assert_eq!(
            error.classification(),
            crate::RestateHttpErrorClass::Terminal
        );
        assert_eq!(ingress.requests.lock_recover().len(), 1);
    }
}
