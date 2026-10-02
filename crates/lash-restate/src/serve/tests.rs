use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use lash_core::RuntimeEffectController;
use lash_restate_test::protocol::generated::{InputCommandMessage, StartMessage, Value};
use lash_restate_test::protocol::{MessageType, ProtocolVersion, encode_message};
use restate_sdk::endpoint::Endpoint;
use restate_sdk::prelude::*;

struct BudgetProbe;

#[restate_sdk::service]
impl BudgetProbe {
    #[handler]
    async fn run(&self, _ctx: Context<'_>, input: Bytes) -> HandlerResult<Bytes> {
        Ok(input)
    }

    #[handler]
    async fn replay(&self, ctx: Context<'_>, _input: Bytes) -> HandlerResult<Json<u32>> {
        let mut total = 0;
        for id in 1..=128 {
            let Json(value) = ctx
                .run(|| async { Ok::<_, HandlerError>(Json(1_u32)) })
                .name(format!("step-{id}"))
                .await?;
            total += value;
        }
        Ok(Json(total))
    }
}

fn endpoint() -> Endpoint {
    Endpoint::builder().bind(BudgetProbe).build()
}

fn prefix() -> Bytes {
    encode_message(
        MessageType::Start,
        &StartMessage {
            id: Bytes::from_static(b"budget-probe"),
            debug_id: "budget-probe".into(),
            known_entries: 1,
            partial_state: true,
            ..Default::default()
        },
    )
}

fn input_header(length: u32) -> Bytes {
    Bytes::copy_from_slice(&((0x0400_u64 << 48) | u64::from(length)).to_be_bytes())
}

async fn raw_http2_refuses_header(length: u32, limits: super::RestateEndpointLimits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(super::serve_endpoint(listener, endpoint(), limits, async {
        let _ = stopped.await;
    }));
    let (mut sender, body) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("HTTP/2 client");
    let response = client
        .post(format!("http://{address}/invoke/BudgetProbe/run"))
        .header("content-type", ProtocolVersion::V6.content_type())
        .body(reqwest::Body::wrap(body))
        .send()
        .await
        .expect("HTTP/2 response head");
    sender.send_data(prefix()).await.expect("valid prefix");
    for byte in input_header(length) {
        sender
            .send_data(Bytes::copy_from_slice(&[byte]))
            .await
            .expect("split protocol header");
    }
    // Keep the upload open without ever supplying a payload.
    let output = tokio::time::timeout(Duration::from_secs(3), response.bytes()).await;
    drop(sender);
    let _ = stop.send(());
    server.await.expect("fixture shutdown");
    let output = output
        .expect("oversized declaration must refuse without waiting for payload")
        .expect("protocol refusal");
    assert!(
        output.is_empty(),
        "oversized input must end before a handler result: {output:?}"
    );
}

#[tokio::test]
async fn raw_http2_budget_plus_one_refuses_before_payload() {
    raw_http2_refuses_header(65, super::RestateEndpointLimits::new(64, 72)).await;
}

#[tokio::test]
async fn raw_http2_u32_max_refuses_before_payload() {
    raw_http2_refuses_header(u32::MAX, super::RestateEndpointLimits::new(64, 72)).await;
}

struct CountedBody {
    chunks: VecDeque<Bytes>,
    read: Arc<AtomicUsize>,
}

impl Body for CountedBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.chunks.pop_front() {
            Some(chunk) => {
                self.read.fetch_add(chunk.len(), Ordering::SeqCst);
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            None => Poll::Pending,
        }
    }
}

#[tokio::test]
async fn incomplete_input_stops_reading_at_the_refused_header() {
    let read = Arc::new(AtomicUsize::new(0));
    let prefix = prefix();
    let expected = prefix.len() + 8;
    let body = CountedBody {
        chunks: [prefix, input_header(u32::MAX), Bytes::from(vec![0; 128])].into(),
        read: read.clone(),
    };
    let request = http::Request::builder()
        .uri("/invoke/BudgetProbe/run")
        .header("content-type", ProtocolVersion::V6.content_type())
        .body(body)
        .expect("fixture request");
    let mut response = super::handle_endpoint(
        &endpoint(),
        request,
        super::RestateEndpointLimits::new(64, 72),
    )
    .into_body();
    std::future::poll_fn(|cx| {
        let _ = Pin::new(&mut response).poll_frame(cx);
        Poll::Ready(())
    })
    .await;
    assert_eq!(read.load(Ordering::SeqCst), expected, "payload was read");
}

