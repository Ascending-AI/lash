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
        assert_eq!(still_running.status, process.status);
        assert!(still_running.outcome.is_none());
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
                .outcome
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
    );

    assert!(matches!(runner.await_process_terminal(&process.id).await,
        Err(PluginError::ProcessCallerDeparted { process_id }) if process_id == process.id));
    assert!(ingress.requests.lock_recover().is_empty());
    let record = registry
        .get_process(&process.id)
        .await
        .expect("read")
        .expect("retained");
    assert_eq!(record.status, lash_core::ProcessStatus::CallerDeparted);
    assert!(record.outcome.is_none());
}

fn child_watch(ingress: &Arc<ScriptedIngress>) -> Arc<dyn lash_core::GroupChildCancelWatch> {
    crate::effect_group::GroupChildCancel::new(
        RestateIngressClient::new(ingress.connection()),
        crate::RestateNamespace::default(),
        "faulting-child-cancel".to_string(),
        0,
    )
    .watch()
}

fn assert_shared_backoff(ingress: &ScriptedIngress) {
    let times = ingress.request_times.lock_recover();
    assert_eq!(
        times.len(),
        8,
        "the shared ladder makes exactly eight requests"
    );
    let delays: Vec<_> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(
        delays,
        [25, 50, 100, 200, 400, 800, 1000].map(Duration::from_millis)
    );
    let requests = ingress.requests.lock_recover();
    for request in requests.iter().skip(1) {
        assert_eq!(request.url, requests[0].url);
        assert_eq!(request.body, requests[0].body);
    }
}

#[tokio::test(start_paused = true)]
async fn child_cancel_watch_exhausts_actual_ingress_faults() {
    for reset in [false, true] {
        let ingress = ScriptedIngress::new(
            (0..1000)
                .map(|_| {
                    if reset {
                        transport_failure("connection reset")
                    } else {
                        response(503, "ingress unavailable")
                    }
                })
                .collect(),
        );
        let watch = child_watch(&ingress);
        let lost = tokio::time::timeout(
            Duration::from_secs(3),
            lash_core::retry_cancel_watch("child cancel law", || watch.cancelled()),
        )
        .await
        .expect("actual faults must exhaust the ladder")
        .expect_err("eight ingress faults lose the watch");
        assert_eq!(lost.code, lash_core::RuntimeErrorCode::TransientCancelWatch);
        assert_shared_backoff(&ingress);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(ingress.requests.lock_recover().len(), 8);
    }
}

#[tokio::test(start_paused = true)]
async fn child_cancel_watch_succeeds_on_the_eighth_attempt() {
    let ingress = ScriptedIngress::new(
        (0..7)
            .map(|_| response(503, "ingress unavailable"))
            .chain([reply(crate::effect_group::EffectGroupNotification::Cancel)])
            .collect(),
    );
    let watch = child_watch(&ingress);
    lash_core::retry_cancel_watch("child cancel law", || watch.cancelled())
        .await
        .expect("the eighth attempt may still observe cancellation");
    assert_shared_backoff(&ingress);
}

#[tokio::test(start_paused = true)]
async fn child_cancel_watch_reattaches_only_for_attach_timeouts() {
    let ingress = ScriptedIngress::new(
        (0..12)
            .map(|_| {
                Err(LlmTransportError::new("attach ceiling elapsed")
                    .with_kind(lash_core::ProviderFailureKind::Timeout))
            })
            .chain([reply(crate::effect_group::EffectGroupNotification::Cancel)])
            .collect(),
    );
    let watch = child_watch(&ingress);
    lash_core::retry_cancel_watch("child cancel law", || watch.cancelled())
        .await
        .expect("healthy attach ceilings do not exhaust the fault ladder");
    assert_eq!(ingress.requests.lock_recover().len(), 13);
    let times = ingress.request_times.lock_recover();
    assert!(
        times.iter().all(|time| *time == times[0]),
        "reattachment adds no fault backoff"
    );
}

#[tokio::test(start_paused = true)]
async fn child_cancel_watch_settled_never_cancels_or_reattaches() {
    let ingress = ScriptedIngress::new(vec![reply(
        crate::effect_group::EffectGroupNotification::Settled,
    )]);
    let watch = child_watch(&ingress);
    assert!(
        tokio::time::timeout(Duration::from_secs(60), watch.cancelled())
            .await
            .is_err()
    );
    assert_eq!(ingress.requests.lock_recover().len(), 1);
}

