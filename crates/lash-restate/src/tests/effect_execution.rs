use super::*;

#[tokio::test]
pub(super) async fn ingress_sweep_resumes_latest_segment_without_duplicate_segment_zero() {
    let (registry, continuations) = process_stores();
    registry
        .register_process(rerunnable_registration("mid-chain"))
        .await
        .expect("register");
    continuations
        .put_segment_handover(
            &ProcessId::from("mid-chain"),
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 3,
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "program-v1".to_string(),
                    engine_state: vec![3],
                },
            },
        )
        .await
        .expect("persist live segment");
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_mid_chain_3","status":"Accepted"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuations);
    let _ = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending");
    server.await.expect("capture server");

    let requests = captured.lock_recover();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/mid-chain%233/run/send "),
        "recovery must address the latest segment workflow key: {}",
        requests[0]
    );
    assert!(
        requests[0].contains("\"segment_ordinal\":3"),
        "recovery input must preserve the latest ordinal: {}",
        requests[0]
    );
    assert!(
        !requests[0].contains("\"execution_id\""),
        "an ingress redrive must mint identity from its new invocation: {}",
        requests[0]
    );
    assert!(!requests[0].starts_with("POST /LashProcessWorkflow/mid-chain/run/send "));
}

#[tokio::test]
pub(super) async fn ingress_sweep_skips_externally_owned_and_reconciles_abandon_request() {
    // ADR 0019 at the Restate tier: the ingress sweep never POSTs a run for an
    // ExternallyOwned row (Lash does not execute it), but it does reconcile such
    // a row's pending Abandon Request into an `Abandoned{ReconciledRequest}`
    // terminal — mirroring the core sweep's `reconcile_externally_owned_abandon`.
    // A Rerunnable row alongside them still submits, so exactly one ingress call
    // fires and it is for the Lash-executed row.
    let registry = process_registry();
    registry
        .register_process(external_registration("ext-abandon"))
        .await
        .expect("register externally-owned row with pending abandon");
    registry
        .request_process_abandon(
            &ProcessId::from("ext-abandon"),
            lash_core::AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 111,
                reason: Some("host retired".to_string()),
            },
        )
        .await
        .expect("record abandon request");
    registry
        .register_process(external_registration("ext-idle"))
        .await
        .expect("register externally-owned row without abandon");
    registry
        .register_process(rerunnable_registration("rerun-1"))
        .await
        .expect("register rerunnable row");

    // The capture server accepts exactly one connection: if any ExternallyOwned
    // row were submitted, a second connect would be attempted and the extra
    // submit would fail, so the single-response server also proves they are not.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_rerun_1","status":"Accepted"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuation_store());
    let report = runner
        .admit_pending_processes("test")
        .await
        .expect("sweep skips externally-owned rows and submits the rerunnable one");
    server.await.expect("mock ingress server task");

    // Skipped is not silent: an externally-owned row is a typed deferral on this
    // tier too, so one registry reads the same whichever tier drove it.
    assert_eq!(report.admitted, vec!["rerun-1".to_string()]);
    let externally_owned = report
        .deferred
        .iter()
        .filter(|entry| entry.disposition == ProcessRecoveryAttemptOutcome::ExternallyOwned)
        .map(|entry| entry.process_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        externally_owned,
        vec!["ext-abandon".to_string(), "ext-idle".to_string()]
    );

    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        1,
        "only the Rerunnable row is submitted; ExternallyOwned rows are never POSTed"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/rerun-1/run/send "),
        "the single submit is the Lash-executed row: {}",
        requests[0]
    );

    // The abandon-request externally-owned row is now terminal Abandoned, written
    // by the reconciled-request path with no Lash execution owner to name.
    let abandoned = registry
        .get_process(&ProcessId::from("ext-abandon"))
        .await
        .expect("read process")
        .expect("get reconciled row");
    assert!(
        abandoned.is_terminal(),
        "an externally-owned row with a pending abandon request is reconciled to terminal"
    );
    let Some(ProcessAwaitOutput::Abandoned { evidence, .. }) = abandoned.outcome.as_ref() else {
        panic!("expected Abandoned terminal, got {:?}", abandoned.status);
    };
    assert_eq!(evidence.writer, AbandonWriter::ReconciledRequest);
    assert!(
        evidence.owner.is_none(),
        "externally-owned work has no Lash execution owner to name"
    );

    // The externally-owned row without an abandon request is left untouched for
    // its external owner to complete.
    let idle = registry
        .get_process(&ProcessId::from("ext-idle"))
        .await
        .expect("read process")
        .expect("get idle externally-owned row");
    assert!(
        !idle.is_terminal(),
        "an externally-owned row with no abandon request is left non-terminal"
    );
}

pub(super) struct MockHttpResponse {
    status: &'static str,
    body: &'static str,
}

pub(super) async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let mut scratch = [0u8; 1024];
    loop {
        let n = socket.read(&mut scratch).await.expect("read request");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
        let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if buf.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub(super) async fn spawn_restate_http_capture(
    responses: Vec<MockHttpResponse>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            captured_server.lock_recover().push(request);
            let body = response.body.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status,
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(body).await.expect("write response body");
            socket.flush().await.expect("flush");
        }
    });
    (format!("http://{addr}"), captured, server)
}

pub(super) async fn spawn_restate_http_black_hole() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _request = read_http_request(&mut socket).await;
        std::future::pending::<()>().await;
    });
    (format!("http://{addr}"), server)
}