#[tokio::test]
async fn legal_input_at_the_message_budget_succeeds() {
    let input = encode_message(
        MessageType::InputCommand,
        &InputCommandMessage {
            value: Some(Value {
                content: Bytes::from(vec![b'x'; 60]),
            }),
            ..Default::default()
        },
    );
    assert_eq!(input.len(), 8 + 64);
    let bytes = [prefix().as_ref(), input.as_ref()].concat();
    let request = http::Request::builder()
        .uri("/invoke/BudgetProbe/run")
        .header("content-type", ProtocolVersion::V6.content_type())
        .body(http_body_util::Full::new(Bytes::from(bytes)))
        .expect("fixture request");
    let output = super::handle_endpoint(
        &endpoint(),
        request,
        super::RestateEndpointLimits::new(64, 72),
    )
    .into_body()
    .collect()
    .await
    .expect("response")
    .to_bytes();
    assert!(output.windows(60).any(|bytes| bytes == [b'x'; 60]));
}

#[tokio::test]
async fn raw_http2_pending_budget_plus_one_refuses_before_payload() {
    raw_http2_refuses_header(57, super::RestateEndpointLimits::new(64, 64)).await;
}

#[tokio::test]
async fn coalesced_oversized_payload_never_reaches_the_sdk() {
    let bytes = [prefix().as_ref(), input_header(65).as_ref(), &[0; 65]].concat();
    let mut body = super::MessageBody::new(
        http_body_util::Full::new(Bytes::from(bytes)),
        super::RestateEndpointLimits::new(64, 72),
    );
    let error = body
        .frame()
        .await
        .expect("size refusal")
        .expect_err("coalesced payload refused");
    assert!(error.to_string().contains("Restate message declares 65"));
    assert!(body.frame().await.is_none(), "refused stream must stop");
}

fn replay_bytes() -> Bytes {
    use lash_restate_test::protocol::generated::{
        RunCommandMessage, RunCompletionNotificationMessage, run_completion_notification_message,
    };
    let mut stream = encode_message(
        MessageType::Start,
        &StartMessage {
            id: Bytes::from_static(b"budget-replay"),
            debug_id: "budget-replay".into(),
            known_entries: 257,
            partial_state: true,
            ..Default::default()
        },
    )
    .to_vec();
    stream.extend_from_slice(&encode_message(
        MessageType::InputCommand,
        &InputCommandMessage {
            value: Some(Value {
                content: Bytes::new(),
            }),
            ..Default::default()
        },
    ));
    for id in 1..=128 {
        stream.extend_from_slice(&encode_message(
            MessageType::RunCommand,
            &RunCommandMessage {
                result_completion_id: id,
                name: format!("step-{id}"),
            },
        ));
        stream.extend_from_slice(&encode_message(
            MessageType::RunCompletionNotification,
            &RunCompletionNotificationMessage {
                completion_id: id,
                result: Some(run_completion_notification_message::Result::Value(Value {
                    content: Bytes::from_static(b"1"),
                })),
            },
        ));
    }
    Bytes::from(stream)
}

