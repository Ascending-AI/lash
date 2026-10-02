//! Serving a lash endpoint to Restate.
//!
//! Restate executes every lash turn through the deployment's endpoint: each
//! journaled step is a small HTTP/2 frame the handler writes and a small frame
//! the server answers, dozens per turn. With Nagle's algorithm on the
//! accepted connection, a handler's frame written while its previous one is
//! still unacknowledged waits for the server's delayed ACK, about 40 ms on
//! Linux, and one fast turn pays that wait at almost every step (FIG-3843).
//! The SDK's own `HttpServer` leaves Nagle on, so lash serves its endpoint
//! here, with `TCP_NODELAY` set on every connection it accepts.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use restate_sdk::endpoint::{Endpoint, HandleOptions, ProtocolMode};

mod message_limits;
use message_limits::MessageBody;
pub use message_limits::RestateEndpointLimits;
use tokio::net::TcpListener;

tokio::task_local! {
    static ATTEMPT_CONTEXT: Option<Arc<lash_trace::AttemptObservation>>;
}

pub(crate) fn current_attempt_observation() -> Option<lash_trace::AttemptObservation> {
    ATTEMPT_CONTEXT
        .try_with(|attempt| attempt.as_deref().cloned())
        .ok()
        .flatten()
}

#[cfg(test)]
pub(crate) fn current_attempt_context() -> Option<lash_trace::TraceCarrier> {
    current_attempt_observation().and_then(|attempt| attempt.context)
}

fn delivery_attempt(headers: &hyper::HeaderMap) -> Option<Arc<lash_trace::AttemptObservation>> {
    let mut parents = headers.get_all("traceparent").iter();
    let parent = parents.next().and_then(|value| value.to_str().ok());
    let parent = parent.filter(|_| parents.next().is_none());
    let state = headers
        .get_all("tracestate")
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>()
        .ok()
        .map(|members| members.join(","));
    let context = lash_trace::TraceCarrier::extract_w3c(parent, state.as_deref());
    let invocation_id = headers
        .get("x-restate-invocation-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    (context.is_some() || invocation_id.is_some()).then(|| {
        Arc::new(lash_trace::AttemptObservation {
            context,
            invocation_id,
        })
    })
}

struct AttemptBody {
    inner: restate_sdk::endpoint::ResponseBody,
    attempt: Option<Arc<lash_trace::AttemptObservation>>,
}

impl Body for AttemptBody {
    type Data = bytes::Bytes;
    type Error = <restate_sdk::endpoint::ResponseBody as Body>::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let body = self.get_mut();
        ATTEMPT_CONTEXT.sync_scope(body.attempt.clone(), || {
            Pin::new(&mut body.inner).poll_frame(cx)
        })
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// How long a shut-down server waits for its open connections to finish:
/// the SDK server's own grace.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// The pause before accepting again after an accept failed (a descriptor
/// limit, say), so a persistent failure does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(10);

/// Serve `endpoint` over HTTP/2 on `listener` until `shutdown` resolves.
///
/// Every accepted connection has `TCP_NODELAY` set, so a handler's journal
/// frames reach the server as they are written (see the module docs).
/// Incoming service-protocol headers are checked against the host's `limits`
/// before their payload is forwarded to the SDK. Replay streams have no total
/// byte limit. Once `shutdown` resolves the server stops accepting and waits up to ten
/// seconds for the open connections to finish before it returns.
pub async fn serve_endpoint(
    listener: TcpListener,
    endpoint: Endpoint,
    limits: RestateEndpointLimits,
    shutdown: impl Future,
) {
    let endpoint = service_fn(move |request: hyper::Request<Incoming>| {
        std::future::ready(Ok::<_, std::convert::Infallible>(handle_endpoint(
            &endpoint, request, limits,
        )))
    });
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, remote) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(error = %error, "Restate endpoint accept failed");
                        tokio::time::sleep(ACCEPT_RETRY).await;
                        continue;
                    }
                };
                if let Err(error) = stream.set_nodelay(true) {
                    tracing::warn!(
                        remote = %remote,
                        error = %error,
                        "Restate endpoint connection kept Nagle's algorithm"
                    );
                }
                let connection = http2::Builder::new(TokioExecutor::new())
                    .initial_stream_window_size(
                        u32::try_from(limits.max_pending_bytes().clamp(8, 65_535))
                            .unwrap_or(65_535),
                    )
                    .serve_connection(TokioIo::new(stream), endpoint.clone());
                let connection = graceful.watch(connection);
                tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        tracing::warn!(remote = %remote, error = ?error, "Restate endpoint connection failed");
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(SHUTDOWN_GRACE) => {
            tracing::warn!("Restate endpoint connections did not close within the shutdown grace");
        }
    }
}

fn handle_endpoint<B>(
    endpoint: &Endpoint,
    request: hyper::Request<B>,
    limits: RestateEndpointLimits,
) -> hyper::Response<AttemptBody>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Unpin + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    let attempt = delivery_attempt(request.headers());
    endpoint
        .handle_with_options(
            request.map(|body| MessageBody::new(body, limits)),
            HandleOptions {
                protocol_mode: ProtocolMode::BidiStream,
            },
        )
        .map(|inner| AttemptBody { inner, attempt })
}

#[cfg(test)]
mod tests;