pub(super) async fn spawn_restate_http_timeout_then_capture(
    response: MockHttpResponse,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (mut first_socket, _) = listener.accept().await.expect("accept first wait");
        let first_request = read_http_request(&mut first_socket).await;
        captured_server.lock_recover().push(first_request);

        let (mut second_socket, _) = listener.accept().await.expect("accept reattached wait");
        let second_request = read_http_request(&mut second_socket).await;
        captured_server.lock_recover().push(second_request);
        let body = response.body.as_bytes();
        let header = format!(
            "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            response.status,
            body.len()
        );
        second_socket
            .write_all(header.as_bytes())
            .await
            .expect("write response headers");
        second_socket
            .write_all(body)
            .await
            .expect("write response body");
        second_socket.flush().await.expect("flush");
        drop(first_socket);
    });
    (format!("http://{addr}"), captured, server)
}

pub(super) async fn spawn_restate_http_stalled_body(
    status: &'static str,
    body: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _request = read_http_request(&mut socket).await;
        let header = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write response headers");
        socket.flush().await.expect("flush response headers");
        std::future::pending::<()>().await;
    });
    (format!("http://{addr}"), server)
}

pub(super) fn short_restate_timeouts(
    control_timeout_ms: u64,
    attach_ceiling_ms: u64,
) -> RestateConnectionConfig {
    RestateConnectionConfig {
        control_timeout_ms,
        attach_ceiling_ms,
    }
}

pub(super) fn assert_retryable_timeout(error: RestateHttpError, expected_message: &str) {
    let RestateHttpError::Request { source, .. } = error else {
        panic!("expected typed Restate request timeout, got {error}");
    };
    assert_eq!(source.kind, lash_core::ProviderFailureKind::Timeout);
    assert_eq!(source.code.as_deref(), Some("timeout"));
    assert!(source.is_retryable());
    assert!(source.message.contains(expected_message), "{source}");
}

#[test]
pub(super) fn restate_connection_timeout_config_has_serde_defaults() {
    let defaults: RestateConnectionConfig = serde_json::from_str("{}").expect("default config");
    assert_eq!(defaults.control_timeout_ms, 30_000);
    assert_eq!(defaults.attach_ceiling_ms, 6 * 60 * 60 * 1_000);

    let configured: RestateConnectionConfig = serde_json::from_value(serde_json::json!({
        "control_timeout_ms": 125,
        "attach_ceiling_ms": 9_000,
    }))
    .expect("configured timeouts");
    assert_eq!(configured.control_timeout_ms, 125);
    assert_eq!(configured.attach_ceiling_ms, 9_000);

    let unknown = serde_json::from_value::<RestateConnectionConfig>(serde_json::json!({
        "control_timeout_ms": 125,
        "attach_ceiling_ms": 9_000,
        "control_timout_ms": 10,
    }))
    .expect_err("unknown timeout fields must be rejected");
    assert!(
        unknown
            .to_string()
            .contains("unknown field `control_timout_ms`")
    );

    for (field, value) in [
        (
            "control_timeout_ms",
            serde_json::json!({"control_timeout_ms": 0}),
        ),
        (
            "attach_ceiling_ms",
            serde_json::json!({"attach_ceiling_ms": 0}),
        ),
    ] {
        let error = serde_json::from_value::<RestateConnectionConfig>(value)
            .expect_err("zero timeout must be rejected");
        assert!(
            error
                .to_string()
                .contains(&format!("{field} must be greater than zero")),
            "unexpected config error: {error}"
        );
    }
}

#[tokio::test]
pub(super) async fn restate_control_operation_times_out_against_black_hole() {
    let (base_url, server) = spawn_restate_http_black_hole().await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(500, 2_000),
    ));
    let started = std::time::Instant::now();

    let error = client
        .send_service_json("LashService", "run", &serde_json::json!({}))
        .await
        .expect_err("control submit must time out");
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;

    assert_retryable_timeout(error, "control deadline");
    assert!(elapsed >= Duration::from_millis(400), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
}

#[tokio::test]
pub(super) async fn restate_attach_survives_control_timeout_and_honors_ceiling() {
    let control_timeout = Duration::from_millis(20);
    let response_delay = Duration::from_millis(80);
    let attach_ceiling = Duration::from_secs(2);
    let (base_url, _captured, server) = spawn_restate_http_capture_delayed(
        vec![MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"attached"}"#,
        }],
        response_delay,
    )
    .await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(
            control_timeout.as_millis() as u64,
            attach_ceiling.as_millis() as u64,
        ),
    ));
    let started = std::time::Instant::now();

    let output: ProcessAwaitOutput = client
        .call_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "await_terminal",
            &RestateProcessAwaitRequest {
                process_id: ProcessId::from("process-1"),
            },
        )
        .await
        .expect("attach must use its ceiling rather than the control timeout");
    let elapsed = started.elapsed();
    server.await.expect("delayed response server");

    assert_eq!(
        output,
        legacy_process_success(serde_json::json!("attached"))
    );
    assert!(elapsed > control_timeout, "elapsed {elapsed:?}");
    assert!(elapsed < attach_ceiling, "elapsed {elapsed:?}");

    let (base_url, black_hole) = spawn_restate_http_black_hole().await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(100, 500),
    ));
    let started = std::time::Instant::now();
    let error = client
        .call_workflow_json::<_, serde_json::Value>(
            "LashWorkflow",
            "key",
            "await",
            &serde_json::json!({}),
        )
        .await
        .expect_err("attach ceiling must bound a black hole");
    let elapsed = started.elapsed();
    black_hole.abort();
    let _ = black_hole.await;

    assert_retryable_timeout(error, "attach ceiling");
    assert!(elapsed >= Duration::from_millis(400), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
}

