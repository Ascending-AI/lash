//! A task-local streaming recorder before the facade's bounded trace projection.
use crate::provider_log::Capture;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct StateData {
    capture: Capture,
    client: reqwest::Client,
    origin: String,
}
// Axum owns its HTTP drivers. A bounded duplex bridge gives us an owned,
// abortable task per accepted socket without adding another network hop.
// Aborting the bridge closes both the socket and the HTTP driver's input.
#[derive(Default)]
struct ConnectionTasks {
    closed: bool,
    handles: Vec<tokio::task::JoinHandle<()>>,
}
struct Connections {
    listener: tokio::net::TcpListener,
    tasks: Arc<Mutex<ConnectionTasks>>,
}
impl axum::serve::Listener for Connections {
    type Io = tokio::io::DuplexStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (mut socket, addr) = axum::serve::Listener::accept(&mut self.listener).await;
        let (http, mut wire) = tokio::io::duplex(64 * 1024);
        let mut tasks = self.tasks.lock().unwrap();
        if !tasks.closed {
            tasks.handles.retain(|task| !task.is_finished());
            tasks.handles.push(tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut wire).await;
            }));
        }
        (http, addr)
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

pub(crate) struct Recorder {
    pub base_url: String,
    task: tokio::task::JoinHandle<()>,
    connections: Arc<Mutex<ConnectionTasks>>,
    capture: Capture,
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.task.abort();
        {
            let mut connections = self.connections.lock().unwrap();
            connections.closed = true;
            for connection in connections.handles.drain(..) {
                connection.abort();
            }
        }
        // Finalize synchronously: the task grader reads evidence immediately
        // after dropping run_turn on its outer deadline.
        let attempts = self
            .capture
            .http_bodies
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for attempt in attempts {
            finish_capture(&self.capture, attempt, true);
        }
        for (index, row) in self.capture.entries.lock().unwrap().iter().enumerate() {
            if row["partial"] == true {
                self.capture.dump(index + 1, "response", row);
            }
        }
    }
}
impl Recorder {
    pub(crate) async fn start(capture: Capture) -> anyhow::Result<Self> {
        Self::start_at(capture, "https://openrouter.ai".into()).await
    }
    async fn start_at(capture: Capture, origin: String) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}/api/v1", listener.local_addr()?);
        let state = Arc::new(StateData {
            capture: capture.clone(),
            origin,
            client: reqwest::Client::builder().build()?,
        });
        let router = Router::new()
            .fallback(forward)
            .layer(DefaultBodyLimit::disable())
            .with_state(state);
        let connections = Arc::new(Mutex::new(ConnectionTasks::default()));
        let listener = Connections {
            listener,
            tasks: connections.clone(),
        };
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            base_url,
            task,
            connections,
            capture,
        })
    }
}
async fn forward(
    State(state): State<Arc<StateData>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if uri.path() != "/api/v1/chat/completions" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let attempt = {
        let mut rows = state.capture.entries.lock().unwrap();
        let attempt = rows.len();
        let Some(row) = rows.last_mut() else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "no active provider attempt",
            )
                .into_response();
        };
        let body = state
            .capture
            .redact(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
        row["wire_request"] = body.clone();
        row["request_sizes"] = crate::accounting::request_sizes(&body);
        tracing::debug!(target:"toolbench",parent:&state.capture.span(),attempt,request=%body,sizes=%row["request_sizes"],"HTTP wire request");
        state.capture.dump(attempt, "request", &body);
        attempt
    };
    let recording = Recording::new(state.capture.clone(), attempt);
    // uri.path() intentionally drops query strings: only /chat/completions is proxied.
    // Only the fixed OpenRouter origin is reachable; credentials stay in HTTP headers.
    let mut request = state
        .client
        .post(format!("{}{}", state.origin, uri.path()))
        .body(body);
    for (name, value) in &headers {
        if !matches!(name.as_str(), "host" | "content-length" | "connection") {
            request = request.header(name, value);
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let body =
                json!({"error":{"message":error.to_string(),"kind":"recorder_upstream_transport"}});
            capture_chunk(&state.capture, attempt, &body.to_string());
            recording.finish(false);
            return (StatusCode::BAD_GATEWAY, axum::Json(body)).into_response();
        }
    };
    let mut builder = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection"
        ) {
            builder = builder.header(name, value);
        }
    }
    let stream = futures_util::stream::unfold(
        (response.bytes_stream(), Some(recording)),
        |(mut upstream, mut recording)| async move {
            match upstream.next().await {
                Some(chunk) => {
                    if let Some(active) = &recording {
                        match &chunk {
                            Ok(bytes) => capture_bytes(&active.capture, active.attempt, bytes),
                            Err(_) => recording.take().unwrap().finish(true),
                        }
                    }
                    Some((chunk, (upstream, recording)))
                }
                None => {
                    if let Some(active) = recording {
                        active.finish(false);
                    }
                    None
                }
            }
        },
    );
    builder
        .body(Body::from_stream(stream))
        .expect("upstream headers form response")
}
struct Recording {
    capture: Capture,
    attempt: usize,
}
impl Recording {
    fn new(capture: Capture, attempt: usize) -> Self {
        capture
            .http_bodies
            .lock()
            .unwrap()
            .entry(attempt)
            .or_default();
        Self { capture, attempt }
    }
    fn finish(self, partial: bool) {
        finish_capture(&self.capture, self.attempt, partial);
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        finish_capture(&self.capture, self.attempt, true);
    }
}
fn finish_capture(capture: &Capture, attempt: usize, partial: bool) {
    let mut finished = capture.http_finished.lock().unwrap();
    if !finished.insert(attempt) {
        return;
    }
    let bodies = capture.http_bodies.lock().unwrap();
    let body = &bodies[&attempt];
    let value = json!({"body_text":String::from_utf8_lossy(body),"bytes_received":body.len(),"utf8_complete":std::str::from_utf8(body).is_ok(),"partial":partial});
    capture.dump(attempt, "http-response", &value);
}
fn capture_bytes(capture: &Capture, attempt: usize, bytes: &[u8]) {
    let finished = capture.http_finished.lock().unwrap();
    if finished.contains(&attempt) {
        return;
    }
    capture
        .http_bodies
        .lock()
        .unwrap()
        .entry(attempt)
        .or_default()
        .extend_from_slice(bytes);
    // Log only this chunk: accumulation and the final dump are linear in body size.
    let value = json!({"chunk_text":String::from_utf8_lossy(bytes),"bytes_received":bytes.len()});
    tracing::debug!(target:"toolbench",parent:&capture.span(),attempt,body=%capture.redact(value),"HTTP wire response bytes");
}
fn capture_chunk(capture: &Capture, attempt: usize, text: &str) {
    capture_bytes(capture, attempt, text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dumping_capture() -> (Capture, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "toolbench-wire-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let capture = Capture::default();
        capture.set_dump_prefix(Some(path.join("capture")));
        (capture, path)
    }
    #[test]
    fn many_chunks_write_once_and_aborted_streams_are_partial() {
        let (capture, dir) = dumping_capture();
        for (attempt, partial) in [(1, false), (2, true)] {
            let recording = Recording::new(capture.clone(), attempt);
            let path = dir.join(format!("capture.turn-1-round-{attempt}-http-response.json"));
            let chunks = 128;
            for _ in 0..chunks {
                capture_bytes(&capture, attempt, b"data: hello\n\n");
                assert!(
                    !path.exists(),
                    "must not rewrite accumulated bytes per chunk"
                );
            }
            if partial {
                drop(recording);
            } else {
                recording.finish(false);
            }
            let body: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(body["body_text"], "data: hello\n\n".repeat(chunks));
            assert_eq!(body["partial"], partial);
            finish_capture(&capture, attempt, true);
            assert_eq!(
                capture
                    .dump_writes
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(i, d)| *i == attempt && d == "http-response")
                    .count(),
                1
            );
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }

    #[tokio::test]
    async fn recorder_drop_aborts_connections_and_finalizes_a_timed_out_stream() {
        let upstream = Router::new().fallback(|| async {
            Body::from_stream(
                futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
                    b"data: partial\n\n",
                ))])
                .chain(futures_util::stream::pending()),
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let (capture, dir) = dumping_capture();
        capture.entries.lock().unwrap().push(json!({}));
        let recorder = Recorder::start_at(capture.clone(), origin).await.unwrap();
        let tasks = recorder.connections.clone();
        let mut response = reqwest::Client::new()
            .post(format!("{}/chat/completions", recorder.base_url))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert!(response.chunk().await.unwrap().is_some());
        assert!(!tasks.lock().unwrap().handles.is_empty());
        // This is the same Drop path as run_turn's outer timeout.
        drop(recorder);
        let path = dir.join("capture.turn-1-round-1-http-response.json");
        let body: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(body["partial"], true);
        assert_eq!(body["body_text"], "data: partial\n\n");
        assert!(tasks.lock().unwrap().handles.is_empty());
        let ended = tokio::time::timeout(std::time::Duration::from_secs(2), response.chunk())
            .await
            .unwrap();
        assert!(!matches!(ended, Ok(Some(_))));
        upstream_task.abort();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(dir.join("capture.turn-1-round-1-request.json")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[tokio::test]
    async fn recorder_preserves_large_request_and_raw_error_body() {
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let upstream=Router::new().fallback(move |headers:HeaderMap,body:Bytes| {
            let received=received_clone.clone();
            async move {
                assert_eq!(headers["authorization"],"Bearer test-key");
                *received.lock().unwrap()=body.to_vec();
                (StatusCode::BAD_REQUEST,axum::Json(json!({"error":{"message":"bad input"},"usage":{"prompt_tokens":30,"cost":0.001}})))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let capture = Capture::default();
        capture.entries.lock().unwrap().push(json!({}));
        let proxy = Recorder::start_at(capture.clone(), origin).await.unwrap();
        let body = json!({"messages":[{"role":"system","content":"é".repeat(6000)}]}).to_string();
        let response = reqwest::Client::new()
            .post(format!("{}/chat/completions", proxy.base_url))
            .bearer_auth("test-key")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert_eq!(*received.lock().unwrap(), body.as_bytes());
        assert_eq!(
            capture.entries.lock().unwrap()[0]["wire_request"],
            serde_json::from_str::<Value>(&body).unwrap()
        );
        assert_eq!(capture.http_bodies.lock().unwrap()[&1], text.as_bytes());
        assert!(
            !capture.entries.lock().unwrap()[0]
                .to_string()
                .contains("test-key")
        );
        upstream_task.abort();
    }
    #[test]
    fn response_recording_preserves_utf8_across_chunks() {
        let capture = Capture::default();
        capture_bytes(&capture, 1, &[0xc3]);
        capture_bytes(&capture, 1, &[0xa9]);
        assert_eq!(
            std::str::from_utf8(&capture.http_bodies.lock().unwrap()[&1]).unwrap(),
            "é"
        );
    }
}
