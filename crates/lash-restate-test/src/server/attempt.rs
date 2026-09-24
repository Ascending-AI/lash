//! One invocation attempt: a synthetic `POST /invoke/{service}/{handler}`
//! through the real `Endpoint::handle`, its response frames applied to the
//! server one by one.

use std::sync::Arc;

use http_body_util::BodyExt;

use super::Shared;
use super::body::{AttemptBody, InputProbe};
use super::model::InvKey;
use super::processor::Flow;
use crate::protocol::FrameDecoder;

pub(super) async fn run(
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
        let next = std::future::poll_fn(|cx| {
            let polled = http_body::Body::poll_frame(body.as_mut(), cx);
            probe.set_response_drained(polled.is_pending());
            if polled.is_pending() {
                shared.activity.notify_waiters();
            }
            polled
        })
        .await;
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