#[tokio::test]
pub(super) async fn restate_attach_body_read_is_clamped_to_control_timeout() {
    let (base_url, server) = spawn_restate_http_stalled_body("200 OK", "{}").await;
    let client = RestateIngressClient::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(100, 2_000),
    ));
    let started = std::time::Instant::now();

    let error = client
        .call_workflow_json::<_, serde_json::Value>(
            "LashWorkflow",
            "key",
            "await",
            &serde_json::json!({}),
        )
        .await
        .expect_err("an attach body is no longer a durable park");
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;

    assert_retryable_timeout(error, "attach ceiling response body");
    assert!(elapsed >= Duration::from_millis(75), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_millis(500), "elapsed {elapsed:?}");
}

#[tokio::test]
pub(super) async fn restate_success_and_error_body_reads_share_the_request_deadline() {
    for status in ["202 Accepted", "500 Internal Server Error"] {
        let (base_url, server) = spawn_restate_http_stalled_body(status, "{}").await;
        let client = RestateIngressClient::new(RestateConnection::with_config(
            base_url,
            short_restate_timeouts(30, 500),
        ));
        let error = client
            .send_service_json("LashService", "run", &serde_json::json!({}))
            .await
            .expect_err("stalled response body must time out");
        server.abort();
        let _ = server.await;
        assert_retryable_timeout(error, "control deadline response body");
    }
}

#[tokio::test]
pub(super) async fn restate_ingress_client_parses_send_invocation_id() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_123","status":"Accepted"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let invocation_id = client
        .send_workflow_json(
            "WorkbenchTurnWorkflow",
            "turn-1",
            "run",
            &serde_json::json!({ "turn_id": "turn-1" }),
        )
        .await
        .expect("send workflow");
    server.await.expect("capture server");

    assert_eq!(invocation_id.as_str(), "inv_123");
    let requests = captured.lock_recover();
    assert!(
        requests[0].starts_with("POST /WorkbenchTurnWorkflow/turn-1/run/send "),
        "unexpected request: {}",
        requests[0]
    );
    assert!(requests[0].contains(r#""turn_id":"turn-1""#));
}

#[derive(Debug)]
pub(super) struct ScriptedHttpTransport {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<VecDeque<HttpResponse>>,
}

impl ScriptedHttpTransport {
    pub(super) fn new(responses: impl IntoIterator<Item = HttpResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    pub(super) fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl HttpTransport for ScriptedHttpTransport {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.requests.lock_recover().push(request);
        self.responses
            .lock_recover()
            .pop_front()
            .ok_or_else(|| HttpTransportError::new("scripted transport exhausted"))
    }
}

#[derive(Debug)]
pub(super) struct AuthorizationTransport {
    inner: Arc<dyn HttpTransport>,
    token: Arc<RwLock<String>>,
}

#[async_trait::async_trait]
impl HttpTransport for AuthorizationTransport {
    async fn send(
        &self,
        mut request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpTransportError> {
        let token = self.token.read_recover().clone();
        request
            .headers
            .push(("authorization".to_string(), format!("Bearer {token}")));
        self.inner.send(request, timeout).await
    }
}

pub(super) fn accepted_response(invocation_id: &str) -> HttpResponse {
    HttpResponse {
        status: 202,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: HttpResponseBody::buffered(format!(
            r#"{{"invocationId":"{invocation_id}","status":"Accepted"}}"#
        )),
    }
}

pub(super) const RESERVED_INGRESS_KEY: &str = "key/with?reserved% space";
pub(super) const ENCODED_RESERVED_INGRESS_KEY: &str = "key%2Fwith%3Freserved%25%20space";

pub(super) fn scripted_ingress_client(
    responses: impl IntoIterator<Item = HttpResponse>,
) -> (RestateIngressClient, Arc<ScriptedHttpTransport>) {
    let scripted = Arc::new(ScriptedHttpTransport::new(responses));
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        scripted.clone(),
    ));
    (client, scripted)
}

pub(super) fn assert_reserved_ingress_url(requests: &[HttpRequest], expected_path: &str) {
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        format!("https://cloud.example/{expected_path}")
    );
}

#[tokio::test]
pub(super) async fn restate_workflow_send_url_encodes_reserved_key() {
    let (client, scripted) = scripted_ingress_client([accepted_response("inv_workflow_send")]);

    client
        .send_workflow_json(
            "Lash/Workflow",
            RESERVED_INGRESS_KEY,
            "run?now",
            &serde_json::json!({}),
        )
        .await
        .expect("send workflow");

    assert_reserved_ingress_url(
        &scripted.requests(),
        &format!("Lash%2FWorkflow/{ENCODED_RESERVED_INGRESS_KEY}/run%3Fnow/send"),
    );
}

#[tokio::test]
pub(super) async fn restate_workflow_json_url_encodes_reserved_key() {
    let (client, scripted) = scripted_ingress_client([HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: HttpResponseBody::buffered("{}"),
    }]);

    client
        .call_workflow_json::<_, serde_json::Value>(
            "Lash/Workflow",
            RESERVED_INGRESS_KEY,
            "run?now",
            &serde_json::json!({}),
        )
        .await
        .expect("call workflow");

    assert_reserved_ingress_url(
        &scripted.requests(),
        &format!("Lash%2FWorkflow/{ENCODED_RESERVED_INGRESS_KEY}/run%3Fnow"),
    );
}

