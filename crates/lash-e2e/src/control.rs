//! The case's control endpoint: what a host's held bodies and keyed
//! effects call. A held body's request is answered only once the case
//! releases its tool, so the answer arriving is the release. Every entry is
//! recorded before it waits, so the case observes a body that entered even
//! when its node dies holding it.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, bail};
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

/// One body entry a host reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub tool: String,
    pub call_id: String,
    pub attempt: u32,
    pub run: Option<String>,
    pub owner: Value,
    pub completion: Option<String>,
    #[serde(default)]
    pub at_ms: u128,
}

/// One scripted model reply of the case's recorded provider: a refusal
/// with `status` and `body`, or a `200` stream of `text` deltas and its
/// `usage`, rendered in the requested endpoint's event shape. The stream
/// may hold before delta `hold.0` until the case releases `hold.1`, or
/// reset its connection before delta `reset`.
#[derive(Clone, Debug, Default)]
pub struct ProviderReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub text: Vec<String>,
    pub usage: (u64, u64),
    pub hold: Option<(usize, String)>,
    pub reset: Option<usize>,
    /// Tool calls the answer ends with, as `(id, tool, arguments)`.
    pub calls: Vec<(String, String, Value)>,
}

impl ProviderReply {
    /// A complete stream of `text` with `usage`.
    #[must_use]
    pub fn answer(text: &[&str], usage: (u64, u64)) -> Self {
        Self {
            status: 200,
            text: text.iter().map(|delta| (*delta).to_owned()).collect(),
            usage,
            ..Self::default()
        }
    }

    /// A complete stream that calls `tool` with `arguments` as call `id`.
    #[must_use]
    pub fn call(id: &str, tool: &str, arguments: Value, usage: (u64, u64)) -> Self {
        Self {
            status: 200,
            usage,
            calls: vec![(id.to_owned(), tool.to_owned(), arguments)],
            ..Self::default()
        }
    }

    /// A refusal with `status` and an OpenAI-shaped error body.
    #[must_use]
    pub fn refused(status: u16, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({"error": {"message": message, "code": status}}),
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct Shared {
    held: Mutex<Vec<Delivery>>,
    effects: Mutex<Vec<Delivery>>,
    released: watch::Sender<BTreeSet<String>>,
    changed: watch::Sender<u64>,
    replies: Mutex<Vec<ProviderReply>>,
    requests: Mutex<Vec<Value>>,
    /// The spans the case's OTLP collector acknowledged, in arrival order.
    spans: Mutex<Vec<Value>>,
    /// Whether the collector refuses exports, as a disconnected one would.
    collector_down: std::sync::atomic::AtomicBool,
}

/// The running endpoint.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
    url: String,
    stop: Arc<tokio::sync::Notify>,
}

impl Control {
    /// Serve a fresh endpoint on a loopback port.
    ///
    /// # Errors
    ///
    /// The listener does not bind.
    pub async fn start() -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr: SocketAddr = listener.local_addr()?;
        let stop = Arc::new(tokio::sync::Notify::new());
        let app = Router::new()
            .route("/hold", post(hold))
            .route("/effect", post(effect))
            .route("/body/{holds}", post(body))
            .route("/provider/{*endpoint}", post(provider))
            .route("/v1/traces", post(traces))
            .with_state(shared.clone());
        let stopped = stop.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { stopped.notified().await })
                .await;
        });
        Ok(Self {
            shared,
            url: format!("http://{addr}"),
            stop,
        })
    }

    /// The URL hosts post to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The URL a workbench fixture's bodies call on entry: an entry of a
    /// tool in `holds` is held like a consumer body's, any other answers at
    /// once.
    #[must_use]
    pub fn body_url(&self, holds: &[&str]) -> String {
        let holds = if holds.is_empty() {
            "-".to_owned()
        } else {
            holds.join(",")
        };
        format!("{}/body/{holds}", self.url)
    }

    /// The base URL of the case's recorded provider.
    #[must_use]
    pub fn provider_url(&self) -> String {
        format!("{}/provider", self.url)
    }

    /// Script the recorded provider: the `n`th request it receives is
    /// answered by `replies[n]`, and every later one by the last.
    pub fn script_provider(&self, replies: Vec<ProviderReply>) {
        if let Ok(mut scripted) = self.shared.replies.lock() {
            *scripted = replies;
        }
    }

    /// The requests the recorded provider received, in order: each one's
    /// endpoint and body.
    #[must_use]
    pub fn provider_requests(&self) -> Vec<Value> {
        self.shared
            .requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }

    /// The case's OTLP/HTTP trace endpoint.
    #[must_use]
    pub fn traces_url(&self) -> String {
        format!("{}/v1/traces", self.url)
    }

    /// Take the collector down (`true`: it refuses every export) or up.
    pub fn collector_down(&self, down: bool) {
        self.shared
            .collector_down
            .store(down, std::sync::atomic::Ordering::SeqCst);
    }

    /// The spans the collector acknowledged, in arrival order.
    #[must_use]
    pub fn spans(&self) -> Vec<Value> {
        self.shared
            .spans
            .lock()
            .map(|spans| spans.clone())
            .unwrap_or_default()
    }

    /// Release every hold of `tool`, now and later.
    pub fn release(&self, tool: &str) {
        self.shared
            .released
            .send_modify(|released| _ = released.insert(tool.to_owned()));
    }

    /// The held entries so far, in arrival order.
    #[must_use]
    pub fn held(&self) -> Vec<Delivery> {
        self.shared
            .held
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// The keyed effects so far, in arrival order.
    #[must_use]
    pub fn effects(&self) -> Vec<Delivery> {
        self.shared
            .effects
            .lock()
            .map(|effects| effects.clone())
            .unwrap_or_default()
    }

    /// Wait until `count` entries of `tool` were held.
    ///
    /// # Errors
    ///
    /// The deadline passes first.
    pub async fn wait_held(
        &self,
        tool: &str,
        count: usize,
        deadline: Instant,
    ) -> Result<Vec<Delivery>> {
        let mut changed = self.shared.changed.subscribe();
        loop {
            let held: Vec<_> = self
                .held()
                .into_iter()
                .filter(|held| held.tool == tool)
                .collect();
            if held.len() >= count {
                return Ok(held);
            }
            if tokio::time::timeout_at(deadline.into(), changed.changed())
                .await
                .is_err()
            {
                bail!(
                    "{tool} was held {} times, not {count}, by the deadline",
                    held.len()
                );
            }
        }
    }

    /// Stop serving; a request still held answers with an error.
    pub fn stop(&self) {
        self.stop.notify_waiters();
        self.stop.notify_one();
    }
}

