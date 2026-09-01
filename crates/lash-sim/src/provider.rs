use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use lash_core::{
    ProviderFailureKind, facade_support::LlmTransportError, llm::transport::TransportRetryVerdict,
};
use lash_llm_transport::{
    LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport, run_with_timeout,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const PROVIDER_WIRE_SCRIPT_SCHEMA: &str = "lash.provider-wire-script.v1";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWireScript {
    pub schema: String,
    pub name: String,
    pub provider_kind: String,
    pub endpoint: ProviderWireEndpoint,
    #[serde(rename = "request_match")]
    pub request_match: ProviderWireRequestMatch,
    #[serde(default)]
    timeline: Vec<ProviderWireEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_provider: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProviderWireProvenance>,
    #[serde(skip)]
    pub(crate) compiled_plan: OnceLock<Result<ScriptedResponsePlan, LlmTransportError>>,
}

impl Clone for ProviderWireScript {
    fn clone(&self) -> Self {
        Self {
            schema: self.schema.clone(),
            name: self.name.clone(),
            provider_kind: self.provider_kind.clone(),
            endpoint: self.endpoint.clone(),
            request_match: self.request_match.clone(),
            timeline: self.timeline.clone(),
            expected_provider: self.expected_provider.clone(),
            provenance: self.provenance.clone(),
            compiled_plan: OnceLock::new(),
        }
    }
}

impl ProviderWireScript {
    pub fn from_json_str(input: &str) -> Result<Self, LlmTransportError> {
        let value: Value = serde_json::from_str(input).map_err(|err| {
            LlmTransportError::new(format!("Invalid Provider Wire Script JSON: {err}"))
                .with_kind(ProviderFailureKind::Validation)
        })?;
        let source = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        let script: Self = serde_json::from_value(value.clone()).map_err(|err| {
            let context = failing_timeline_event_index(&value).map_or_else(
                || format!("Provider Wire Script `{source}` failed to parse"),
                |index| {
                    format!(
                        "Provider Wire Script `{source}` chunk event at index {index} failed to parse"
                    )
                },
            );
            LlmTransportError::new(format!("{context}: {err}"))
                .with_kind(ProviderFailureKind::Validation)
        })?;
        script.validate()?;
        Ok(script)
    }

    pub fn timeline(&self) -> &[ProviderWireEvent] {
        &self.timeline
    }

    pub fn timeline_mut(&mut self) -> &mut Vec<ProviderWireEvent> {
        self.compiled_plan.take();
        &mut self.timeline
    }

    pub(crate) fn from_parts(
        schema: String,
        name: String,
        provider_kind: String,
        endpoint: ProviderWireEndpoint,
        request_match: ProviderWireRequestMatch,
        timeline: Vec<ProviderWireEvent>,
    ) -> Self {
        Self {
            schema,
            name,
            provider_kind,
            endpoint,
            request_match,
            timeline,
            expected_provider: None,
            provenance: None,
            compiled_plan: OnceLock::new(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), LlmTransportError> {
        if self.schema != PROVIDER_WIRE_SCRIPT_SCHEMA {
            return Err(script_validation_error(format!(
                "Provider Wire Script `{}` uses unsupported schema `{}`",
                self.name, self.schema
            )));
        }
        if self.timeline.is_empty() {
            return Err(script_validation_error(format!(
                "Provider Wire Script `{}` has no timeline events",
                self.name
            )));
        }
        self.request_match.validate(&self.name)?;
        let mut previous_at = 0;
        for (index, event) in self.timeline.iter().enumerate() {
            let at = event.at();
            if index > 0 && at < previous_at {
                return Err(script_validation_error(format!(
                    "Provider Wire Script `{}` timeline event `{}` at index {index} moved backward from {previous_at} to {at}",
                    self.name,
                    event.event_name()
                )));
            }
            previous_at = at;
        }
        self.plan()?;
        Ok(())
    }

    fn plan(&self) -> Result<&ScriptedResponsePlan, LlmTransportError> {
        match self
            .compiled_plan
            .get_or_init(|| ScriptedResponsePlan::build(&self.name, &self.timeline))
        {
            Ok(plan) => Ok(plan),
            Err(error) => Err(error.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWireEndpoint {
    pub method: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWireProvenance {
    pub kind: ProviderWireProvenanceKind,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderWireProvenanceKind {
    CapturedLive,
    ProviderDocumentation,
    RealWorldReport,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWireHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWireRequestMatch {
    #[serde(default, skip_serializing_if = "is_false")]
    pub any: bool,
    #[serde(default)]
    pub body: BTreeMap<String, JsonMatcher>,
    #[serde(default)]
    pub headers: BTreeMap<String, HeaderMatcher>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Default for ProviderWireRequestMatch {
    fn default() -> Self {
        Self::any()
    }
}

impl ProviderWireRequestMatch {
    pub fn any() -> Self {
        Self {
            any: true,
            body: BTreeMap::new(),
            headers: BTreeMap::new(),
        }
    }

    fn validate(&self, script_name: &str) -> Result<(), LlmTransportError> {
        let has_predicates = !self.body.is_empty() || !self.headers.is_empty();
        if self.any && has_predicates {
            return Err(script_validation_error(format!(
                "Provider Wire Script `{script_name}` request matcher cannot combine `any` with predicates"
            )));
        }
        if !self.any && !has_predicates {
            return Err(script_validation_error(format!(
                "Provider Wire Script `{script_name}` request matcher must contain a predicate or explicit `any: true`"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JsonMatcher {
    #[serde(default)]
    pub present: Option<bool>,
    #[serde(default)]
    pub equals: Option<Value>,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub contains_role: Option<String>,
    #[serde(default)]
    pub min_len: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderMatcher {
    #[serde(default)]
    pub present: Option<bool>,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub contains: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderWireEvent {
    ResponseStart {
        #[serde(default)]
        at: u64,
        status: u16,
        #[serde(default)]
        headers: Vec<ProviderWireHeader>,
    },
    Body {
        #[serde(default)]
        at: u64,
        data: String,
    },
    Chunk {
        #[serde(default)]
        at: u64,
        payload: ProviderWireChunkPayload,
    },
    Sse {
        #[serde(default)]
        at: u64,
        data: String,
    },
    End {
        #[serde(default)]
        at: u64,
    },
    Disconnect {
        #[serde(default)]
        at: u64,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        retryable: Option<bool>,
    },
    Timeout {
        #[serde(default)]
        at: u64,
        #[serde(default)]
        message: Option<String>,
    },
    HttpError {
        #[serde(default)]
        at: u64,
        status: u16,
        #[serde(default)]
        headers: Vec<ProviderWireHeader>,
        body: String,
    },
    TransportError {
        #[serde(default)]
        at: u64,
        message: String,
        #[serde(default)]
        retryable: Option<bool>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderWireChunkPayload {
    Data(String),
    Bytes(Vec<u8>),
}

impl ProviderWireChunkPayload {
    fn into_bytes(self) -> Bytes {
        match self {
            Self::Data(data) => Bytes::from(data),
            Self::Bytes(bytes) => Bytes::from(bytes),
        }
    }
}

impl ProviderWireEvent {
    pub fn at(&self) -> u64 {
        match self {
            Self::ResponseStart { at, .. }
            | Self::Body { at, .. }
            | Self::Chunk { at, .. }
            | Self::Sse { at, .. }
            | Self::End { at }
            | Self::Disconnect { at, .. }
            | Self::Timeout { at, .. }
            | Self::HttpError { at, .. }
            | Self::TransportError { at, .. } => *at,
        }
    }

    pub fn event_name(&self) -> &'static str {
        match self {
            Self::ResponseStart { .. } => "response_start",
            Self::Body { .. } => "body",
            Self::Chunk { .. } => "chunk",
            Self::Sse { .. } => "sse",
            Self::End { .. } => "end",
            Self::Disconnect { .. } => "disconnect",
            Self::Timeout { .. } => "timeout",
            Self::HttpError { .. } => "http_error",
            Self::TransportError { .. } => "transport_error",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ScriptedResponsePlan {
    Response {
        event_index: usize,
        status: u16,
        headers: Vec<(String, String)>,
        body: BodyPlan,
    },
    HttpError {
        event_index: usize,
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
    },
    Failure {
        event_index: usize,
        error: LlmTransportError,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum BodyPlan {
    Buffered(Vec<BufferedStep>),
    Streamed(Vec<StreamStep>),
}

#[derive(Clone, Debug)]
pub(crate) struct BufferedStep {
    event_index: usize,
    bytes: Option<Bytes>,
}

#[derive(Clone, Debug)]
pub(crate) enum StreamStep {
    Chunk {
        event_index: usize,
        bytes: Bytes,
    },
    End {
        event_index: usize,
    },
    Failure {
        event_index: usize,
        error: LlmTransportError,
    },
}

impl ScriptedResponsePlan {
    fn build(script_name: &str, timeline: &[ProviderWireEvent]) -> Result<Self, LlmTransportError> {
        let first = timeline.first().ok_or_else(|| {
            script_validation_error(format!(
                "Provider Wire Script `{script_name}` has no timeline events"
            ))
        })?;
        match first {
            ProviderWireEvent::ResponseStart {
                status, headers, ..
            } => Ok(Self::Response {
                event_index: 0,
                status: *status,
                headers: header_vec(headers.clone()),
                body: BodyPlan::build(script_name, &timeline[1..])?,
            }),
            ProviderWireEvent::HttpError {
                status,
                headers,
                body,
                ..
            } => {
                require_final_event(script_name, timeline, 0, "http_error")?;
                Ok(Self::HttpError {
                    event_index: 0,
                    status: *status,
                    headers: header_vec(headers.clone()),
                    body: Bytes::from(body.clone()),
                })
            }
            ProviderWireEvent::Timeout { message, .. } => {
                require_final_event(script_name, timeline, 0, "timeout")?;
                Ok(Self::Failure {
                    event_index: 0,
                    error: timeout_error(message.clone()),
                })
            }
            ProviderWireEvent::TransportError {
                message, retryable, ..
            } => {
                require_final_event(script_name, timeline, 0, "transport_error")?;
                Ok(Self::Failure {
                    event_index: 0,
                    error: transport_error(message.clone(), *retryable),
                })
            }
            event => Err(script_validation_error(format!(
                "Provider Wire Script `{script_name}` emitted `{}` before response_start at index 0",
                event.event_name()
            ))),
        }
    }

    #[cfg(test)]
    fn event_indices(&self) -> Vec<usize> {
        match self {
            Self::Response {
                event_index, body, ..
            } => std::iter::once(*event_index)
                .chain(body.event_indices())
                .collect(),
            Self::HttpError { event_index, .. } | Self::Failure { event_index, .. } => {
                vec![*event_index]
            }
        }
    }
}

impl BodyPlan {
    fn build(
        script_name: &str,
        timeline_after_start: &[ProviderWireEvent],
    ) -> Result<Self, LlmTransportError> {
        let mut buffered_steps = Vec::with_capacity(timeline_after_start.len());
        let mut streamed_steps = Vec::with_capacity(timeline_after_start.len());
        let mut streamed = false;

        for (offset, event) in timeline_after_start.iter().enumerate() {
            let event_index = offset + 1;
            match event {
                ProviderWireEvent::Body { data, .. } => {
                    let bytes = Bytes::from(data.clone());
                    buffered_steps.push(BufferedStep {
                        event_index,
                        bytes: Some(bytes.clone()),
                    });
                    streamed_steps.push(StreamStep::Chunk { event_index, bytes });
                }
                ProviderWireEvent::Chunk { payload, .. } => {
                    streamed = true;
                    streamed_steps.push(StreamStep::Chunk {
                        event_index,
                        bytes: payload.clone().into_bytes(),
                    });
                }
                ProviderWireEvent::Sse { data, .. } => {
                    streamed = true;
                    streamed_steps.push(StreamStep::Chunk {
                        event_index,
                        bytes: Bytes::from(format!("data: {data}\n\n")),
                    });
                }
                ProviderWireEvent::End { .. } => {
                    require_final_body_event(script_name, timeline_after_start, offset, "end")?;
                    buffered_steps.push(BufferedStep {
                        event_index,
                        bytes: None,
                    });
                    streamed_steps.push(StreamStep::End { event_index });
                }
                ProviderWireEvent::Disconnect {
                    message, retryable, ..
                } => {
                    require_final_body_event(
                        script_name,
                        timeline_after_start,
                        offset,
                        "disconnect",
                    )?;
                    streamed = true;
                    streamed_steps.push(StreamStep::Failure {
                        event_index,
                        error: disconnect_error(message.clone(), *retryable),
                    });
                }
                ProviderWireEvent::Timeout { message, .. } => {
                    require_final_body_event(script_name, timeline_after_start, offset, "timeout")?;
                    streamed = true;
                    streamed_steps.push(StreamStep::Failure {
                        event_index,
                        error: timeout_error(message.clone()),
                    });
                }
                ProviderWireEvent::TransportError {
                    message, retryable, ..
                } => {
                    require_final_body_event(
                        script_name,
                        timeline_after_start,
                        offset,
                        "transport_error",
                    )?;
                    streamed = true;
                    streamed_steps.push(StreamStep::Failure {
                        event_index,
                        error: transport_error(message.clone(), *retryable),
                    });
                }
                ProviderWireEvent::ResponseStart { .. } => {
                    return Err(script_validation_error(format!(
                        "Provider Wire Script `{script_name}` emitted a second response_start at index {event_index}"
                    )));
                }
                ProviderWireEvent::HttpError { .. } => {
                    return Err(script_validation_error(format!(
                        "Provider Wire Script `{script_name}` emitted http_error after response_start at index {event_index}"
                    )));
                }
            }
        }

        if streamed {
            Ok(Self::Streamed(streamed_steps))
        } else {
            Ok(Self::Buffered(buffered_steps))
        }
    }

    #[cfg(test)]
    fn event_indices(&self) -> impl Iterator<Item = usize> + '_ {
        match self {
            Self::Buffered(steps) => BodyEventIndices::Buffered(steps.iter()),
            Self::Streamed(steps) => BodyEventIndices::Streamed(steps.iter()),
        }
    }
}

#[cfg(test)]
enum BodyEventIndices<'a> {
    Buffered(std::slice::Iter<'a, BufferedStep>),
    Streamed(std::slice::Iter<'a, StreamStep>),
}

#[cfg(test)]
impl Iterator for BodyEventIndices<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Buffered(steps) => steps.next().map(|step| step.event_index),
            Self::Streamed(steps) => steps.next().map(StreamStep::event_index),
        }
    }
}

impl StreamStep {
    fn event_index(&self) -> usize {
        match self {
            Self::Chunk { event_index, .. }
            | Self::End { event_index }
            | Self::Failure { event_index, .. } => *event_index,
        }
    }

    fn into_chunk(self) -> Result<Option<Bytes>, LlmTransportError> {
        match self {
            Self::Chunk { bytes, .. } => Ok(Some(bytes)),
            Self::End { .. } => Ok(None),
            Self::Failure { error, .. } => Err(error),
        }
    }
}

fn require_final_event(
    script_name: &str,
    timeline: &[ProviderWireEvent],
    event_index: usize,
    event_name: &str,
) -> Result<(), LlmTransportError> {
    if event_index + 1 == timeline.len() {
        Ok(())
    } else {
        Err(script_validation_error(format!(
            "Provider Wire Script `{script_name}` `{event_name}` at index {event_index} must be the final timeline event"
        )))
    }
}

fn require_final_body_event(
    script_name: &str,
    timeline_after_start: &[ProviderWireEvent],
    body_offset: usize,
    event_name: &str,
) -> Result<(), LlmTransportError> {
    if body_offset + 1 == timeline_after_start.len() {
        Ok(())
    } else {
        let event_index = body_offset + 1;
        Err(script_validation_error(format!(
            "Provider Wire Script `{script_name}` `{event_name}` at index {event_index} must be the final timeline event"
        )))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ScriptedLlmHttpExchange {
    pub script_name: String,
    pub provider_kind: String,
    pub request: ScriptedLlmHttpRequestExchange,
    pub response: ScriptedLlmHttpResponseExchange,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScriptedLlmHttpRequestExchange {
    pub method: String,
    pub url: String,
    pub path: String,
    pub headers: Vec<ProviderWireHeader>,
    pub body_bytes: usize,
    pub body_shape: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScriptedLlmHttpResponseExchange {
    pub status: Option<u16>,
    pub headers: Vec<ProviderWireHeader>,
    pub event_names: Vec<String>,
    pub event_schedule: Vec<ProviderWireTimelineEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderWireTimelineEntry {
    pub event: String,
    pub at: u64,
}

#[derive(Clone, Debug)]
pub struct ScriptedLlmHttpTransport {
    scripts: Arc<Mutex<VecDeque<ProviderWireScript>>>,
    exchanges: Arc<Mutex<Vec<ScriptedLlmHttpExchange>>>,
    event_schedule: Option<ScriptedTransportSchedule>,
    next_exchange_index: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, Default)]
pub struct ScriptedTransportSchedule {
    inner: Arc<ScriptedTransportScheduleInner>,
}

#[derive(Debug, Default)]
struct ScriptedTransportScheduleInner {
    gates: Mutex<BTreeMap<ScriptedProviderEventKey, Arc<ScriptedTransportEventGate>>>,
    releases: Mutex<Vec<ScriptedProviderEventRelease>>,
    next_release_sequence: AtomicUsize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ScriptedProviderEventKey {
    exchange_index: usize,
    event_index: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScriptedProviderEventRelease {
    pub exchange_index: usize,
    pub event_index: usize,
    pub event_name: String,
    pub at: u64,
    pub blocked_before_release: bool,
    pub release_sequence: usize,
}

#[derive(Debug, Default)]
struct ScriptedTransportEventGate {
    opened: AtomicBool,
    blocked: AtomicBool,
    blocked_notify: tokio::sync::Notify,
    opened_notify: tokio::sync::Notify,
}

impl ScriptedTransportSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn wait_until_blocked(&self, exchange_index: usize, event_index: usize) {
        self.gate(exchange_index, event_index)
            .wait_until_blocked()
            .await;
    }

    /// Release the scheduler-owned provider event whether or not the provider
    /// future has already parked on the gate. A delivered boundary represents byte
    /// availability, so an early release must be buffered instead of being dropped.
    pub fn release(
        &self,
        exchange_index: usize,
        event_index: usize,
        event_name: impl Into<String>,
        at: u64,
    ) -> ScriptedProviderEventRelease {
        let gate = self.gate(exchange_index, event_index);
        self.open_gate(&gate, exchange_index, event_index, event_name.into(), at)
    }

    /// Whether the turn future is currently parked on this gate. Lets the
    /// boundary harness couple gate release to turn liveness (poll until either
    /// the gate blocks or the turn finishes) instead of blocking forever.
    pub fn is_blocked(&self, exchange_index: usize, event_index: usize) -> bool {
        self.gate(exchange_index, event_index).is_blocked()
    }

    fn open_gate(
        &self,
        gate: &ScriptedTransportEventGate,
        exchange_index: usize,
        event_index: usize,
        event_name: String,
        at: u64,
    ) -> ScriptedProviderEventRelease {
        let release = ScriptedProviderEventRelease {
            exchange_index,
            event_index,
            event_name,
            at,
            blocked_before_release: gate.is_blocked(),
            release_sequence: self
                .inner
                .next_release_sequence
                .fetch_add(1, Ordering::SeqCst),
        };
        gate.open();
        self.inner.releases.lock_recover().push(release.clone());
        release
    }

    pub fn releases(&self) -> Vec<ScriptedProviderEventRelease> {
        self.inner.releases.lock_recover().clone()
    }

    async fn wait_for_release(&self, exchange_index: usize, event_index: usize) {
        self.gate(exchange_index, event_index)
            .wait_for_release()
            .await;
    }

    fn gate(&self, exchange_index: usize, event_index: usize) -> Arc<ScriptedTransportEventGate> {
        let key = ScriptedProviderEventKey {
            exchange_index,
            event_index,
        };
        let mut gates = self.inner.gates.lock_recover();
        gates
            .entry(key)
            .or_insert_with(|| Arc::new(ScriptedTransportEventGate::default()))
            .clone()
    }
}

impl ScriptedTransportEventGate {
    fn open(&self) {
        self.opened.store(true, Ordering::SeqCst);
        self.opened_notify.notify_waiters();
    }

    fn is_blocked(&self) -> bool {
        self.blocked.load(Ordering::SeqCst)
    }

    async fn wait_until_blocked(&self) {
        while !self.blocked.load(Ordering::SeqCst) {
            self.blocked_notify.notified().await;
        }
    }

    async fn wait_for_release(&self) {
        if self.opened.load(Ordering::SeqCst) {
            return;
        }
        self.blocked.store(true, Ordering::SeqCst);
        self.blocked_notify.notify_waiters();
        while !self.opened.load(Ordering::SeqCst) {
            self.opened_notify.notified().await;
        }
    }
}

impl ScriptedLlmHttpTransport {
    pub fn new(script: ProviderWireScript) -> Result<Self, LlmTransportError> {
        Self::from_scripts([script])
    }

    pub fn from_scripts(
        scripts: impl IntoIterator<Item = ProviderWireScript>,
    ) -> Result<Self, LlmTransportError> {
        let scripts: VecDeque<_> = scripts.into_iter().collect();
        for script in &scripts {
            script.validate()?;
        }
        Ok(Self {
            scripts: Arc::new(Mutex::new(scripts)),
            exchanges: Arc::new(Mutex::new(Vec::new())),
            event_schedule: None,
            next_exchange_index: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn from_json_str(input: &str) -> Result<Self, LlmTransportError> {
        Self::new(ProviderWireScript::from_json_str(input)?)
    }

    pub fn with_event_schedule(mut self, schedule: ScriptedTransportSchedule) -> Self {
        self.event_schedule = Some(schedule);
        self
    }

    pub fn remaining_scripts(&self) -> Result<usize, LlmTransportError> {
        let scripts = self.scripts.lock_recover();
        Ok(scripts.len())
    }

    pub fn exchanges(&self) -> Result<Vec<ScriptedLlmHttpExchange>, LlmTransportError> {
        let exchanges = self.exchanges.lock_recover();
        Ok(exchanges.clone())
    }

    fn next_script(&self) -> Result<ProviderWireScript, LlmTransportError> {
        let mut scripts = self.scripts.lock_recover();
        scripts.pop_front().ok_or_else(|| {
            LlmTransportError::new("No Provider Wire Script remained for LLM request")
                .with_kind(ProviderFailureKind::Transport)
        })
    }

    fn record_exchange(&self, exchange: ScriptedLlmHttpExchange) -> Result<(), LlmTransportError> {
        let mut exchanges = self.exchanges.lock_recover();
        exchanges.push(exchange);
        Ok(())
    }

    fn record_or_replace_pending_exchange(
        &self,
        exchange: ScriptedLlmHttpExchange,
    ) -> Result<(), LlmTransportError> {
        let mut exchanges = self.exchanges.lock_recover();
        if let Some(existing) = exchanges.iter_mut().rev().find(|existing| {
            existing.script_name == exchange.script_name && existing.response.event_names.is_empty()
        }) {
            *existing = exchange;
        } else {
            exchanges.push(exchange);
        }
        Ok(())
    }
}

#[async_trait]
impl LlmHttpTransport for ScriptedLlmHttpTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let script = self.next_script()?;
        match_request(&script, &request)?;
        if let Some(schedule) = &self.event_schedule {
            let exchange_index = self.next_exchange_index.fetch_add(1, Ordering::SeqCst);
            self.record_exchange(scripted_exchange(&script, &request, false))?;
            let timeout_message = request
                .response_start_timeout_message
                .as_deref()
                .unwrap_or("LLM HTTP response start timed out");
            let result = execute_scheduled_script(
                &script,
                exchange_index,
                schedule.clone(),
                timeout,
                timeout_message,
            )
            .await;
            if result.is_ok() {
                self.record_or_replace_pending_exchange(scripted_exchange(
                    &script, &request, true,
                ))?;
            }
            return result;
        }
        let result = execute_script(&script);
        let exchange = scripted_exchange(&script, &request, true);
        self.record_exchange(exchange)?;
        result
    }
}

fn scripted_exchange(
    script: &ProviderWireScript,
    request: &LlmHttpRequest,
    include_response: bool,
) -> ScriptedLlmHttpExchange {
    ScriptedLlmHttpExchange {
        script_name: script.name.clone(),
        provider_kind: script.provider_kind.clone(),
        request: ScriptedLlmHttpRequestExchange {
            method: request.method.as_str().to_string(),
            url: request.url.clone(),
            path: request_path(&request.url),
            headers: redacted_http_headers(&request.headers),
            body_bytes: request.body.len(),
            body_shape: request_body_shape(&request.body),
        },
        response: scripted_response_exchange(script, include_response),
    }
}

fn scripted_response_exchange(
    script: &ProviderWireScript,
    include_response: bool,
) -> ScriptedLlmHttpResponseExchange {
    if !include_response {
        return ScriptedLlmHttpResponseExchange {
            status: None,
            headers: Vec::new(),
            event_names: Vec::new(),
            event_schedule: Vec::new(),
        };
    }

    let mut status = None;
    let mut headers = Vec::new();
    for event in script.timeline() {
        match event {
            ProviderWireEvent::ResponseStart {
                status: next_status,
                headers: next_headers,
                ..
            }
            | ProviderWireEvent::HttpError {
                status: next_status,
                headers: next_headers,
                ..
            } => {
                status = Some(*next_status);
                headers = redacted_provider_headers(next_headers);
                break;
            }
            _ => {}
        }
    }

    ScriptedLlmHttpResponseExchange {
        status,
        headers,
        event_names: script
            .timeline()
            .iter()
            .map(|event| event.event_name().to_string())
            .collect(),
        event_schedule: script
            .timeline()
            .iter()
            .map(|event| ProviderWireTimelineEntry {
                event: event.event_name().to_string(),
                at: event.at(),
            })
            .collect(),
    }
}

fn redacted_http_headers(headers: &[(String, String)]) -> Vec<ProviderWireHeader> {
    headers
        .iter()
        .map(|(name, value)| ProviderWireHeader {
            name: name.clone(),
            value: redacted_header_value(name, value),
        })
        .collect()
}

fn redacted_provider_headers(headers: &[ProviderWireHeader]) -> Vec<ProviderWireHeader> {
    headers
        .iter()
        .map(|header| ProviderWireHeader {
            name: header.name.clone(),
            value: redacted_header_value(&header.name, &header.value),
        })
        .collect()
}

fn redacted_header_value(name: &str, value: &str) -> String {
    let lower_name = name.to_ascii_lowercase();
    let lower_value = value.to_ascii_lowercase();
    if matches!(
        lower_name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || lower_name.contains("api-key")
        || lower_name.contains("token")
        || lower_value.contains("bearer ")
        || lower_value.contains("sk-")
    {
        "[redacted]".to_string()
    } else {
        value.to_string()
    }
}

fn request_body_shape(body: &Bytes) -> Value {
    serde_json::from_slice::<Value>(body)
        .map(|value| json_shape(&value))
        .unwrap_or_else(|_| {
            json!({
                "type": "bytes",
                "bytes": body.len()
            })
        })
}

fn json_shape(value: &Value) -> Value {
    match value {
        Value::Null => json!({ "type": "null" }),
        Value::Bool(_) => json!({ "type": "bool" }),
        Value::Number(_) => json!({ "type": "number" }),
        Value::String(text) => json!({
            "type": "string",
            "bytes": text.len()
        }),
        Value::Array(items) => {
            let mut shape = Map::new();
            shape.insert("type".to_string(), json!("array"));
            shape.insert("len".to_string(), json!(items.len()));
            if let Some(first) = items.first() {
                shape.insert("items".to_string(), json_shape(first));
            }
            Value::Object(shape)
        }
        Value::Object(fields) => {
            let mut shaped_fields = Map::new();
            for (key, value) in fields {
                shaped_fields.insert(key.clone(), json_shape(value));
            }
            json!({
                "type": "object",
                "keys": fields.keys().cloned().collect::<Vec<_>>(),
                "fields": shaped_fields
            })
        }
    }
}

#[derive(Debug)]
struct ScriptedByteStream {
    steps: VecDeque<StreamStep>,
}

impl ScriptedByteStream {
    fn new(steps: Vec<StreamStep>) -> Self {
        Self {
            steps: steps.into(),
        }
    }
}

#[async_trait]
impl LlmByteStream for ScriptedByteStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        self.steps
            .pop_front()
            .map_or(Ok(None), StreamStep::into_chunk)
    }
}

#[derive(Debug)]
struct ScheduledScriptedByteStream {
    exchange_index: usize,
    schedule: ScriptedTransportSchedule,
    steps: VecDeque<StreamStep>,
}

impl ScheduledScriptedByteStream {
    fn new(
        exchange_index: usize,
        schedule: ScriptedTransportSchedule,
        steps: Vec<StreamStep>,
    ) -> Self {
        Self {
            exchange_index,
            schedule,
            steps: steps.into(),
        }
    }
}

#[async_trait]
impl LlmByteStream for ScheduledScriptedByteStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        let Some(step) = self.steps.pop_front() else {
            return Ok(None);
        };
        let event_index = step.event_index();
        self.schedule
            .wait_for_release(self.exchange_index, event_index)
            .await;
        step.into_chunk()
    }
}

async fn execute_scheduled_script(
    script: &ProviderWireScript,
    exchange_index: usize,
    schedule: ScriptedTransportSchedule,
    timeout: Option<std::time::Duration>,
    response_start_timeout_message: &str,
) -> Result<LlmHttpResponse, LlmTransportError> {
    match script.plan()?.clone() {
        ScriptedResponsePlan::Response {
            event_index,
            status,
            headers,
            body,
        } => {
            wait_for_scheduled_response_event(
                &schedule,
                exchange_index,
                event_index,
                timeout,
                response_start_timeout_message,
            )
            .await?;
            let body = match body {
                BodyPlan::Buffered(steps) => {
                    let mut bytes = BytesMut::new();
                    for step in steps {
                        schedule
                            .wait_for_release(exchange_index, step.event_index)
                            .await;
                        if let Some(chunk) = step.bytes {
                            bytes.extend_from_slice(&chunk);
                        }
                    }
                    LlmHttpBody::buffered(bytes.freeze())
                }
                BodyPlan::Streamed(steps) => LlmHttpBody::streamed(
                    ScheduledScriptedByteStream::new(exchange_index, schedule, steps),
                ),
            };
            Ok(LlmHttpResponse {
                status,
                headers,
                body,
            })
        }
        ScriptedResponsePlan::HttpError {
            event_index,
            status,
            headers,
            body,
        } => {
            wait_for_scheduled_response_event(
                &schedule,
                exchange_index,
                event_index,
                timeout,
                response_start_timeout_message,
            )
            .await?;
            Ok(LlmHttpResponse {
                status,
                headers,
                body: LlmHttpBody::buffered(body),
            })
        }
        ScriptedResponsePlan::Failure { event_index, error } => {
            wait_for_scheduled_response_event(
                &schedule,
                exchange_index,
                event_index,
                timeout,
                response_start_timeout_message,
            )
            .await?;
            Err(error)
        }
    }
}

async fn wait_for_scheduled_response_event(
    schedule: &ScriptedTransportSchedule,
    exchange_index: usize,
    event_index: usize,
    timeout: Option<std::time::Duration>,
    timeout_message: &str,
) -> Result<(), LlmTransportError> {
    run_with_timeout(
        async {
            schedule.wait_for_release(exchange_index, event_index).await;
            Ok(())
        },
        timeout,
        timeout_message,
    )
    .await
}

fn match_request(
    script: &ProviderWireScript,
    request: &LlmHttpRequest,
) -> Result<(), LlmTransportError> {
    if !script
        .endpoint
        .method
        .eq_ignore_ascii_case(request.method.as_str())
    {
        return Err(script_match_error(format!(
            "Provider Wire Script `{}` expected method `{}`, got `{}`",
            script.name, script.endpoint.method, request.method
        )));
    }

    let request_path = request_path(&request.url);
    if request_path != script.endpoint.path && !request_path.ends_with(&script.endpoint.path) {
        return Err(script_match_error(format!(
            "Provider Wire Script `{}` expected path `{}`, got `{request_path}`",
            script.name, script.endpoint.path
        )));
    }

    if script.request_match.any {
        return Ok(());
    }

    for (name, matcher) in &script.request_match.headers {
        let values = request
            .headers
            .iter()
            .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        matcher.match_values(&script.name, name, &values)?;
    }

    if !script.request_match.body.is_empty() {
        let body: Value = serde_json::from_slice(&request.body).map_err(|err| {
            script_match_error(format!(
                "Provider Wire Script `{}` could not parse request body JSON: {err}",
                script.name
            ))
        })?;
        for (path, matcher) in &script.request_match.body {
            matcher.match_value(&script.name, path, select_path(&body, path)?)?;
        }
    }

    Ok(())
}

fn execute_script(script: &ProviderWireScript) -> Result<LlmHttpResponse, LlmTransportError> {
    match script.plan()?.clone() {
        ScriptedResponsePlan::Response {
            status,
            headers,
            body,
            ..
        } => {
            let body = match body {
                BodyPlan::Buffered(steps) => {
                    let mut bytes = BytesMut::new();
                    for step in steps {
                        if let Some(chunk) = step.bytes {
                            bytes.extend_from_slice(&chunk);
                        }
                    }
                    LlmHttpBody::buffered(bytes.freeze())
                }
                BodyPlan::Streamed(steps) => LlmHttpBody::streamed(ScriptedByteStream::new(steps)),
            };
            Ok(LlmHttpResponse {
                status,
                headers,
                body,
            })
        }
        ScriptedResponsePlan::HttpError {
            status,
            headers,
            body,
            ..
        } => Ok(LlmHttpResponse {
            status,
            headers,
            body: LlmHttpBody::buffered(body),
        }),
        ScriptedResponsePlan::Failure { error, .. } => Err(error),
    }
}

impl JsonMatcher {
    fn match_value(
        &self,
        script_name: &str,
        field: &str,
        value: Option<&Value>,
    ) -> Result<(), LlmTransportError> {
        if let Some(present) = self.present
            && present != value.is_some()
        {
            return Err(script_match_error(format!(
                "Provider Wire Script `{script_name}` field `{field}` presence mismatch: expected present={present}, actual present={}",
                value.is_some()
            )));
        }
        if let Some(expected) = &self.equals
            && value != Some(expected)
        {
            return Err(script_match_error(format!(
                "Provider Wire Script `{script_name}` field `{field}` equality mismatch: expected {}, actual {}",
                expected,
                value
                    .map(Value::to_string)
                    .unwrap_or_else(|| "<missing>".to_string())
            )));
        }
        if let Some(needle) = &self.contains {
            let contains = value.is_some_and(|value| match value {
                Value::String(text) => text.contains(needle),
                other => other.to_string().contains(needle),
            });
            if !contains {
                return Err(script_match_error(format!(
                    "Provider Wire Script `{script_name}` field `{field}` did not contain `{needle}`; actual {}",
                    value
                        .map(Value::to_string)
                        .unwrap_or_else(|| "<missing>".to_string())
                )));
            }
        }
        if let Some(role) = &self.contains_role {
            let contains_role = value.and_then(Value::as_array).is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("role").and_then(Value::as_str) == Some(role))
            });
            if !contains_role {
                return Err(script_match_error(format!(
                    "Provider Wire Script `{script_name}` field `{field}` did not contain role `{role}`; actual {}",
                    value
                        .map(Value::to_string)
                        .unwrap_or_else(|| "<missing>".to_string())
                )));
            }
        }
        if let Some(min_len) = self.min_len {
            let actual_len = value.and_then(Value::as_array).map_or(0, Vec::len);
            if actual_len < min_len {
                return Err(script_match_error(format!(
                    "Provider Wire Script `{script_name}` field `{field}` length {actual_len} < {min_len}; actual {}",
                    value
                        .map(Value::to_string)
                        .unwrap_or_else(|| "<missing>".to_string())
                )));
            }
        }
        Ok(())
    }
}

impl HeaderMatcher {
    fn match_values(
        &self,
        script_name: &str,
        name: &str,
        values: &[&str],
    ) -> Result<(), LlmTransportError> {
        let actual_present = !values.is_empty();
        if let Some(present) = self.present
            && present != actual_present
        {
            return Err(script_match_error(format!(
                "Provider Wire Script `{script_name}` header `{name}` presence mismatch: expected present={present}, actual values={values:?}"
            )));
        }
        if let Some(expected) = &self.equals
            && !values.iter().any(|value| *value == expected)
        {
            return Err(script_match_error(format!(
                "Provider Wire Script `{script_name}` header `{name}` equality mismatch: expected `{expected}`, actual values={values:?}"
            )));
        }
        if let Some(needle) = &self.contains
            && !values.iter().any(|value| value.contains(needle))
        {
            return Err(script_match_error(format!(
                "Provider Wire Script `{script_name}` header `{name}` did not contain `{needle}`; actual values={values:?}"
            )));
        }
        Ok(())
    }
}

fn request_path(url: &str) -> String {
    let without_origin = url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|idx| &rest[idx..]))
        .unwrap_or(url);
    let path = without_origin.split('?').next().unwrap_or(without_origin);
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

fn header_vec(headers: Vec<ProviderWireHeader>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|header| (header.name, header.value))
        .collect()
}

fn failing_timeline_event_index(value: &Value) -> Option<usize> {
    value
        .get("timeline")
        .and_then(Value::as_array)
        .and_then(|timeline| {
            timeline.iter().enumerate().find_map(|(index, event)| {
                serde_json::from_value::<ProviderWireEvent>(event.clone())
                    .err()
                    .map(|_| index)
            })
        })
}

fn disconnect_error(message: Option<String>, retryable: Option<bool>) -> LlmTransportError {
    let retry_verdict = if retryable.unwrap_or(true) {
        TransportRetryVerdict::RetryableTransient
    } else {
        TransportRetryVerdict::NotRetryable
    };
    LlmTransportError::new(format!(
        "Stream read failed: {}",
        message.unwrap_or_else(|| "scripted disconnect".to_string())
    ))
    .with_kind(ProviderFailureKind::Stream)
    .with_retry_verdict(retry_verdict)
}

fn timeout_error(message: Option<String>) -> LlmTransportError {
    LlmTransportError::new(message.unwrap_or_else(|| "scripted provider timeout".to_string()))
        .with_kind(ProviderFailureKind::Timeout)
        .with_code("timeout")
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
}

fn transport_error(message: String, retryable: Option<bool>) -> LlmTransportError {
    let retry_verdict = if retryable.unwrap_or(true) {
        TransportRetryVerdict::RetryableTransient
    } else {
        TransportRetryVerdict::NotRetryable
    };
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Transport)
        .with_retry_verdict(retry_verdict)
}

fn select_path<'a>(root: &'a Value, path: &str) -> Result<Option<&'a Value>, LlmTransportError> {
    let mut current = root;
    for segment in parse_path(path)? {
        match segment {
            PathSegment::Key(key) => {
                let Some(next) = current.get(key.as_str()) else {
                    return Ok(None);
                };
                current = next;
            }
            PathSegment::Index(index) => {
                let Some(next) = current.as_array().and_then(|items| items.get(index)) else {
                    return Ok(None);
                };
                current = next;
            }
        }
    }
    Ok(Some(current))
}

#[derive(Debug, PartialEq, Eq)]
enum PathSegment {
    Key(String),
    Index(usize),
}

fn parse_path(path: &str) -> Result<Vec<PathSegment>, LlmTransportError> {
    if path.trim().is_empty() {
        return Err(script_validation_error(
            "Provider Wire Script request matcher path cannot be empty".to_string(),
        ));
    }

    let mut segments = Vec::new();
    for raw_part in path.split('.') {
        if raw_part.is_empty() {
            return Err(script_validation_error(format!(
                "Provider Wire Script request matcher path `{path}` has an empty segment"
            )));
        }

        let Some((key, tail)) = raw_part.split_once('[') else {
            segments.push(PathSegment::Key(raw_part.to_string()));
            continue;
        };
        if !key.is_empty() {
            segments.push(PathSegment::Key(key.to_string()));
        }

        let mut rest = tail;
        loop {
            let Some((index, tail)) = rest.split_once(']') else {
                return Err(script_validation_error(format!(
                    "Provider Wire Script request matcher path `{path}` has an unterminated array index"
                )));
            };
            let index = index.parse::<usize>().map_err(|_| {
                script_validation_error(format!(
                    "Provider Wire Script request matcher path `{path}` has invalid array index `{index}`"
                ))
            })?;
            segments.push(PathSegment::Index(index));
            if tail.is_empty() {
                break;
            }
            let Some(tail) = tail.strip_prefix('[') else {
                return Err(script_validation_error(format!(
                    "Provider Wire Script request matcher path `{path}` has invalid bracket syntax"
                )));
            };
            rest = tail;
        }
    }

    Ok(segments)
}

fn script_validation_error(message: String) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Validation)
        .with_code("provider_wire_script")
}

fn script_match_error(message: String) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Validation)
        .with_code("provider_wire_script_mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use lash_core::llm::transport::ProviderFailureKind;
    use lash_core::llm::types::{
        LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmRole, LlmStreamEvent,
        LlmTerminalReason, LlmToolChoice, LlmToolSpec,
    };
    use lash_core::provider::{
        DefaultProviderFailureClassifier, Provider, ProviderFailureClassifier, ProviderOptions,
        ProviderReliability, RequestTimeout,
    };
    use lash_llm_transport::LlmHttpBody;
    use lash_provider_openai::{OpenAiCompatibleProvider, OpenAiProvider};
    use serde_json::json;

    use crate::canonical_scripts::{
        CANONICAL_SCRIPTS, OPENAI_COMPAT_DISCONNECT, OPENAI_COMPAT_RATE_LIMIT,
        OPENAI_COMPAT_TOOL_CALL, OPENAI_COMPAT_VALIDATION, OPENAI_RESPONSES_TEXT,
    };

    const RATE_LIMIT_BODY: &str = "{\"error\":{\"message\":\"Rate limit reached for requests\",\"type\":\"rate_limit_error\",\"code\":\"rate_limit_exceeded\"}}";
    const VALIDATION_BODY: &str = "{\"error\":{\"message\":\"Invalid request: tools[0].function.parameters is invalid\",\"type\":\"invalid_request_error\",\"code\":\"invalid_request_error\"}}";

    #[tokio::test]
    async fn provider_wire_script_openai_compatible_chat_stream_uses_real_provider_parser() {
        let (events, sender) = event_collector();
        let mut provider = scripted_provider(OPENAI_COMPAT_TOOL_CALL);

        let response = provider
            .complete(request(Some(sender)))
            .await
            .expect("scripted response");

        assert_eq!(response.terminal_reason, LlmTerminalReason::ToolUse);
        assert_eq!(response.full_text(), "café ");
        let deltas = text_deltas(&events);
        assert_eq!(deltas, vec!["café ".to_string()]);

        let tool_call = response
            .parts
            .iter()
            .find_map(|part| match part {
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    ..
                } => Some((call_id, tool_name, input_json)),
                _ => None,
            })
            .expect("tool call part");
        assert_eq!(tool_call.0, "call_1");
        assert_eq!(tool_call.1, "lookup");
        assert_eq!(tool_call.2, "{\"q\":\"x\"}");
    }

    #[tokio::test]
    async fn provider_wire_script_openai_compatible_rate_limit_error_preserves_envelope() {
        let mut provider = scripted_provider(OPENAI_COMPAT_RATE_LIMIT);

        let err = provider
            .complete(request(None))
            .await
            .expect_err("rate limit error");

        assert_eq!(err.status, Some(429));
        assert_eq!(
            err.raw.as_deref().map(String::as_str),
            Some(RATE_LIMIT_BODY)
        );
        assert_eq!(
            err.headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
                .count(),
            2
        );
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(request_body_json(&err)["model"], "openai/gpt-5.4");
        assert_eq!(request_body_json(&err)["stream"], false);
        assert_eq!(
            err.retry_verdict,
            TransportRetryVerdict::RetryableThrottle {
                retry_after: Some(Duration::from_secs(7)),
            }
        );

        let classified = DefaultProviderFailureClassifier.classify(err);
        assert_eq!(classified.kind, ProviderFailureKind::Quota);
        assert!(classified.is_retryable());
        assert_eq!(classified.retry_after(), Some(Duration::from_secs(7)));
    }

    #[tokio::test]
    async fn provider_wire_script_openai_compatible_validation_error_preserves_envelope() {
        let mut provider = scripted_provider(OPENAI_COMPAT_VALIDATION);

        let err = provider
            .complete(request(None))
            .await
            .expect_err("validation error");

        assert_eq!(err.status, Some(400));
        assert_eq!(
            err.raw.as_deref().map(String::as_str),
            Some(VALIDATION_BODY)
        );
        assert_eq!(
            err.headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
                .map(|(_, value)| value.as_str()),
            Some("req-validation")
        );
        assert_eq!(
            request_body_json(&err)["tools"][0]["function"]["name"],
            "lookup"
        );
        assert!(!err.is_retryable());

        let classified = DefaultProviderFailureClassifier.classify(err);
        assert_eq!(classified.kind, ProviderFailureKind::Validation);
        assert!(!classified.is_retryable());
    }

    #[tokio::test]
    async fn provider_wire_script_openai_compatible_mid_stream_disconnect_surfaces_stream_error() {
        let (events, sender) = event_collector();
        let mut provider = scripted_provider(OPENAI_COMPAT_DISCONNECT);

        let err = provider
            .complete(request(Some(sender)))
            .await
            .expect_err("mid-stream disconnect");

        assert_eq!(err.kind, ProviderFailureKind::Stream);
        assert!(err.is_retryable());
        assert!(err.message.contains("scripted socket closed"));
        assert_eq!(text_deltas(&events), vec!["partial".to_string()]);
    }

    #[tokio::test]
    async fn provider_wire_script_direct_openai_responses_uses_real_provider_parser() {
        let transport =
            Arc::new(ScriptedLlmHttpTransport::from_json_str(OPENAI_RESPONSES_TEXT).unwrap());
        let mut provider = OpenAiProvider::new("test-key").with_transport(transport);

        let response = provider
            .complete(responses_request())
            .await
            .expect("scripted OpenAI Responses response");

        assert_eq!(response.terminal_reason, LlmTerminalReason::Stop);
        assert_eq!(response.full_text(), "Direct answer.");
        assert_eq!(response.usage.input_tokens, 5);
        assert_eq!(response.usage.output_tokens, 2);
    }

    #[tokio::test]
    async fn provider_wire_script_cancellation_before_response_start_commits_no_output() {
        let schedule = ScriptedTransportSchedule::new();
        let transport = Arc::new(
            ScriptedLlmHttpTransport::from_json_str(OPENAI_COMPAT_TOOL_CALL)
                .unwrap()
                .with_event_schedule(schedule.clone()),
        );
        let (events, sender) = event_collector();
        let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
            .with_transport(transport);

        let task = tokio::spawn(async move { provider.complete(request(Some(sender))).await });
        schedule.wait_until_blocked(0, 0).await;
        task.abort();

        let join_err = task.await.expect_err("cancelled provider task");
        assert!(join_err.is_cancelled());
        assert!(
            events.lock_recover().is_empty(),
            "no stream events should be committed before response start"
        );
    }

    #[tokio::test]
    async fn scripted_transport_response_start_gate_timeout_uses_production_timeout_envelope() {
        let schedule = ScriptedTransportSchedule::new();
        let transport = Arc::new(
            ScriptedLlmHttpTransport::from_json_str(OPENAI_COMPAT_TOOL_CALL)
                .unwrap()
                .with_event_schedule(schedule),
        );
        let provider_transport: Arc<dyn lash_llm_transport::LlmHttpTransport> = transport.clone();
        let (events, sender) = event_collector();
        let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
            .with_options(ProviderOptions {
                reliability: ProviderReliability::default()
                    .request_timeout(Some(RequestTimeout::Millis(1)))
                    .stream_chunk_timeout_ms(Some(1)),
                ..ProviderOptions::default()
            })
            .with_transport(provider_transport);

        let err = provider
            .complete(request(Some(sender)))
            .await
            .expect_err("response start gate should time out");

        assert_eq!(err.kind, ProviderFailureKind::Timeout);
        assert_eq!(err.code.as_deref(), Some("timeout"));
        assert!(err.is_retryable());
        assert!(err.status.is_none());
        assert!(
            events.lock_recover().is_empty(),
            "response-start timeout should not commit stream events"
        );

        let exchanges = transport.exchanges().expect("exchange log");
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].request.path, "/chat/completions");
        assert!(
            exchanges[0]
                .request
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("authorization")
                    && header.value == "[redacted]")
        );
        assert!(exchanges[0].response.event_names.is_empty());
    }

    #[tokio::test]
    async fn scripted_transport_buffers_scheduler_releases_before_provider_parks() {
        let schedule = ScriptedTransportSchedule::new();
        let script = ProviderWireScript::from_json_str(OPENAI_COMPAT_TOOL_CALL).expect("script");
        for (event_index, wire_event) in script.timeline().iter().enumerate() {
            let release =
                schedule.release(0, event_index, wire_event.event_name(), wire_event.at());
            assert!(
                !release.blocked_before_release,
                "regression setup must release before the provider parks on event {event_index}"
            );
        }
        let transport = Arc::new(
            ScriptedLlmHttpTransport::from_scripts([script])
                .expect("valid scripted provider")
                .with_event_schedule(schedule),
        );
        let provider_transport: Arc<dyn lash_llm_transport::LlmHttpTransport> = transport.clone();
        let (events, sender) = event_collector();
        let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
            .with_options(ProviderOptions {
                reliability: ProviderReliability::default()
                    .request_timeout(Some(RequestTimeout::Millis(1)))
                    .stream_chunk_timeout_ms(Some(1)),
                ..ProviderOptions::default()
            })
            .with_transport(provider_transport);

        let response = provider
            .complete(request(Some(sender)))
            .await
            .expect("early scheduler releases must be buffered, not retried into no-script");

        assert_eq!(response.terminal_reason, LlmTerminalReason::ToolUse);
        assert_eq!(response.full_text(), "café ");
        assert_eq!(text_deltas(&events), vec!["café ".to_string()]);
        assert_eq!(transport.remaining_scripts().expect("remaining scripts"), 0);
        let exchanges = transport.exchanges().expect("exchange log");
        assert_eq!(
            exchanges.len(),
            1,
            "a success-required turn with one script must not retry after scheduler-owned releases"
        );
    }

    #[test]
    fn provider_wire_script_rejects_non_monotonic_timeline_at_metadata() {
        let err = ProviderWireScript::from_json_str(
            r#"{
              "schema": "lash.provider-wire-script.v1",
              "name": "non-monotonic",
              "provider_kind": "openai-compatible",
              "endpoint": { "method": "POST", "path": "/chat/completions" },
              "request_match": { "any": true },
              "timeline": [
                { "at": 10, "event": "response_start", "status": 200 },
                { "at": 5, "event": "end" }
              ]
            }"#,
        )
        .expect_err("non-monotonic at metadata should fail validation");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("moved backward"));
    }

    #[test]
    fn provider_wire_script_rejects_non_terminal_end() {
        let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "response_start", "status": 200 },
            { "event": "end" },
            { "event": "body", "data": "unreachable" }
        ])))
        .expect_err("end must be the final timeline event");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("end"));
        assert!(err.message.contains("final"));
    }

    #[test]
    fn provider_wire_script_rejects_chunk_before_response_start() {
        let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "chunk", "payload": { "data": "premature" } },
            { "event": "response_start", "status": 200 },
            { "event": "end" }
        ])))
        .expect_err("chunk before response_start must fail validation");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("chunk"));
        assert!(err.message.contains("before response_start"));
    }

    #[test]
    fn provider_wire_script_rejects_empty_chunk_payload_at_load_with_path_and_index() {
        let input = json!({
            "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
            "name": "provider-scripts/witness-empty-chunk.json",
            "provider_kind": "openai-compatible",
            "endpoint": { "method": "POST", "path": "/chat/completions" },
            "request_match": { "any": true },
            "timeline": [
                { "event": "response_start", "status": 200 },
                { "event": "chunk", "payload": { "data": "valid" } },
                { "event": "chunk" },
                { "event": "end" }
            ]
        })
        .to_string();

        let err = ProviderWireScript::from_json_str(&input)
            .expect_err("a chunk without a payload must fail while loading");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(
            err.message
                .contains("provider-scripts/witness-empty-chunk.json")
        );
        assert!(err.message.contains("chunk event at index 2"));
    }

    #[test]
    fn provider_wire_script_rejects_both_chunk_payloads_at_load_with_path_and_index() {
        let input = json!({
            "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
            "name": "provider-scripts/witness-both-chunk-payloads.json",
            "provider_kind": "openai-compatible",
            "endpoint": { "method": "POST", "path": "/chat/completions" },
            "request_match": { "any": true },
            "timeline": [
                { "event": "response_start", "status": 200 },
                { "event": "chunk", "payload": { "data": "valid" } },
                { "event": "chunk", "data": "text", "bytes": [116, 101, 120, 116] },
                { "event": "end" }
            ]
        })
        .to_string();

        let err = ProviderWireScript::from_json_str(&input)
            .expect_err("a chunk with both payload alternatives must fail while loading");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(
            err.message
                .contains("provider-scripts/witness-both-chunk-payloads.json")
        );
        assert!(err.message.contains("chunk event at index 2"));
    }

    #[test]
    fn provider_wire_script_rejects_empty_request_match_at_load() {
        let input = json!({
            "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
            "name": "provider-scripts/witness-empty-request-match.json",
            "provider_kind": "openai-compatible",
            "endpoint": { "method": "POST", "path": "/chat/completions" },
            "request_match": {},
            "timeline": [{ "event": "transport_error", "message": "fixture error" }]
        })
        .to_string();

        let err = ProviderWireScript::from_json_str(&input)
            .expect_err("an empty request matcher must fail while loading");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(
            err.message
                .contains("provider-scripts/witness-empty-request-match.json")
        );
        assert!(err.message.contains("request matcher"));
    }

    #[test]
    fn provider_wire_script_rejects_sse_before_response_start() {
        let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "sse", "data": "{}" },
            { "event": "response_start", "status": 200 },
            { "event": "end" }
        ])))
        .expect_err("SSE before response_start must fail validation");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("sse"));
        assert!(err.message.contains("before response_start"));
    }

    #[test]
    fn provider_wire_script_rejects_body_before_response_start() {
        let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "body", "data": "premature" },
            { "event": "response_start", "status": 200 },
            { "event": "end" }
        ])))
        .expect_err("body before response_start must fail validation");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("body"));
        assert!(err.message.contains("before response_start"));
    }

    #[test]
    fn provider_wire_script_rejects_http_error_after_response_start() {
        let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "response_start", "status": 200 },
            { "event": "http_error", "status": 429, "body": "rate limited" }
        ])))
        .expect_err("http_error after response_start must fail validation");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("http_error after response_start"));
    }

    #[test]
    fn provider_wire_script_rejects_second_response_start() {
        let err = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "response_start", "status": 200 },
            { "event": "response_start", "status": 201 },
            { "event": "end" }
        ])))
        .expect_err("second response_start must fail validation");

        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("second response_start"));
    }

    #[test]
    fn canonical_provider_wire_script_plans_cover_every_timeline_event_index() {
        for canonical in CANONICAL_SCRIPTS {
            let script = ProviderWireScript::from_json_str(canonical.content)
                .unwrap_or_else(|error| panic!("{} failed to parse: {error}", canonical.path));
            assert_eq!(
                script.plan().expect("compiled plan").event_indices(),
                (0..script.timeline().len()).collect::<Vec<_>>(),
                "{} plan must cover each timeline event exactly once",
                canonical.path
            );
        }
    }

    #[test]
    fn cloned_provider_wire_script_recompiles_a_replaced_timeline() {
        let script = ProviderWireScript::from_json_str(OPENAI_COMPAT_TOOL_CALL).expect("script");
        let mut failure = script.clone();
        *failure.timeline_mut() = vec![ProviderWireEvent::TransportError {
            at: 0,
            message: "retry me".to_string(),
            retryable: Some(true),
        }];

        assert!(matches!(
            failure.plan().expect("recompiled failure plan"),
            ScriptedResponsePlan::Failure { event_index: 0, .. }
        ));
    }

    #[test]
    fn in_place_provider_wire_script_mutation_recompiles_before_execution() {
        let mut script =
            ProviderWireScript::from_json_str(OPENAI_COMPAT_TOOL_CALL).expect("script");
        script.plan().expect("initial plan");

        let timeline = script.timeline_mut();
        timeline.clear();
        timeline.push(ProviderWireEvent::TransportError {
            at: 0,
            message: "retry me".to_string(),
            retryable: Some(true),
        });

        let error = execute_script(&script).expect_err("mutated plan should execute as failure");
        assert_eq!(error.kind, ProviderFailureKind::Transport);
        assert_eq!(error.message, "retry me");
    }

    #[test]
    fn scripted_transport_from_scripts_validates_programmatic_scripts() {
        let timeline = vec![
            ProviderWireEvent::Chunk {
                at: 0,
                payload: ProviderWireChunkPayload::Data("premature".to_string()),
            },
            ProviderWireEvent::ResponseStart {
                at: 0,
                status: 200,
                headers: Vec::new(),
            },
            ProviderWireEvent::End { at: 0 },
        ];
        let json_error = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "chunk", "payload": { "data": "premature" } },
            { "event": "response_start", "status": 200 },
            { "event": "end" }
        ])))
        .expect_err("the JSON path should reject the illegal shape");
        let script = ProviderWireScript::from_parts(
            PROVIDER_WIRE_SCRIPT_SCHEMA.to_string(),
            "invalid-shape".to_string(),
            "openai-compatible".to_string(),
            ProviderWireEndpoint {
                method: "POST".to_string(),
                path: "/chat/completions".to_string(),
            },
            ProviderWireRequestMatch::default(),
            timeline,
        );

        let scripts_error = ScriptedLlmHttpTransport::from_scripts([script])
            .expect_err("from_scripts should reject the illegal shape");
        assert_eq!(scripts_error.kind, ProviderFailureKind::Validation);
        assert_eq!(scripts_error.message, json_error.message);
    }

    #[tokio::test]
    async fn consecutive_body_events_are_streamed_as_individual_chunks() {
        let script = ProviderWireScript::from_json_str(&wire_script_json(json!([
            { "event": "response_start", "status": 200 },
            { "event": "body", "data": "first" },
            { "event": "body", "data": "second" },
            { "event": "chunk", "payload": { "data": "tail" } },
            { "event": "end" }
        ])))
        .expect("streaming body script");
        let response = execute_script(&script).expect("streaming response");
        let LlmHttpBody::Streamed(mut stream) = response.body else {
            panic!("body events followed by a chunk must produce a stream");
        };

        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.expect("stream chunk") {
            chunks.push(chunk);
        }

        assert_eq!(
            chunks,
            vec![
                Bytes::from("first"),
                Bytes::from("second"),
                Bytes::from("tail")
            ]
        );
        assert_eq!(chunks.len(), 3);
    }

    fn scripted_provider(script: &str) -> OpenAiCompatibleProvider {
        let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(script).unwrap());
        OpenAiCompatibleProvider::new("test-key", "https://provider.test").with_transport(transport)
    }

    fn event_collector() -> (Arc<Mutex<Vec<LlmStreamEvent>>>, LlmEventSender) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sender = LlmEventSender::new(move |event| {
            captured.lock_recover().push(event);
        });
        (events, sender)
    }

    fn text_deltas(events: &Arc<Mutex<Vec<LlmStreamEvent>>>) -> Vec<String> {
        events
            .lock_recover()
            .iter()
            .filter_map(|event| match event {
                LlmStreamEvent::Delta(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn request_body_json(err: &LlmTransportError) -> serde_json::Value {
        serde_json::from_str(err.request_body.as_deref().expect("request body snapshot"))
            .expect("request body JSON")
    }

    fn wire_script_json(timeline: Value) -> String {
        json!({
            "schema": PROVIDER_WIRE_SCRIPT_SCHEMA,
            "name": "invalid-shape",
            "provider_kind": "openai-compatible",
            "endpoint": { "method": "POST", "path": "/chat/completions" },
            "request_match": { "any": true },
            "timeline": timeline
        })
        .to_string()
    }

    fn request(stream_events: Option<LlmEventSender>) -> LlmRequest {
        LlmRequest {
            model: "openai/gpt-5.4".to_string(),
            messages: vec![LlmMessage::text(LlmRole::User, "lookup x")],
            attachments: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(vec![LlmToolSpec {
                name: "lookup".to_string(),
                description: "Lookup".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "q": { "type": "string" }
                    }
                })
                .into(),
                output_schema: json!({}).into(),
            }]),
            tool_choice: LlmToolChoice::Auto,
            model_variant: Default::default(),
            model_capability: lash_core::ModelCapability::default(),
            generation: lash_core::GenerationOptions::default(),
            scope: lash_core::LlmRequestScope::new(
                "session-1",
                "session-1:frame:sim",
                "session-1:request:sim",
            ),
            output_spec: None,
            stream_events,
            provider_trace: None,
        }
    }

    fn responses_request() -> LlmRequest {
        LlmRequest {
            model: "gpt-5.4".to_string(),
            messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
            attachments: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::Auto,
            model_variant: Default::default(),
            model_capability: lash_core::ModelCapability::default(),
            generation: lash_core::GenerationOptions::default(),
            scope: lash_core::LlmRequestScope::new(
                "session-1",
                "session-1:frame:sim",
                "session-1:request:sim",
            ),
            output_spec: None,
            stream_events: Some(LlmEventSender::new(|_event| {})),
            provider_trace: None,
        }
    }
}