#[tokio::test]
pub(super) async fn restate_workflow_empty_url_encodes_reserved_key() {
    let (client, scripted) = scripted_ingress_client([HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: HttpResponseBody::buffered("{}"),
    }]);

    client
        .call_workflow_empty::<serde_json::Value>("Lash/Workflow", RESERVED_INGRESS_KEY, "run?now")
        .await
        .expect("call empty workflow");

    assert_reserved_ingress_url(
        &scripted.requests(),
        &format!("Lash%2FWorkflow/{ENCODED_RESERVED_INGRESS_KEY}/run%3Fnow"),
    );
}

#[tokio::test]
pub(super) async fn restate_object_json_url_encodes_reserved_key() {
    let (client, scripted) = scripted_ingress_client([HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: HttpResponseBody::buffered("{}"),
    }]);

    client
        .call_object_json::<_, serde_json::Value>(
            "Lash/Object",
            RESERVED_INGRESS_KEY,
            "run?now",
            &serde_json::json!({}),
        )
        .await
        .expect("call object");

    assert_reserved_ingress_url(
        &scripted.requests(),
        &format!("Lash%2FObject/{ENCODED_RESERVED_INGRESS_KEY}/run%3Fnow"),
    );
}

#[tokio::test]
pub(super) async fn restate_object_empty_url_encodes_reserved_key() {
    let (client, scripted) = scripted_ingress_client([HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: HttpResponseBody::buffered("{}"),
    }]);

    client
        .call_object_empty("Lash/Object", RESERVED_INGRESS_KEY, "run?now")
        .await
        .expect("call empty object");

    assert_reserved_ingress_url(
        &scripted.requests(),
        &format!("Lash%2FObject/{ENCODED_RESERVED_INGRESS_KEY}/run%3Fnow"),
    );
}

#[tokio::test]
pub(super) async fn restate_object_send_url_encodes_reserved_key() {
    let (client, scripted) = scripted_ingress_client([accepted_response("inv_object_send")]);

    client
        .send_object_json(
            "Lash/Object",
            RESERVED_INGRESS_KEY,
            "run?now",
            &serde_json::json!({}),
        )
        .await
        .expect("send object");

    assert_reserved_ingress_url(
        &scripted.requests(),
        &format!("Lash%2FObject/{ENCODED_RESERVED_INGRESS_KEY}/run%3Fnow/send"),
    );
}

#[tokio::test]
pub(super) async fn host_transport_injects_authorization_on_ingress_submit() {
    let scripted = Arc::new(ScriptedHttpTransport::new([accepted_response("inv_auth")]));
    let token = Arc::new(RwLock::new("cloud-token".to_string()));
    let decorated: Arc<dyn HttpTransport> = Arc::new(AuthorizationTransport {
        inner: scripted.clone(),
        token,
    });
    let connection = RestateConnection::with_transport("https://cloud.example", decorated);
    let client = RestateIngressClient::new(connection);

    client
        .send_service_json("LashService", "run", &serde_json::json!({"input": "hello"}))
        .await
        .expect("authenticated ingress submit");

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value == "Bearer cloud-token"
    }));
}

#[tokio::test]
pub(super) async fn ingress_unauthorized_error_mentions_status_401() {
    let scripted: Arc<dyn HttpTransport> = Arc::new(ScriptedHttpTransport::new([HttpResponse {
        status: 401,
        headers: Vec::new(),
        body: HttpResponseBody::buffered(r#"{"message":"missing bearer token"}"#),
    }]));
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        scripted,
    ));

    let error = client
        .send_service_json("LashService", "run", &serde_json::json!({}))
        .await
        .expect_err("unauthorized submit must fail");

    assert!(error.to_string().contains("status 401"), "{error}");
    assert!(
        error.to_string().contains("missing bearer token"),
        "{error}"
    );
}

#[tokio::test]
pub(super) async fn authorization_decorator_reads_rotated_credentials_per_request() {
    let scripted = Arc::new(ScriptedHttpTransport::new([
        accepted_response("inv_first"),
        accepted_response("inv_second"),
    ]));
    let token = Arc::new(RwLock::new("first-token".to_string()));
    let decorated: Arc<dyn HttpTransport> = Arc::new(AuthorizationTransport {
        inner: scripted.clone(),
        token: Arc::clone(&token),
    });
    let client = RestateIngressClient::new(RestateConnection::with_transport(
        "https://cloud.example",
        decorated,
    ));

    client
        .send_service_json("LashService", "run", &serde_json::json!({"attempt": 1}))
        .await
        .expect("first submit");
    *token.write_recover() = "second-token".to_string();
    client
        .send_service_json("LashService", "run", &serde_json::json!({"attempt": 2}))
        .await
        .expect("second submit");

    let authorization = scripted
        .requests()
        .into_iter()
        .map(|request| {
            request
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .expect("authorization header")
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(authorization, ["Bearer first-token", "Bearer second-token"]);
}

#[tokio::test]
pub(super) async fn restate_ingress_client_accepts_previously_accepted_send() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_duplicate","status":"PreviouslyAccepted"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let invocation_id = client
        .send_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "run",
            &serde_json::json!({ "process_id": "process-1" }),
        )
        .await
        .expect("idempotent duplicate send");
    server.await.expect("capture server");

    assert_eq!(invocation_id.as_str(), "inv_duplicate");
}