async fn hold(State(shared): State<Arc<Shared>>, Json(delivery): Json<Delivery>) -> Json<bool> {
    let tool = delivery.tool.clone();
    if let Ok(mut held) = shared.held.lock() {
        held.push(delivery);
    }
    shared.changed.send_modify(|count| *count += 1);
    let mut released = shared.released.subscribe();
    let _ = released.wait_for(|released| released.contains(&tool)).await;
    Json(true)
}

async fn effect(State(shared): State<Arc<Shared>>, Json(delivery): Json<Delivery>) -> Json<bool> {
    if let Ok(mut effects) = shared.effects.lock() {
        effects.push(delivery);
    }
    shared.changed.send_modify(|count| *count += 1);
    Json(true)
}

/// A workbench fixture body's entry: it names its tool `label` and its
/// attempt `ordinal`.
async fn body(
    State(shared): State<Arc<Shared>>,
    Path(holds): Path<String>,
    Json(entry): Json<Value>,
) -> Json<bool> {
    let tool = entry["label"].as_str().unwrap_or_default().to_owned();
    if !holds.split(',').any(|held| held == tool) {
        return Json(true);
    }
    let text = |field: &str| entry[field].as_str().map(ToOwned::to_owned);
    let delivery = Delivery {
        tool,
        call_id: text("call_id").unwrap_or_default(),
        attempt: entry["ordinal"]
            .as_u64()
            .and_then(|ordinal| u32::try_from(ordinal).ok())
            .unwrap_or_default(),
        run: text("logical_run"),
        owner: entry["owner"].clone(),
        completion: text("completion"),
        at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis()),
    };
    hold(State(shared), Json(delivery)).await
}

/// The recorded provider: it answers the `n`th request by its script's
/// `n`th reply, in the shape of the endpoint asked for.
async fn provider(
    State(shared): State<Arc<Shared>>,
    Path(endpoint): Path<String>,
    Json(request): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let index = shared.requests.lock().map_or(0, |mut requests| {
        requests.push(serde_json::json!({"endpoint": endpoint, "body": request}));
        requests.len() - 1
    });
    shared.changed.send_modify(|count| *count += 1);
    let reply = shared
        .replies
        .lock()
        .ok()
        .and_then(|replies| replies.get(index).or(replies.last()).cloned())
        .unwrap_or_else(|| ProviderReply::refused(500, "the case scripted no provider reply"));
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in &reply.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            axum::http::HeaderValue::try_from(value.as_str()),
        ) {
            headers.insert(name, value);
        }
    }
    if reply.status != 200 {
        let status = axum::http::StatusCode::from_u16(reply.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        return (status, headers, Json(reply.body)).into_response();
    }
    let frames = render(&endpoint, &reply);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    // Each frame is one step; the hold and the reset sit before a delta's.
    let stream = futures_util::stream::unfold(0, move |step| {
        let shared = shared.clone();
        let frames = frames.clone();
        let reply = reply.clone();
        async move {
            let (frame, delta) = frames.get(step)?.clone();
            if let Some(delta) = delta {
                if let Some((at, barrier)) = &reply.hold
                    && delta == *at
                {
                    if let Ok(mut held) = shared.held.lock() {
                        held.push(Delivery {
                            tool: barrier.clone(),
                            call_id: String::new(),
                            attempt: u32::try_from(index).unwrap_or(u32::MAX),
                            run: None,
                            owner: Value::Null,
                            completion: None,
                            at_ms: 0,
                        });
                    }
                    shared.changed.send_modify(|count| *count += 1);
                    let mut released = shared.released.subscribe();
                    let _ = released
                        .wait_for(|released| released.contains(barrier))
                        .await;
                }
                if reply.reset == Some(delta) {
                    // The client observes the output before the reset.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    return Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "the recorded stream resets here",
                        )),
                        frames.len(),
                    ));
                }
            }
            Some((Ok(axum::body::Bytes::from(frame)), step + 1))
        }
    });
    (headers, axum::body::Body::from_stream(stream)).into_response()
}