#[tokio::test]
async fn raw_http2_long_replay_of_legal_messages_succeeds() {
    use lash_restate_test::protocol::{
        FrameDecoder,
        generated::{OutputCommandMessage, output_command_message},
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(super::serve_endpoint(
        listener,
        endpoint(),
        super::RestateEndpointLimits::new(64, 72),
        async {
            let _ = stopped.await;
        },
    ));
    let stream = replay_bytes();
    assert!(stream.len() > 72 * 50);
    let output = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("client")
        .post(format!("http://{address}/invoke/BudgetProbe/replay"))
        .header("content-type", ProtocolVersion::V6.content_type())
        .body(stream)
        .send()
        .await
        .expect("response")
        .bytes()
        .await
        .expect("response bytes");
    let mut decoder = FrameDecoder::default();
    decoder.push(&output);
    let frames = decoder.drain().expect("valid protocol response");
    let result = frames
        .iter()
        .find(|frame| frame.ty == MessageType::OutputCommand)
        .expect("completed replay")
        .decode::<OutputCommandMessage>()
        .expect("output");
    assert_eq!(
        result.result,
        Some(output_command_message::Result::Value(Value {
            content: Bytes::from_static(b"128")
        }))
    );
    assert!(frames.iter().any(|frame| frame.ty == MessageType::End));
    let _ = stop.send(());
    server.await.expect("shutdown");
}

#[allow(
    clippy::disallowed_methods,
    reason = "the live gate supplies isolated server addresses"
)]
#[tokio::test]
#[ignore = "requires the pinned live Restate server"]
async fn live_restate_bounded_endpoint_serves_many_messages() {
    let admin = std::env::var("RESTATE_ADMIN_URL").expect("live admin URL");
    let ingress = std::env::var("RESTATE_INGRESS_URL").expect("live ingress URL");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(super::serve_endpoint(
        listener,
        endpoint(),
        super::RestateEndpointLimits::new(1024, 1032),
        async {
            let _ = stopped.await;
        },
    ));
    let client = reqwest::Client::new();
    let registered = client
        .post(format!("{admin}/deployments"))
        .json(&serde_json::json!({"uri":format!("http://{address}"),"force":true}))
        .send()
        .await
        .expect("discover bounded endpoint");
    assert!(
        registered.status().is_success(),
        "{}",
        registered.text().await.expect("discovery body")
    );
    let response = client
        .post(format!("{ingress}/BudgetProbe/replay"))
        .header("content-type", "application/octet-stream")
        .body("")
        .send()
        .await
        .expect("native Restate invocation");
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.expect("invocation error")
    );
    assert_eq!(response.json::<u32>().await.expect("durable result"), 128);
    let _ = stop.send(());
    server.await.expect("shutdown");
}

struct AttemptSample {
    label: String,
    attempt: Option<lash_trace::AttemptObservation>,
}

#[derive(Clone)]
struct AttemptProbe {
    seen: Arc<std::sync::Mutex<Vec<AttemptSample>>>,
}

#[restate_sdk::service]
impl AttemptProbe {
    #[handler]
    async fn run(&self, ctx: Context<'_>, input: Bytes) -> HandlerResult<Bytes> {
        let controller = crate::RestateRuntimeEffectController::new_for_test(ctx);
        let captured = controller.attempt_observation();
        let retained = captured.clone();
        let copied = tokio::spawn(async move {
            assert_eq!(super::current_attempt_observation(), None);
            retained
        })
        .await
        .expect("copied attempt");
        assert_eq!(copied, captured);
        let label = String::from_utf8(input.to_vec()).expect("probe label");
        for _ in 0..2 {
            assert_eq!(super::current_attempt_observation(), captured);
            self.seen
                .lock()
                .expect("probe observations")
                .push(AttemptSample {
                    label: label.clone(),
                    attempt: controller.attempt_observation(),
                });
            tokio::task::yield_now().await;
        }
        Ok(input)
    }
}

fn attempt_request(
    label: &'static str,
    parent: Option<&str>,
    state: &[&str],
) -> http::Request<http_body_util::Full<Bytes>> {
    let input = encode_message(
        MessageType::InputCommand,
        &InputCommandMessage {
            value: Some(Value {
                content: Bytes::from_static(label.as_bytes()),
            }),
            headers: vec![lash_restate_test::protocol::generated::Header {
                key: "traceparent".into(),
                value: "00-cccccccccccccccccccccccccccccccc-dddddddddddddddd-01".into(),
            }],
            ..Default::default()
        },
    );
    let mut request = http::Request::builder()
        .uri("/invoke/AttemptProbe/run")
        .header("content-type", ProtocolVersion::V6.content_type())
        .header("x-restate-invocation-id", format!("inv-{label}"));
    if let Some(parent) = parent {
        request = request.header("traceparent", parent);
    }
    for state in state {
        request = request.header("tracestate", *state);
    }
    request
        .body(http_body_util::Full::new(Bytes::from(
            [prefix().as_ref(), input.as_ref()].concat(),
        )))
        .expect("attempt request")
}

