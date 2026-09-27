//! Serving a lash endpoint to Restate.
//!
//! Restate drives every lash turn through the deployment's endpoint: each
//! journaled step is a small HTTP/2 frame the handler writes and a small frame
//! the server answers, dozens per turn. With Nagle's algorithm on the
//! accepted connection, a handler's frame written while its previous one is
//! still unacknowledged waits for the server's delayed ACK, about 40 ms on
//! Linux, and one fast turn pays that wait at almost every step (FIG-3843).
//! The SDK's own `HttpServer` leaves Nagle on, so lash serves its endpoint
//! here, with `TCP_NODELAY` set on every connection it accepts.

use std::future::Future;
use std::time::Duration;

use hyper::server::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use restate_sdk::endpoint::Endpoint;
use restate_sdk::hyper::HyperEndpoint;
use tokio::net::TcpListener;

/// How long a shut-down server waits for its open connections to finish:
/// the SDK server's own grace.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// The pause before accepting again after an accept failed (a descriptor
/// limit, say), so a persistent failure does not spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(10);

/// Serve `endpoint` over HTTP/2 on `listener` until `shutdown` resolves.
///
/// Every accepted connection has `TCP_NODELAY` set, so a handler's journal
/// frames reach the server as they are written (see the module docs). Once
/// `shutdown` resolves the server stops accepting and waits up to ten
/// seconds for the open connections to finish before it returns.
pub async fn serve_endpoint(listener: TcpListener, endpoint: Endpoint, shutdown: impl Future) {
    let endpoint = HyperEndpoint::new(endpoint);
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