/// The SSE frames of `reply` for `endpoint`, each with the index of the
/// text delta it carries.
fn render(endpoint: &str, reply: &ProviderReply) -> Vec<(String, Option<usize>)> {
    let frame = |value: Value| format!("data: {value}\n\n");
    let text: String = reply.text.concat();
    let (input, output) = reply.usage;
    let mut frames = Vec::new();
    if endpoint.ends_with("responses") {
        let mut sequence = 0;
        let mut next = || {
            sequence += 1;
            sequence - 1
        };
        frames.push((
            frame(
                serde_json::json!({"type": "response.created", "sequence_number": next(),
            "response": {"id": "resp_e2e", "status": "in_progress"}}),
            ),
            None,
        ));
        frames.push((
            frame(
                serde_json::json!({"type": "response.output_item.added", "sequence_number": next(),
            "output_index": 0, "item": {"type": "message", "id": "msg_e2e", "role": "assistant"}}),
            ),
            None,
        ));
        for (index, delta) in reply.text.iter().enumerate() {
            frames.push((frame(serde_json::json!({"type": "response.output_text.delta", "sequence_number": next(),
                "output_index": 0, "item_id": "msg_e2e", "content_index": 0, "delta": delta})), Some(index)));
        }
        frames.push((frame(serde_json::json!({"type": "response.completed", "sequence_number": next(),
            "response": {"id": "resp_e2e", "status": "completed",
                "output": [{"type": "message", "id": "msg_e2e", "status": "completed", "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]}],
                "usage": {"input_tokens": input, "output_tokens": output, "total_tokens": input + output}}})), None));
    } else {
        for (index, delta) in reply.text.iter().enumerate() {
            let mut delta = serde_json::json!({"content": delta});
            if index == 0 {
                delta["role"] = "assistant".into();
            }
            frames.push((
                frame(
                    serde_json::json!({"id": "chatcmpl-e2e", "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
                ),
                Some(index),
            ));
        }
        if !reply.calls.is_empty() {
            let calls: Vec<Value> = reply
                .calls
                .iter()
                .enumerate()
                .map(|(index, (id, tool, arguments))| {
                    serde_json::json!({"index": index, "id": id, "type": "function",
                        "function": {"name": tool, "arguments": arguments.to_string()}})
                })
                .collect();
            let mut delta = serde_json::json!({"tool_calls": calls});
            if reply.text.is_empty() {
                delta["role"] = "assistant".into();
            }
            frames.push((
                frame(
                    serde_json::json!({"id": "chatcmpl-e2e", "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
                ),
                None,
            ));
        }
        let finish = if reply.calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        frames.push((
            frame(
                serde_json::json!({"id": "chatcmpl-e2e", "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
            ),
            None,
        ));
        frames.push((
            frame(
                serde_json::json!({"id": "chatcmpl-e2e", "object": "chat.completion.chunk",
            "choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output,
                "total_tokens": input + output}}),
            ),
            None,
        ));
        frames.push(("data: [DONE]\n\n".to_owned(), None));
    }
    frames
}

/// The case's OTLP collector: it acknowledges every span of an export, or,
/// while down, refuses the export whole.
async fn traces(
    State(shared): State<Arc<Shared>>,
    Json(export): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if shared
        .collector_down
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let spans = export["resourceSpans"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|resource| {
            resource["scopeSpans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|scope| scope["spans"].as_array().cloned().unwrap_or_default());
    if let Ok(mut acknowledged) = shared.spans.lock() {
        acknowledged.extend(spans);
    }
    shared.changed.send_modify(|count| *count += 1);
    Json(serde_json::json!({"partialSuccess": {}})).into_response()
}
