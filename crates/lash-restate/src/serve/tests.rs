use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
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