#[tokio::test]
pub(super) async fn restate_ingress_client_calls_workflow_and_decodes_output() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":{"ok":true}}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let output: ProcessAwaitOutput = client
        .call_workflow_json(
            "LashProcessWorkflow",
            "process-1",
            "await_terminal",
            &RestateProcessAwaitRequest {
                process_id: ProcessId::from("process-1"),
            },
        )
        .await
        .expect("call workflow");
    server.await.expect("capture server");

    assert_eq!(
        output,
        legacy_process_success(serde_json::json!({ "ok": true }))
    );
    let requests = captured.lock_recover();
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/process-1/await_terminal "),
        "unexpected request: {}",
        requests[0]
    );
    assert!(!requests[0].contains("/send "));
}

#[tokio::test]
pub(super) async fn restate_ingress_client_pins_effect_replay_with_idempotency_key() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"status":"cancelled"}"#,
    }])
    .await;
    let client = RestateIngressClient::new(base_url);

    let output: Resolution = client
        .call_workflow_json_idempotent(
            "LashDurableWaitWorkflow",
            "promise-key",
            "await_resolution",
            &serde_json::json!({}),
            "stable-envelope-hash",
        )
        .await
        .expect("call idempotent workflow");
    server.await.expect("capture server");

    assert_eq!(output, Resolution::Cancelled);
    let requests = captured.lock_recover();
    assert!(
        requests[0].contains("idempotency-key: stable-envelope-hash"),
        "explicit effect replay identity must reach Restate: {}",
        requests[0]
    );
}

pub(super) async fn await_process_terminal_until_terminal(
    process_work: &dyn lash_core::ProcessWorkSubstrate,
    process_ref: &lash_core::ProcessRef,
) -> Result<ProcessAwaitOutput, PluginError> {
    loop {
        match process_work.await_process_terminal(process_ref).await? {
            lash_core::ProcessTerminalWait::Terminal(output) => return Ok(output),
            lash_core::ProcessTerminalWait::Reattach => {}
        }
    }
}

#[tokio::test]
pub(super) async fn restate_process_attach_calls_await_terminal_ingress() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":"attached"}"#,
    }])
    .await;
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register attach target");
    let process_ref = lash_core::ProcessRef::from_record(&record);
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuation_store());

    let output = await_process_terminal_until_terminal(&runner, &process_ref)
        .await
        .expect("attach await");
    server.await.expect("capture server");

    assert_eq!(
        output,
        legacy_process_success(serde_json::json!("attached"))
    );
}

#[tokio::test]
pub(super) async fn cancel_during_successor_boundary_routes_root_and_await_terminal_resolves() {
    assert_eq!(
        terminal_completion_workflow_key(&ProcessId::from("retained-terminal"), 2),
        Some("retained-terminal".to_string())
    );
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("retained-terminal"))
        .await
        .expect("register");
    let expected = process_cancellation("cancelled after a long chain", None);
    registry
        .complete_process(
            &ProcessId::from("retained-terminal"),
            expected.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    let runner =
        RestateProcessIngressRunner::new("http://127.0.0.1:1", registry, continuation_store());

    assert_eq!(
        await_process_terminal_until_terminal(
            &runner,
            &lash_core::ProcessRef::from_record(&record),
        )
        .await
        .expect("registry terminal bypasses expired workflow key"),
        expected
    );
}

#[tokio::test]
pub(super) async fn restate_process_attach_maps_ingress_error_to_plugin_error() {
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"boom"}"#,
    }])
    .await;
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register attach error target");
    let process_ref = lash_core::ProcessRef::from_record(&record);
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuation_store());

    let err = await_process_terminal_until_terminal(&runner, &process_ref)
        .await
        .expect_err("attach error");
    server.await.expect("capture server");

    assert!(
        err.to_string()
            .contains("ingress await for process `process-1` failed")
    );
    assert!(err.to_string().contains("status 500"));
    assert!(err.to_string().contains("boom"));
}

#[tokio::test]
pub(super) async fn restate_process_attach_preserves_re_attach_signal_on_ceiling() {
    let (base_url, black_hole) = spawn_restate_http_black_hole().await;
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register attach ceiling target");
    let process_ref = lash_core::ProcessRef::from_record(&record);
    let runner = RestateProcessIngressRunner::new(
        RestateConnection::with_config(base_url, short_restate_timeouts(100, 25)),
        registry,
        continuation_store(),
    );

    let wait = runner
        .await_terminal_wait(&process_ref)
        .await
        .expect("attach ceiling must request host re-attachment");
    black_hole.abort();
    let _ = black_hole.await;

    assert_eq!(wait, lash_core::ProcessTerminalWait::Reattach);
}

#[tokio::test]
pub(super) async fn restate_process_attach_reattaches_after_timeout_until_terminal() {
    let expected = legacy_process_success(serde_json::json!({"reattached": true}));
    let (base_url, captured, server) = spawn_restate_http_timeout_then_capture(MockHttpResponse {
        status: "200 OK",
        body: r#"{"type":"success","value":{"reattached":true}}"#,
    })
    .await;
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register reattach target");
    let process_ref = lash_core::ProcessRef::from_record(&record);
    let runner = RestateProcessIngressRunner::new(
        RestateConnection::with_config(base_url, short_restate_timeouts(100, 25)),
        registry,
        continuation_store(),
    );

    let output = loop {
        match runner
            .await_process_terminal(&process_ref)
            .await
            .expect("process port re-enters after a bounded wait timeout")
        {
            lash_core::ProcessTerminalWait::Terminal(output) => break output,
            lash_core::ProcessTerminalWait::Reattach => {}
        }
    };
    server.await.expect("timeout-then-terminal server");

    assert_eq!(output, expected);
    let requests = captured.lock_recover();
    assert_eq!(requests.len(), 2, "attach must re-enter with the same id");
    assert!(
        requests
            .iter()
            .all(|request| request
                .starts_with("POST /LashProcessWorkflow/process-1/await_terminal "))
    );
}