#[tokio::test]
async fn attempt_context_is_scoped_to_each_response_body_poll() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let endpoint = Endpoint::builder()
        .bind(AttemptProbe { seen: seen.clone() })
        .build();
    let parent_a = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let parent_b = "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00";
    let limits = super::RestateEndpointLimits::new(1024, 1032);
    let mut a = super::handle_endpoint(
        &endpoint,
        attempt_request("a", Some(parent_a), &["rojo=1", "congo=2"]),
        limits,
    )
    .into_body();
    let mut b = super::handle_endpoint(
        &endpoint,
        attempt_request("b", Some(parent_b), &["invalid"]),
        limits,
    )
    .into_body();
    let mut absent =
        super::handle_endpoint(&endpoint, attempt_request("absent", None, &[]), limits).into_body();
    let mut malformed = super::handle_endpoint(
        &endpoint,
        attempt_request("malformed", Some("broken"), &[]),
        limits,
    )
    .into_body();
    assert!(
        seen.lock().expect("observations").is_empty(),
        "SDK executes only when response bodies are polled"
    );
    std::future::poll_fn(|cx| {
        for body in [&mut a, &mut b, &mut absent, &mut malformed] {
            let _ = Pin::new(body).poll_frame(cx);
            assert_eq!(
                super::current_attempt_context(),
                None,
                "poll leaked context"
            );
        }
        Poll::Ready(())
    })
    .await;
    let (a, b, absent, malformed) = tokio::join!(
        a.collect(),
        b.collect(),
        absent.collect(),
        malformed.collect()
    );
    for output in [a, b, absent, malformed] {
        output.expect("completed probe");
    }
    let seen = seen.lock().expect("observations");
    assert_eq!(seen.len(), 8, "each handler ran across two polls");
    for AttemptSample {
        label,
        attempt: context,
    } in seen.iter()
    {
        let expected = match label.as_str() {
            "a" => lash_trace::TraceCarrier::extract_w3c(Some(parent_a), Some("rojo=1,congo=2")),
            "b" => lash_trace::TraceCarrier::extract_w3c(Some(parent_b), None),
            "absent" | "malformed" => None,
            _ => panic!("unexpected probe label"),
        };
        assert_eq!(
            context.as_ref().and_then(|attempt| attempt.context.clone()),
            expected,
            "wrong delivery context for {label}"
        );
        assert_eq!(
            context
                .as_ref()
                .and_then(|attempt| attempt.invocation_id.as_deref()),
            Some(format!("inv-{label}").as_str())
        );
    }
    assert_eq!(super::current_attempt_context(), None);
}

type AttemptScopeSample = (
    lash_trace::AttemptObservation,
    lash_trace::DurableTraceScope,
);

#[derive(Clone)]
struct ServerAttemptProbe {
    seen: Arc<std::sync::Mutex<Vec<AttemptScopeSample>>>,
    tracing: lash_core::trace::TraceRuntime,
}

#[restate_sdk::service]
impl ServerAttemptProbe {
    #[handler]
    async fn run(&self, ctx: Context<'_>, input: Bytes) -> HandlerResult<Bytes> {
        let controller = crate::RestateRuntimeEffectController::new_for_test(ctx);
        let attempt = controller.attempt_observation().expect("delivery metadata");
        assert_eq!(
            attempt.invocation_id.as_deref(),
            Some(controller.context().invocation_id())
        );
        let tracing = self.tracing.clone();
        let Json(scope) = controller
            .context()
            .run(move || async move {
                let scope_id =
                    lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Run {
                        session_id: "attempt-law".into(),
                        run: "run".into(),
                    });
                let candidate = tracing
                    .scopes()
                    .propose(&scope_id, &lash_trace::TraceCause::Root);
                let scope = lash_trace::DurableTraceScope {
                    scope: scope_id,
                    cause: lash_trace::TraceCause::Root,
                    anchor: candidate.anchor(),
                    started_at_ms: 1_700_000_000_000,
                };
                candidate.settle(lash_trace::TraceCandidateOutcome::Selected);
                Ok::<_, HandlerError>(Json(scope))
            })
            .name("retained-root-anchor")
            .await?;
        for ordinal in 0..2 {
            let seen = self.seen.clone();
            let attempt = attempt.clone();
            let scope = scope.clone();
            let tracing = self.tracing.clone();
            controller
                .context()
                .run(move || async move {
                    let attempt_id = lash_trace::TraceAttemptId::new(format!(
                        "{}-{ordinal}",
                        attempt.context.as_ref().expect("server tracing").span_id(),
                    ));
                    let permit = lash_trace::EmissionPermit::live_execution(attempt_id.clone());
                    tracing.emitter().emit(
                        Some(&permit),
                        &scope,
                        Some(&attempt),
                        || lash_trace::TraceRecordIdentity::Live {
                            scope: scope.scope.clone(),
                            attempt: attempt_id,
                            ordinal: 0,
                        },
                        scope.started_at_ms + ordinal + 1,
                        || {
                            (
                                lash_trace::TraceContext::default(),
                                lash_trace::TraceEvent::ExecCodeCompleted {
                                    duration_ms: 1,
                                    output: String::new(),
                                    output_chars: 0,
                                    observation_count: 0,
                                    observation_projections: Vec::new(),
                                    error: None,
                                    terminal_finish: None,
                                    tool_calls: Vec::new(),
                                },
                            )
                        },
                    );
                    seen.lock()
                        .expect("live observations")
                        .push((attempt, scope));
                    Ok::<_, HandlerError>(Json(ordinal))
                })
                .name(format!("otel-live-{ordinal}"))
                .await?;
            if ordinal == 0 {
                controller
                    .context()
                    .sleep(Duration::from_millis(200))
                    .await?;
            }
        }
        Ok(input)
    }
}

