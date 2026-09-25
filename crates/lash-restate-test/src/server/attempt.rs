//! One invocation attempt: a synthetic `POST /invoke/{service}/{handler}`
//! through the real `Endpoint::handle`, its response frames applied to the
//! server one by one.

use std::sync::Arc;

use http_body_util::BodyExt;
use tokio::sync::oneshot;

use super::Shared;
use super::body::{AttemptBody, InputProbe};
use super::model::InvKey;
use super::processor::Flow;
use super::serial::Turn;
use crate::protocol::FrameDecoder;

tokio::task_local! {
    /// The attempt whose task is running: the server it belongs to (by
    /// address) and its turn. Ingress requests a handler issues read it.
    static CURRENT: (usize, Turn);
}

/// The turn of the attempt whose task is running this code, if it belongs
/// to `shared`'s server.
pub(super) fn current_turn(shared: &Arc<Shared>) -> Option<Turn> {
    let server = Arc::as_ptr(shared) as usize;
    CURRENT
        .try_with(|(owner, turn)| (*owner == server).then_some(*turn))
        .ok()
        .flatten()
}

#[expect(
    clippy::too_many_arguments,
    reason = "one attempt's whole identity, handed from the processor to its task"
)]
pub(super) async fn run(
    shared: Arc<Shared>,
    key: InvKey,
    number: u32,
    service: String,
    handler: String,
    body: AttemptBody,
    probe: Arc<InputProbe>,
    started: Option<oneshot::Receiver<()>>,
) {
    // Serial scheduling: the attempt does not touch its handler before it
    // first holds the turn.
    if let Some(started) = started
        && started.await.is_err()
    {
        return;
    }
    let server = Arc::as_ptr(&shared) as usize;
    CURRENT
        .scope(
            (server, (key, number)),
            drive(shared, key, number, service, handler, body, probe),
        )
        .await;
}

async fn drive(
    shared: Arc<Shared>,
    key: InvKey,
    number: u32,
    service: String,
    handler: String,
    body: AttemptBody,
    probe: Arc<InputProbe>,
) {
    let invocation_id = shared.invocation_id(key);
    let request = match http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("/invoke/{service}/{handler}"))
        .header(
            http::header::CONTENT_TYPE,
            shared.config.protocol.content_type(),
        )
        .header(http::header::ACCEPT, shared.config.protocol.content_type())
        .header("x-restate-invocation-id", invocation_id)
        .body(body)
    {
        Ok(request) => request,
        Err(error) => {
            shared.stream_ended(key, number, format!("could not build the request: {error}"));
            return;
        }
    };
    let Some(endpoint) = shared.endpoint() else {
        shared.stream_ended(key, number, "no deployment is registered".to_owned());
        return;
    };
    let response = endpoint.handle(request);
    let status = response.status();
    if !status.is_success() {
        let body = response
            .into_body()
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .unwrap_or_default();
        shared.stream_ended(
            key,
            number,
            format!(
                "the endpoint answered {status}: {}",
                String::from_utf8_lossy(&body)
            ),
        );
        return;
    }
    let mut body = std::pin::pin!(response.into_body());
    let mut decoder = FrameDecoder::default();
    loop {
        // Report an empty read of the response body before parking on it:
        // the server counts the attempt idle only once everything it wrote
        // has been applied.
        // The handler runs inside this poll: a panic in it ends the stream
        // the way a deployment whose handler died ends it, instead of
        // killing the attempt task and leaving the attempt running forever.
        let next = std::future::poll_fn(|cx| {
            // Not drained while the poll runs: the handler polled inside it
            // may write a frame and block on its input before the poll
            // returns that frame, and a starved probe beside a stale
            // "drained" would read as blocked on the server.
            probe.set_response_drained(false);
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                http_body::Body::poll_frame(body.as_mut(), cx)
            }));
            let pending = matches!(polled, Ok(std::task::Poll::Pending));
            probe.set_response_drained(pending);
            if pending {
                // Decide the turn at the park itself, not whenever the
                // scheduler's task next runs: by then work outside the
                // server (a store call) may have woken the handler again.
                shared.parked();
            }
            match polled {
                Ok(std::task::Poll::Ready(frame)) => std::task::Poll::Ready(Ok(frame)),
                Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                Err(_) => std::task::Poll::Ready(Err(())),
            }
        })
        .await;
        let Ok(next) = next else {
            shared.stream_ended(key, number, "the handler panicked".to_owned());
            return;
        };
        let Some(frame) = next else {
            break;
        };
        let received_us = wall_now_us();
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                shared.stream_ended(key, number, format!("the response body failed: {error}"));
                return;
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        decoder.push(&data);
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    // Serial scheduling: an attempt that gave the turn up
                    // while its handler went on (a watch in flight beside
                    // its own work, say) writes once it holds the turn.
                    if shared.config.scheduling == super::Scheduling::Serial {
                        shared.await_turn((key, number), super::Wake::Outside).await;
                    }
                    if shared.on_frame(key, number, frame, received_us) == Flow::Stop {
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    shared.stream_ended(key, number, format!("undecodable frame: {error}"));
                    return;
                }
            }
        }
    }
    let detail = match decoder.finish() {
        Ok(()) => "unknown".to_owned(),
        Err(error) => error.to_string(),
    };
    shared.stream_ended(key, number, detail);
}

/// Wall-clock epoch microseconds, taken when a frame is read — before the
/// server lock — so a sleep's wall stamp is measured against its arrival.
fn wall_now_us() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros())
        .unwrap_or(0)
}
