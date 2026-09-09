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
use std::sync::Arc;

struct StateData {
    capture: Capture,
    client: reqwest::Client,
    origin: String,
}
pub(crate) struct Recorder {
    pub base_url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.task.abort();
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
            capture,
            origin,
            client: reqwest::Client::builder().build()?,
        });
        let router = Router::new()
            .fallback(forward)
            .layer(DefaultBodyLimit::disable())
            .with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self { base_url, task })
    }
}
async fn forward(
    State(state): State<Arc<StateData>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
    let capture = state.capture.clone();
    let stream = response.bytes_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            // Buffer raw bytes losslessly, including UTF-8 split across network chunks.
            capture_bytes(&capture, attempt, bytes);
        }
        chunk
    });
    builder
        .body(Body::from_stream(stream))
        .expect("upstream headers form response")
}
fn capture_bytes(capture: &Capture, attempt: usize, bytes: &[u8]) {
    let mut bodies = capture.http_bodies.lock().unwrap();
    let body = bodies.entry(attempt).or_default();
    body.extend_from_slice(bytes);
    // Accumulation preserves split UTF-8; a truncated final scalar is marked incomplete.
    let value = json!({"body_text":String::from_utf8_lossy(body),"bytes_received":body.len(),"utf8_complete":std::str::from_utf8(body).is_ok()});
    capture.dump(attempt, "http-response", &value);
    tracing::debug!(target:"toolbench",parent:&capture.span(),attempt,body=%capture.redact(value),"HTTP wire response bytes");
}
fn capture_chunk(capture: &Capture, attempt: usize, text: &str) {
    capture_bytes(capture, attempt, text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
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
