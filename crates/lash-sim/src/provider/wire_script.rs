use super::transport::{disconnect_error, header_vec, timeout_error, transport_error};
use super::*;

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

    pub(super) fn plan(&self) -> Result<&ScriptedResponsePlan, LlmTransportError> {
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
    pub(super) event_index: usize,
    pub(super) bytes: Option<Bytes>,
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
    pub(super) fn event_indices(&self) -> Vec<usize> {
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
    pub(super) fn event_index(&self) -> usize {
        match self {
            Self::Chunk { event_index, .. }
            | Self::End { event_index }
            | Self::Failure { event_index, .. } => *event_index,
        }
    }

    pub(super) fn into_chunk(self) -> Result<Option<Bytes>, LlmTransportError> {
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