async fn capture_otlp_request(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (head_end, length) = loop {
        let read = stream.read(&mut chunk).await.expect("OTLP request head");
        assert_ne!(read, 0, "incomplete OTLP head");
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() <= 1024 * 1024, "OTLP fixture budget");
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..end]).expect("OTLP HTTP headers");
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("OTLP length"))
                })
                .expect("OTLP content length");
            assert!(length <= 1024 * 1024, "OTLP fixture budget");
            break (end + 4, length);
        }
    };
    while bytes.len() < head_end + length {
        let read = stream.read(&mut chunk).await.expect("OTLP request body");
        assert_ne!(read, 0, "incomplete OTLP body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let request = serde_json::from_slice(&bytes[head_end..head_end + length]).expect("OTLP JSON");
    stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await.expect("OTLP acknowledgement");
    request
}

fn exported_attempt_matches(
    exported: &[serde_json::Value],
    attempt: &lash_trace::AttemptObservation,
) -> bool {
    let carrier = attempt.context.as_ref().expect("server tracing enabled");
    let trace_id = carrier.trace_id().to_string();
    let span_id = carrier.span_id().to_string();
    exported.iter().any(|batch| {
        batch["resourceSpans"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|resource| {
                resource["scopeSpans"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|scope| {
                        scope["spans"].as_array().into_iter().flatten().any(|span| {
                            span["name"]
                                .as_str()
                                .is_some_and(|name| name.starts_with("invocation-attempt "))
                                && span["traceId"].as_str() == Some(trace_id.as_str())
                                && span["spanId"].as_str() == Some(span_id.as_str())
                                && span["attributes"].as_array().into_iter().flatten().any(
                                    |attribute| {
                                        attribute["key"].as_str() == Some("restate.invocation.id")
                                            && attribute["value"]["stringValue"].as_str()
                                                == attempt.invocation_id.as_deref()
                                    },
                                )
                        })
                    })
            })
    })
}

#[allow(
    clippy::disallowed_methods,
    reason = "the private gate supplies the server and collector addresses"
)]
#[tokio::test]
#[ignore = "requires the pinned Restate server with service tracing and replay enabled"]
async fn server_attempt_headers_link_to_lash_domain_spans() {
    let admin = std::env::var("RESTATE_ADMIN_URL").expect("live admin URL");
    let ingress = std::env::var("RESTATE_INGRESS_URL").expect("live ingress URL");
    let gate = std::env::var("KILN_GATE_ID").expect("private gate identity");
    let collector_address =
        std::env::var("LASH_OTEL_COLLECTOR_BIND").expect("private collector address");
    let collector = tokio::net::TcpListener::bind(collector_address)
        .await
        .expect("collector bind");
    let exported = Arc::new(std::sync::Mutex::new(Vec::new()));
    let batches = exported.clone();
    let (stop_collector, collector_stopped) = tokio::sync::oneshot::channel();
    let collector_task = tokio::spawn(async move {
        let mut stopped = collector_stopped;
        loop {
            tokio::select! {
                connection = collector.accept() => {
                    let (mut stream, _) = connection.expect("collector connection");
                    let batch = capture_otlp_request(&mut stream).await;
                    batches.lock().expect("exported batches").push(batch);
                }
                _ = &mut stopped => break,
            }
        }
    });
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
    let telemetry = Arc::new(lash_trace::otel::OtelTelemetry::new(
        &tracer_provider,
        &meter_provider,
        lash_trace::otel::OtelOptions::default(),
    ));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let endpoint = Endpoint::builder()
        .bind(ServerAttemptProbe {
            seen: seen.clone(),
            tracing: lash_core::trace::TraceRuntime::default()
                .with_scopes(telemetry.clone())
                .with_projector(telemetry.clone())
                .with_metrics(telemetry.metrics().clone()),
        })
        .build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("endpoint bind");
    let address = listener.local_addr().expect("endpoint address");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(super::serve_endpoint(
        listener,
        endpoint,
        super::RestateEndpointLimits::new(1024, 1032),
        async {
            let _ = stopped.await;
        },
    ));
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let client = reqwest::Client::new();
        let registration = client
            .post(format!("{admin}/deployments"))
            .json(&serde_json::json!({"uri":format!("http://{address}")}))
            .send()
            .await
            .expect("register endpoint");
        assert!(
            registration.status().is_success(),
            "{}",
            registration.text().await.expect("registration error")
        );
        let output = client
            .post(format!("{ingress}/ServerAttemptProbe/run"))
            .header("content-type", "application/octet-stream")
            .header("idempotency-key", format!("{gate}-attempt-law"))
            .body("probe")
            .send()
            .await
            .expect("native invocation");
        assert!(
            output.status().is_success(),
            "{}",
            output.text().await.expect("invocation error")
        );
        assert_eq!(output.bytes().await.expect("result").as_ref(), b"probe");
        let attempts = seen.lock().expect("live attempts").clone();
        assert_eq!(
            attempts.len(),
            2,
            "only the two fresh bodies observe attempts"
        );
        assert_eq!(attempts[0].1, attempts[1].1, "root anchor survives replay");
        let scope = &attempts[0].1;
        let attempts = attempts
            .iter()
            .map(|(attempt, _)| attempt)
            .collect::<Vec<_>>();
        assert_eq!(attempts[0].invocation_id, attempts[1].invocation_id);
        assert!(
            !attempts[0]
                .context
                .as_ref()
                .expect("first context")
                .same_span(attempts[1].context.as_ref().expect("resumed context")),
            "resume must deliver a fresh server attempt"
        );
        loop {
            let matched = {
                let exported = exported.lock().expect("exported batches");
                attempts
                    .iter()
                    .all(|attempt| exported_attempt_matches(&exported, attempt))
            };
            if matched {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tracer_provider.force_flush().expect("domain export flush");
        let spans = exporter.get_finished_spans().expect("Lash domain spans");
        assert_eq!(
            spans.len(),
            3,
            "one admission and two fresh domain spans, no infrastructure hierarchy"
        );
        let root = scope.anchor.context().expect("retained root anchor");
        let admission = spans
            .iter()
            .find(|span| span.name == "lash.run.admitted")
            .expect("root admission");
        assert_eq!(
            admission.span_context.trace_id().to_bytes(),
            root.trace_id().to_bytes()
        );
        assert_eq!(
            admission.span_context.span_id().to_bytes(),
            root.span_id().to_bytes()
        );
        assert_eq!(admission.parent_span_id.to_bytes(), [0; 8]);
        let domain = spans
            .iter()
            .filter(|span| span.name == "lash.exec_code")
            .collect::<Vec<_>>();
        assert_eq!(domain.len(), 2);
        for (span, attempt) in domain.iter().zip(&attempts) {
            assert_eq!(
                span.span_context.trace_id().to_bytes(),
                root.trace_id().to_bytes()
            );
            assert_eq!(span.parent_span_id.to_bytes(), root.span_id().to_bytes());
            let carrier = attempt.context.as_ref().expect("server attempt context");
            assert_eq!(
                span.links.len(),
                1,
                "only the current server attempt is linked"
            );
            assert_eq!(
                span.links[0].span_context.trace_id().to_bytes(),
                carrier.trace_id().to_bytes()
            );
            assert_eq!(
                span.links[0].span_context.span_id().to_bytes(),
                carrier.span_id().to_bytes()
            );
            assert!(span.links[0].span_context.is_remote());
            assert!(span.attributes.iter().any(|attribute| {
                attribute.key.as_str() == "lash.attempt.invocation_id"
                    && Some(attribute.value.to_string().as_str())
                        == attempt.invocation_id.as_deref()
            }));
            assert_eq!(span.instrumentation_scope.name(), "lash");
        }
    })
    .await;
    let _ = stop.send(());
    server.await.expect("endpoint shutdown");
    let _ = stop_collector.send(());
    collector_task.await.expect("collector shutdown");
    result.expect("exported server attempt witness timed out");
}