#[tokio::test]
pub(super) async fn restate_turn_attach_preserves_re_attach_code_on_ceiling() {
    let (base_url, black_hole) = spawn_restate_http_black_hole().await;
    let attach = RestateTurnAttach::new(RestateConnection::with_config(
        base_url,
        short_restate_timeouts(100, 25),
    ));

    let error = attach
        .await_terminal(&TurnAddress::new("session-1", "turn-1"))
        .await
        .expect_err("attach ceiling must be coded for host re-attachment");
    black_hole.abort();
    let _ = black_hole.await;

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed
    );
    assert!(error.is_retryable());
}

/// Like [`spawn_restate_http_capture`], but holds each accepted connection open
/// for `delay` before responding, modeling a durable promise that resolves only
/// once the workflow's `run` completes.
pub(super) async fn spawn_restate_http_capture_delayed(
    responses: Vec<MockHttpResponse>,
    delay: std::time::Duration,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            captured_server.lock_recover().push(request);
            tokio::time::sleep(delay).await;
            let body = response.body.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status,
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write response headers");
            socket.write_all(body).await.expect("write response body");
            socket.flush().await.expect("flush");
        }
    });
    (format!("http://{addr}"), captured, server)
}

#[tokio::test]
pub(super) async fn restate_attach_before_run_resolves_with_delayed_workflow_output() {
    // The ingress attach is a synchronous long-hold call issued while the
    // workflow's `run` is still in flight; it resolves only when the durable
    // promise does. A delayed mock stands in for that hold, and the eventual
    // output flows back through the driver's attach.
    let delay = std::time::Duration::from_millis(300);
    let (base_url, captured, server) = spawn_restate_http_capture_delayed(
        vec![MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":{"eventual":true}}"#,
        }],
        delay,
    )
    .await;
    let registry = process_registry();
    let deployment =
        RestateProcessDeployment::new(base_url, Arc::clone(&registry), continuation_store());
    let driver = deployment.test_process_work();
    // A non-terminal process routes await_terminal through the ingress attach
    // rather than the registry short-circuit.
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register non-terminal process");

    let started = std::time::Instant::now();
    let output = driver
        .await_process_terminal(&lash_core::ProcessRef::from_record(&record))
        .await
        .expect("attach await resolves with the eventual output");
    let lash_core::ProcessTerminalWait::Terminal(output) = output else {
        panic!("delayed successful attach unexpectedly requested re-attachment")
    };
    let elapsed = started.elapsed();
    server.await.expect("capture server");

    assert_eq!(
        output,
        legacy_process_success(serde_json::json!({ "eventual": true }))
    );
    assert!(
        elapsed >= delay,
        "the attach must block on the durable promise until run resolves (waited {elapsed:?})"
    );
    let requests = captured.lock_recover();
    assert_eq!(
        requests.len(),
        1,
        "await_terminal issues exactly one ingress call"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/process-1/await_terminal "),
        "unexpected request: {}",
        requests[0]
    );
}

#[tokio::test]
pub(super) async fn restate_driver_short_circuits_terminal_without_ingress_call() {
    // Empty response set: the capture server accepts nothing, so any ingress
    // call would fail. The registry terminal short-circuit must fire first, so
    // the attach is never consulted for an already-terminal process.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![]).await;
    let registry = process_registry();
    let deployment =
        RestateProcessDeployment::new(base_url, Arc::clone(&registry), continuation_store());
    let driver = deployment.test_process_work();
    let output = process_success(serde_json::json!("already-terminal"));
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register");
    registry
        .complete_process(
            &ProcessId::from("process-1"),
            output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let resolved = driver
        .await_process_terminal(&lash_core::ProcessRef::from_record(&record))
        .await
        .expect("terminal short-circuit resolves without ingress");
    let lash_core::ProcessTerminalWait::Terminal(resolved) = resolved else {
        panic!("terminal registry row unexpectedly requested re-attachment")
    };
    server.await.expect("capture server");

    assert_eq!(resolved, output);
    assert!(
        captured.lock_recover().is_empty(),
        "a terminal short-circuit must not issue any ingress call"
    );
}

#[tokio::test]
pub(super) async fn restate_process_attach_maps_malformed_ingress_body_to_plugin_error() {
    // A 2xx response whose body does not decode into ProcessAwaitOutput must
    // surface as a PluginError, not a panic — complementing the non-2xx case.
    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "200 OK",
        body: "this-is-not-valid-json",
    }])
    .await;
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register malformed-body target");
    let process_ref = lash_core::ProcessRef::from_record(&record);
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuation_store());

    let err = await_process_terminal_until_terminal(&runner, &process_ref)
        .await
        .expect_err("a malformed ingress body must surface as an error");
    server.await.expect("capture server");

    assert!(
        err.to_string()
            .contains("ingress await for process `process-1` failed"),
        "unexpected error: {err}"
    );
}