struct HeldTool {
    definition: lash_core::ToolDefinition,
    gate: Arc<tokio::sync::Semaphore>,
    runs: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

struct BodyDropWitness(Option<Arc<AtomicBool>>);

impl Drop for BodyDropWitness {
    fn drop(&mut self) {
        if let Some(dropped) = &self.0 {
            dropped.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for HeldTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition.manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let mut witness = BodyDropWitness(Some(self.dropped.clone()));
        self.runs.fetch_add(1, Ordering::SeqCst);
        let _permit = self.gate.acquire().await.expect("body gate stays open");
        assert!(
            !call
                .context
                .cancellation_token()
                .expect("child attempt has a stop")
                .is_cancelled()
        );
        witness.0 = None;
        lash_core::ToolOutcome::ok(serde_json::json!({"done": true})).into()
    }
}

async fn start_tool_child(
    ingress: &Arc<ScriptedIngress>,
) -> (
    tokio::task::JoinHandle<Vec<u8>>,
    Arc<tokio::sync::Semaphore>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
) {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let definition = lash_core::ToolDefinition::raw(
        "tool:held",
        "held",
        "Held body",
        serde_json::json!({"type": "object"}),
        serde_json::json!({"type": "object"}),
    );
    let provider = Arc::new(HeldTool {
        definition: definition.clone(),
        gate: gate.clone(),
        runs: runs.clone(),
        dropped: dropped.clone(),
    });
    let child = crate::effect_group::GroupChildCancel::new(
        RestateIngressClient::new(ingress.connection()),
        crate::RestateNamespace::default(),
        "recorded-tool-child".to_string(),
        0,
    );
    let journal = Arc::new(ReplayableRecordingContext::default());
    let controller = Arc::new(
        RestateRuntimeEffectController::new_for_test(journal.clone())
            .with_group_child_cancel(child),
    );
    let run = tokio::spawn(async move {
        let run_once = || async {
            Box::pin(lash_core::testing::coordinate_tool_provider_with_services(
                ScopedEffectController::shared(
                    controller.clone(),
                    lash_core::AdmittedScope::runtime_operation("recorded-tool-child"),
                )
                .expect("admitted child controller"),
                Arc::new(lash_core::testing::MockSessionManager::default()),
                &SessionId::from("child-law"),
                definition.clone(),
                provider.clone(),
                prepared_tool_call_with("held-call", "held"),
            ))
            .await
            .expect("the tool body completes despite losing its watch")
        };
        let (completed, outcome) = run_once().await;
        assert!(completed.output.is_success());
        let bytes = serde_json::to_vec(&outcome).expect("recorded tool outcome");
        journal.start_replay();
        let (_, replay) = run_once().await;
        assert_eq!(
            serde_json::to_vec(&replay).expect("replayed outcome"),
            bytes
        );
        bytes
    });
    (run, gate, runs, dropped)
}

async fn until_child_requests(ingress: &ScriptedIngress, count: usize) {
    for _ in 0..10_000 {
        if ingress.requests.lock_recover().len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("child did not issue request {count}");
}

#[tokio::test(start_paused = true)]
async fn child_cancel_watch_exhausts_actual_ingress_faults_under_both_callers() {
    for tool in [true, false] {
        for reset in [false, true] {
            let ingress = ScriptedIngress::new(
                (0..1000)
                    .map(|_| {
                        if reset {
                            transport_failure("connection reset")
                        } else {
                            response(503, "ingress unavailable")
                        }
                    })
                    .collect(),
            );
            let (run, gate, runs, dropped) = if tool {
                let (run, gate, runs, dropped) = start_tool_child(&ingress).await;
                (run, gate, runs, Some(dropped))
            } else {
                let (run, gate, runs) =
                    super::effect_group_child_cancel::start_atomic_child(ingress.clone()).await;
                (run, gate, runs, None)
            };
            until_child_requests(&ingress, 1).await;
            for (attempt, delay) in [25, 50, 100, 200, 400, 800, 1000].into_iter().enumerate() {
                tokio::time::advance(Duration::from_millis(delay - 1)).await;
                tokio::task::yield_now().await;
                assert_eq!(
                    ingress.requests.lock_recover().len(),
                    attempt + 1,
                    "no early retry for caller tool={tool}"
                );
                tokio::time::advance(Duration::from_millis(1)).await;
                until_child_requests(&ingress, attempt + 2).await;
            }
            assert_shared_backoff(&ingress);
            tokio::time::advance(Duration::from_secs(60)).await;
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                ingress.requests.lock_recover().len(),
                8,
                "the lost watch must stop sending for caller tool={tool}"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            assert!(!run.is_finished(), "watch loss leaves the body running");
            if let Some(dropped) = dropped {
                assert!(
                    !dropped.load(Ordering::SeqCst),
                    "watch loss never drops the tool"
                );
            }
            gate.add_permits(1);
            run.await
                .expect("child completes and records its body outcome");
            assert_eq!(
                runs.load(Ordering::SeqCst),
                1,
                "replay never runs the tool again"
            );
            assert_eq!(ingress.requests.lock_recover().len(), 8);
        }
    }
}

#[tokio::test]
async fn child_cancel_watch_unregistered_target_is_typed_and_terminal() {
    let ingress = ScriptedIngress::new(vec![response(404, "service not found")]);
    let child = crate::effect_group::GroupChildCancel::new(
        RestateIngressClient::new(ingress.connection()),
        crate::RestateNamespace::default(),
        "unregistered-child-cancel".to_string(),
        0,
    );

    let error = child
        .watch()
        .cancelled()
        .await
        .expect_err("no service binds this target");
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::EngineServiceUnregistered
    );
    assert!(error.code.is_terminal());
    assert_eq!(ingress.requests.lock_recover().len(), 1);
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
        delivery.drive(std::future::pending::<()>()),
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
    let body =
        serde_json::json!({"message": serde_json::to_string(&refusal).expect("typed refusal")});
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
            "drive",
            &serde_json::json!({"request": "drive-1"}),
            "drive-1",
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
                .any(|(name, value)| name == "idempotency-key" && value == "drive-1")
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
            .send_object_json_idempotent_bounded("LashSession", "session", "drive", &(), "drive-1")
            .await
            .expect_err("a definitive refusal cannot improve with a retry");
        assert_eq!(
            error.classification(),
            crate::RestateHttpErrorClass::Terminal
        );
        assert_eq!(ingress.requests.lock_recover().len(), 1);
    }
}
