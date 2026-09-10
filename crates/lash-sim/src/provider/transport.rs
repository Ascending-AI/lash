use super::*;

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

pub(super) fn execute_script(
    script: &ProviderWireScript,
) -> Result<LlmHttpResponse, LlmTransportError> {
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

pub(super) fn header_vec(headers: Vec<ProviderWireHeader>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|header| (header.name, header.value))
        .collect()
}

pub(super) fn failing_timeline_event_index(value: &Value) -> Option<usize> {
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

pub(super) fn disconnect_error(
    message: Option<String>,
    retryable: Option<bool>,
) -> LlmTransportError {
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

pub(super) fn timeout_error(message: Option<String>) -> LlmTransportError {
    LlmTransportError::new(message.unwrap_or_else(|| "scripted provider timeout".to_string()))
        .with_kind(ProviderFailureKind::Timeout)
        .with_code("timeout")
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
}

pub(super) fn transport_error(message: String, retryable: Option<bool>) -> LlmTransportError {
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

pub(super) fn script_validation_error(message: String) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Validation)
        .with_code("provider_wire_script")
}

fn script_match_error(message: String) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Validation)
        .with_code("provider_wire_script_mismatch")
}