/// Records each pushed event's `(event_type, sequence)` in emit order, and
/// every worker fault the handle reports.
#[derive(Clone, Default)]
pub(super) struct RecordingProcessEventSink {
    events: Arc<Mutex<Vec<(String, u64)>>>,
    faults: Arc<Mutex<Vec<lash_core::facade_support::ProcessWorkerFault>>>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for RecordingProcessEventSink {
    async fn emit(&self, event: &lash_core::ProcessEvent) {
        self.events
            .lock_recover()
            .push((event.event_type.clone(), event.sequence));
    }

    async fn emit_worker_fault(&self, fault: &lash_core::facade_support::ProcessWorkerFault) {
        self.faults.lock_recover().push(fault.clone());
    }
}

#[tokio::test]
pub(super) async fn restate_deployment_sink_funnel_feeds_appended_events() {
    // ADR 0017 names `RestateProcessDeployment::new_with_sink` as the durable
    // hosts' wrap funnel: a sink installed there observes every append made
    // through the deployment's shared registry, including terminal events.
    let sink = RecordingProcessEventSink::default();
    let deployment = RestateProcessDeployment::new_with_sink(
        "http://127.0.0.1:8080",
        process_registry(),
        continuation_store(),
        Some(Arc::new(sink.clone())),
    );
    let registry = deployment.test_registry();
    registry
        .register_process(
            external_registration("sink-funnel").with_extra_event_types([
                lash_core::ProcessEventType {
                    name: "producer.tick".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
        )
        .await
        .expect("register");
    registry
        .append_event(
            &ProcessId::from("sink-funnel"),
            lash_core::ProcessEventAppendRequest::new("producer.tick", serde_json::json!({})),
        )
        .await
        .expect("append");
    registry
        .complete_process(
            &ProcessId::from("sink-funnel"),
            process_success(serde_json::Value::Null),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let events = sink.events.lock_recover().clone();
    assert_eq!(
        events
            .iter()
            .map(|(event_type, _)| event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["producer.tick", "process.completed"],
        "the deployment-wrapped registry feeds every append to the sink"
    );
    assert!(
        events[0].1 < events[1].1,
        "the deployment sink preserves strictly ordered event sequences"
    );
}

#[tokio::test]
pub(super) async fn restate_process_attach_is_reentrant_across_sequential_awaits() {
    // The shared await_terminal handler is re-entrant: two sequential attaches
    // each issue an independent ingress call and both succeed.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"first"}"#,
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"type":"success","value":"second"}"#,
        },
    ])
    .await;
    let registry = process_registry();
    let record = registry
        .register_process(external_registration("process-1"))
        .await
        .expect("register reentrant attach target");
    let process_ref = lash_core::ProcessRef::from_record(&record);
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuation_store());

    let first = await_process_terminal_until_terminal(&runner, &process_ref)
        .await
        .expect("first attach await");
    let second = await_process_terminal_until_terminal(&runner, &process_ref)
        .await
        .expect("second attach await");
    server.await.expect("capture server");

    assert_eq!(first, legacy_process_success(serde_json::json!("first")));
    assert_eq!(second, legacy_process_success(serde_json::json!("second")));
    assert_eq!(
        captured.lock_recover().len(),
        2,
        "each await issues an independent ingress call"
    );
}

#[tokio::test]
pub(super) async fn restate_admin_client_cancels_kills_and_queries_invocation_status() {
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "202 Accepted",
            body: "",
        },
        MockHttpResponse {
            status: "200 OK",
            body: "",
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"rows":[{"id":"inv_123","target":"WorkbenchTurnWorkflow/turn-1/run","target_service_name":"WorkbenchTurnWorkflow","target_service_key":"turn-1","target_handler_name":"run","status":"completed","completion_result":"success","completion_failure":null}]}"#,
        },
        MockHttpResponse {
            status: "200 OK",
            body: r#"{"rows":[{"id":"inv_456","target":"WorkbenchTurnWorkflow/turn-2/run","target_service_name":"WorkbenchTurnWorkflow","target_service_key":"turn-2","target_handler_name":"run","status":"suspended"}]}"#,
        },
    ])
    .await;
    let client = RestateAdminClient::new(base_url);
    let invocation_id = RestateInvocationId::new("inv_123");

    client
        .cancel_invocation(&invocation_id)
        .await
        .expect("cancel");
    client
        .kill_invocation_for_test_cleanup(&invocation_id)
        .await
        .expect("kill");
    let status = client
        .invocation_status(&invocation_id)
        .await
        .expect("status")
        .expect("status row");
    let workflow_status = client
        .workflow_invocation_status("WorkbenchTurnWorkflow", "turn-2", "run")
        .await
        .expect("workflow status")
        .expect("workflow status row");
    server.await.expect("capture server");

    assert!(status.completed_successfully());
    assert_eq!(status.target_service_name, "WorkbenchTurnWorkflow");
    assert!(workflow_status.is_still_active());
    let requests = captured.lock_recover();
    assert!(
        requests[0].starts_with("PATCH /invocations/inv_123/cancel "),
        "unexpected cancel request: {}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("PATCH /invocations/inv_123/kill "),
        "unexpected kill request: {}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("POST /query "),
        "unexpected query request: {}",
        requests[2]
    );
    assert!(requests[2].contains("FROM sys_invocation WHERE id = 'inv_123'"));
    assert!(requests[3].contains(
        "target_service_name = 'WorkbenchTurnWorkflow' AND target_service_key = 'turn-2' AND target_handler_name = 'run'"
    ));
}

