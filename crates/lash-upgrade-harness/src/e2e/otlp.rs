//! S34's local OTLP/HTTP JSON receiver. Socket loss is an observed fault;
//! collector records are independent of the host's JSONL trace.

mod http;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, watch};
use tokio::task::JoinHandle;

use super::control::CleanupReceipt;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectedSpan {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default)]
    pub parent_span_id: String,
    pub name: String,
    pub start_time_unix_nano: String,
    pub end_time_unix_nano: String,
    #[serde(default)]
    pub attributes: Vec<Attribute>,
    #[serde(default)]
    pub links: Vec<SpanLink>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attribute {
    pub key: String,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanLink {
    pub trace_id: String,
    pub span_id: String,
}

impl CollectedSpan {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .and_then(|attribute| attribute.value.get("stringValue"))
            .and_then(Value::as_str)
    }

    pub fn integer(&self, key: &str) -> Option<u64> {
        self.attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .and_then(|attribute| attribute.value.get("intValue"))
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|value| value.parse().ok())
                    .or_else(|| value.as_u64())
            })
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            hex_id(&self.trace_id, 32) && hex_id(&self.span_id, 16),
            "collector received an invalid OTLP span identity"
        );
        ensure!(
            self.parent_span_id.is_empty()
                || self.parent_span_id == "0000000000000000"
                || hex_id(&self.parent_span_id, 16),
            "collector received an invalid OTLP parent identity"
        );
        let start: u64 = self
            .start_time_unix_nano
            .parse()
            .context("OTLP start timestamp")?;
        let end: u64 = self
            .end_time_unix_nano
            .parse()
            .context("OTLP end timestamp")?;
        ensure!(start <= end, "collector received reversed span timestamps");
        for link in &self.links {
            ensure!(
                hex_id(&link.trace_id, 32) && hex_id(&link.span_id, 16),
                "collector received an invalid OTLP link"
            );
        }
        Ok(())
    }
}

fn hex_id(id: &str, length: usize) -> bool {
    id.len() == length
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && id.bytes().any(|byte| byte != b'0')
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CollectorReceipt {
    pub acknowledged_requests: usize,
    pub disconnected_requests: usize,
    pub spans: Vec<CollectedSpan>,
}

#[derive(Default)]
struct State {
    receipt: Mutex<CollectorReceipt>,
    changed: Notify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Connected,
    Disconnected,
    Stopped,
}

pub struct OtlpReceiver {
    address: SocketAddr,
    mode: watch::Sender<Mode>,
    state: Arc<State>,
    task: JoinHandle<Result<()>>,
}

impl OtlpReceiver {
    /// Use a case lease's collector port. A bound listener is the ready event.
    pub async fn bind(address: SocketAddr) -> Result<Self> {
        ensure!(
            address.ip().is_loopback(),
            "OTLP fixture must bind loopback"
        );
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .context("bind OTLP collector")?;
        let address = listener.local_addr()?;
        let (mode, receiver) = watch::channel(Mode::Connected);
        let state = Arc::new(State::default());
        let task = tokio::spawn(http::serve(listener, receiver, state.clone()));
        Ok(Self {
            address,
            mode,
            state,
            task,
        })
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}/v1/traces", self.address)
    }

    /// Drop established streams as well as newly accepted sockets.
    pub fn disconnect(&self) {
        self.mode.send_replace(Mode::Disconnected);
    }

    pub fn reconnect(&self) {
        self.mode.send_replace(Mode::Connected);
    }

    pub async fn snapshot(&self) -> CollectorReceipt {
        self.state.receipt.lock().await.clone()
    }

    /// Wait for a measured predicate, never an elapsed outage/readiness sleep.
    pub async fn wait_for(
        &self,
        deadline: Duration,
        predicate: impl Fn(&CollectorReceipt) -> bool,
    ) -> Result<CollectorReceipt> {
        tokio::time::timeout(deadline, async {
            loop {
                let changed = self.state.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let receipt = self.snapshot().await;
                if predicate(&receipt) {
                    return Ok(receipt);
                }
                ensure!(
                    !self.task.is_finished(),
                    "collector stopped before the expected export"
                );
                changed.await;
            }
        })
        .await
        .context("collector evidence deadline")?
    }

    /// Teardown joins every owned connection; malformed exports and task panics
    /// remain scenario failures instead of disappearing with the listener.
    pub async fn finish(self) -> Result<(CollectorReceipt, CleanupReceipt)> {
        self.mode.send_replace(Mode::Stopped);
        self.task.await.context("collector task panicked")??;
        let receipt = self.state.receipt.lock().await.clone();
        Ok((
            receipt,
            CleanupReceipt {
                resource: format!("otlp:{}", self.address),
                closed: true,
                detail: "listener and all accepted streams joined".into(),
            },
        ))
    }
}

fn decode(body: &[u8]) -> Result<Vec<CollectedSpan>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Export {
        resource_spans: Vec<Resource>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resource {
        scope_spans: Vec<Scope>,
    }
    #[derive(Deserialize)]
    struct Scope {
        spans: Vec<CollectedSpan>,
    }
    let export: Export =
        serde_json::from_slice(body).context("decode OTLP ExportTraceServiceRequest")?;
    let spans: Vec<_> = export
        .resource_spans
        .into_iter()
        .flat_map(|resource| resource.scope_spans)
        .flat_map(|scope| scope.spans)
        .collect();
    ensure!(
        !spans.is_empty(),
        "empty OTLP export cannot witness delivery"
    );
    for span in &spans {
        span.validate()?;
    }
    Ok(spans)
}