/// A submit that fails mid-pass is that row's outcome, not the pass's. Failing
/// the call would throw away the ids that already reached the ingress, so the
/// failure rides back as a typed per-row deferral instead.
#[tokio::test]
pub(super) async fn a_failed_ingress_submit_defers_its_row_without_discarding_the_pass() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("submit-fails"))
        .await
        .expect("register the row whose submit fails");

    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"ingress unavailable"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuation_store());
    let report = runner
        .admit_pending_processes("test")
        .await
        .expect("a per-row submit failure does not fail the pass");
    server.await.expect("mock ingress server task");

    assert!(report.admitted.is_empty());
    assert_eq!(report.deferred.len(), 1, "{report:?}");
    assert_eq!(report.deferred[0].process_id, "submit-fails");
    let ProcessRecoveryAttemptOutcome::BackendError { operation, .. } =
        &report.deferred[0].disposition
    else {
        panic!(
            "expected a typed backend error, got {:?}",
            report.deferred[0]
        );
    };
    assert_eq!(*operation, ProcessRecoveryOperation::SubmitRun);
}

/// A per-row deferral only reaches a host that reads the report, and every
/// in-tree caller discards it. The fault surface is the path that does not
/// depend on anyone reading a return value, so a failed ingress submit has to
/// arrive there too.
#[tokio::test]
pub(super) async fn a_failed_ingress_submit_reports_a_worker_fault_to_the_sink() {
    let sink = RecordingProcessEventSink::default();
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("submit-fails-loudly"))
        .await
        .expect("register the row whose submit fails");

    let (base_url, _captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "500 Internal Server Error",
        body: r#"{"message":"ingress unavailable"}"#,
    }])
    .await;
    let runner = RestateProcessIngressRunner::new(base_url, registry, continuation_store())
        .with_event_sink(Some(Arc::new(sink.clone())));
    let report = runner
        .admit_pending_processes("test")
        .await
        .expect("a per-row submit failure does not fail the pass");
    server.await.expect("mock ingress server task");
    assert_eq!(report.deferred.len(), 1, "{report:?}");

    let faults = sink.faults.lock_recover().clone();
    assert_eq!(faults.len(), 1, "{faults:?}");
    let lash_core::facade_support::ProcessWorkerFault::RecoveryBackendError {
        process_id,
        operation,
        ..
    } = &faults[0]
    else {
        panic!("expected a recovery backend fault, got {:?}", faults[0]);
    };
    assert_eq!(process_id, "submit-fails-loudly");
    assert_eq!(*operation, ProcessRecoveryOperation::SubmitRun);
}

/// Every wait of a non-session scope is owned by that scope's own
/// `LashDurableWaitIndex` object, keyed by the scope's journal identity, so a
/// scope-exact retirement can revoke and fence the whole scope in one keyed
/// handler; session waits keep the session's object (FIG-2499 fix round 1).
#[test]
pub(super) fn durable_wait_index_is_keyed_by_scope_for_session_free_waits() {
    let process = lash_core::ExecutionScope::process("proc-1");
    let operation = lash_core::ExecutionScope::runtime_operation("op-1");
    let process_key = crate::durable_wait::durable_wait_index_key_for_scope(&process);
    let operation_key = crate::durable_wait::durable_wait_index_key_for_scope(&operation);
    assert_eq!(
        process_key,
        format!(
            "scope:{}",
            process.journal_identity().expect("process identity").key()
        )
    );
    assert_ne!(process_key, operation_key);
    let session = lash_core::ExecutionScope::turn("session-1", "turn-1");
    assert_eq!(
        crate::durable_wait::durable_wait_index_key_for_scope(&session),
        "session-1"
    );
    for (scope, wait) in [
        (
            process.clone(),
            AwaitEventWaitIdentity::tool_completion("a"),
        ),
        (
            process.clone(),
            AwaitEventWaitIdentity::tool_completion("b"),
        ),
    ] {
        let key = restate_await_event_key(&scope, wait).expect("mint");
        assert_eq!(
            RestateDurableWaitAddress::for_key(&key).index_key(),
            process_key,
            "every wait of one scope shares the scope's index object"
        );
    }
}

/// With no Restate reachable, a scope-exact retirement is a typed failure —
/// never a silent `Ok(0)` that would let the scope mint again — and a mint
/// under a session-free scope consults the durable fence before minting,
/// exactly as a session-bearing scope already did (FIG-2499 review round 1).
#[tokio::test]
pub(super) async fn scope_retirement_and_mint_consult_restate_rather_than_answering_locally() {
    let host = crate::RestateEffectHost::new(crate::RestateConnection::new("http://127.0.0.1:1"));
    let scope = lash_core::ExecutionScope::runtime_operation("unreachable-op");
    let retirement = host
        .retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&scope)
                .expect("runtime operations retire"),
        )
        .await
        .expect_err("retirement without a reachable Restate is a typed failure");
    assert_eq!(
        retirement.code,
        lash_core::RuntimeErrorCode::RestateAwaitEventSessionUpdate
    );
    let mint = host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("late"))
        .await
        .expect_err("a session-free mint reads the durable fence first");
    assert_eq!(
        mint.code,
        lash_core::RuntimeErrorCode::RestateAwaitEventRevocationRead
    );
    let reinstate = host
        .reinstate_effect_scope(&lash_core::ExecutionScope::process("unreachable-process"))
        .await
        .expect_err("reinstatement without a reachable Restate is a typed failure");
    assert_eq!(
        reinstate.code,
        lash_core::RuntimeErrorCode::RestateAwaitEventSessionUpdate
    );
    let session = host
        .reinstate_effect_scope(&lash_core::ExecutionScope::turn("s", "t"))
        .await
        .expect_err("session scopes are refused before any ingress call");
    assert_eq!(
        session.code,
        lash_core::RuntimeErrorCode::AwaitEventScopeNotRetirable
    );
}
