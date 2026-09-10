//! Transport evidence captured before Lash normalizes failures.
use lash::provider::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct Capture {
    pub(crate) http_bodies: Arc<Mutex<std::collections::BTreeMap<usize, Vec<u8>>>>,
    pub(crate) http_finished: Arc<Mutex<std::collections::BTreeSet<usize>>>,
    pub(crate) entries: Arc<Mutex<Vec<Value>>>,
    secret: Arc<Mutex<String>>,
    span: Arc<Mutex<Option<tracing::Span>>>,
    dump_prefix: Arc<Mutex<Option<std::path::PathBuf>>>,
    #[cfg(test)]
    pub(crate) dump_writes: Arc<Mutex<Vec<(usize, String)>>>,
    pub(crate) dump_errors: Arc<Mutex<Vec<String>>>,
}
impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}

pub(crate) fn retry_policy(retries: u32) -> ProviderRetryPolicy {
    ProviderRetryPolicy {
        enabled: retries > 0,
        max_attempts: retries.saturating_add(1),
        base_delay_ms: 1_000,
        max_delay_ms: 10_000,
        // Courtesy throttle waits otherwise allow eight EXTRA provider calls.
        throttle_wait_budget_ms: 0,
        ..ProviderRetryPolicy::default()
    }
}

impl Capture {
    pub(crate) fn wrap(&self, components: ProviderComponents, secret: &str) -> ProviderComponents {
        *self.span.lock().unwrap_or_else(|e| e.into_inner()) = Some(tracing::Span::current());
        *self.secret.lock().unwrap_or_else(|e| e.into_inner()) = secret.to_owned();
        let capture = self.clone();
        let secret = secret.to_owned();
        components.map_provider(|inner| {
            Box::new(LoggedProvider {
                inner,
                capture,
                secret,
            })
        })
    }
    pub(crate) fn span(&self) -> tracing::Span {
        self.span
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(tracing::Span::none)
    }
    pub(crate) fn redact(&self, value: Value) -> Value {
        redact(
            value,
            &self.secret.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }
    pub(crate) fn rows(&self) -> Vec<Value> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

struct LoggedProvider {
    inner: Box<dyn Provider>,
    capture: Capture,
    secret: String,
}
impl std::fmt::Debug for LoggedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoggedProvider")
    }
}

#[async_trait::async_trait]
impl Provider for LoggedProvider {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn route_identity(&self, model: &str) -> lash::direct::ProviderRouteIdentity {
        self.inner.route_identity(model)
    }
    fn options(&self) -> ProviderOptions {
        self.inner.options()
    }
    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.set_options(options);
    }
    fn serialize_config(&self) -> Value {
        self.inner.serialize_config()
    }
    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }
    fn generation_retry_guarantee(&self, request: &LlmRequest) -> GenerationRetryGuarantee {
        self.inner.generation_retry_guarantee(request)
    }
    async fn close(&self) -> Result<(), LlmTransportError> {
        self.inner.close().await
    }
    async fn reconcile_usage(
        &mut self,
        generation_id: &str,
    ) -> Result<Option<ReconciledUsage>, LlmTransportError> {
        self.inner.reconcile_usage(generation_id).await
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(Self {
            inner: self.inner.clone_boxed(),
            capture: self.capture.clone(),
            secret: self.secret.clone(),
        })
    }
    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        let request_id = request.scope.request_id.clone();
        let attempt_index = self.capture.rows().len() + 1;
        tracing::debug!(target: "toolbench", parent: &self.capture.span(), attempt_index, request_id, request = %redact(serde_json::to_value(&request).expect("request serializes"), &self.secret), "provider request");
        self.capture.entries.lock().unwrap_or_else(|e| e.into_inner()).push(json!({"request_id":request_id, "partial":true, "request_ms":null, "cost":null, "response":null, "error":null}));
        let started = std::time::Instant::now();
        let result = self.inner.complete(request).await;
        let (error, response) = match &result {
            Ok(response) => (Value::Null, Some(response)),
            Err(error) => (
                error_object(error, &self.secret),
                error.partial_response.as_deref(),
            ),
        };
        let mut row = json!({"partial":false,"request_id":request_id, "request_ms":started.elapsed().as_millis(), "error":error,
            "cost":reported_cost(response, result.as_ref().err()),
            "response":response.map(|r| redact(serde_json::to_value(r).expect("response serializes"), &self.secret))});
        let mut entries = self
            .capture
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for key in ["wire_request", "wire_responses", "request_sizes"] {
            row[key] = entries[attempt_index - 1][key].clone();
        }
        let http_body = self
            .capture
            .http_bodies
            .lock()
            .unwrap()
            .get(&attempt_index)
            .cloned();
        let http_json = http_body
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Value>(b).ok());
        row["http_response_json"] = http_json.clone().unwrap_or(Value::Null);
        let raw_usage = response
            .and_then(|r| r.provider_usage.clone())
            .or_else(|| {
                result
                    .as_ref()
                    .err()?
                    .raw
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|v| v.get("usage").cloned())
            })
            .or_else(|| {
                row["wire_responses"]
                    .as_array()?
                    .iter()
                    .rev()
                    .find_map(|v| v.get("usage").filter(|u| !u.is_null()).cloned())
            });
        if row["wire_request"].is_null() {
            let body = response
                .and_then(|r| r.request_body.as_deref())
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            if let Some(body) = body {
                row["wire_request"] = self.capture.redact(body);
                row["request_sizes"] = crate::accounting::request_sizes(&row["wire_request"]);
                self.capture
                    .dump(attempt_index, "request", &row["wire_request"]);
            }
        }
        row["raw_usage"] = raw_usage
            .or_else(|| http_json.as_ref()?.get("usage").cloned())
            .unwrap_or(Value::Null);
        row["usage"] = serde_json::to_value(crate::accounting::Usage::from_raw(&row["raw_usage"]))
            .expect("usage serializes");
        entries[attempt_index - 1] = row.clone();
        drop(entries);
        self.capture.dump(attempt_index, "response", &row);
        tracing::debug!(target: "toolbench", parent: &self.capture.span(), attempt_index, evidence = %row, "provider response");
        self.capture
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())[attempt_index - 1] = row;
        result
    }
}

fn reported_cost(
    response: Option<&LlmResponse>,
    error: Option<&LlmTransportError>,
) -> Option<Value> {
    response
        .and_then(|r| r.provider_usage.as_ref())
        .and_then(|u| u.get("cost"))
        .filter(|v| v.is_number())
        .cloned()
        .or_else(|| {
            // OpenAI empty_response failures can expose a final usage chunk in
            // raw while omitting partial_response entirely. It is still money
            // spent, with the same OpenRouter usage.cost contract as success.
            let raw = error?.raw.as_deref()?;
            serde_json::from_str::<Value>(raw)
                .ok()?
                .pointer("/usage/cost")
                .filter(|v| v.is_number())
                .cloned()
        })
}

pub(crate) fn error_object(error: &LlmTransportError, secret: &str) -> Value {
    let classified = DefaultProviderFailureClassifier.classify(error.clone());
    let header = |name: &str| {
        error
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    };
    let raw = error.raw.as_deref().map(|s| s.as_str());
    let raw_json = raw.and_then(|s| serde_json::from_str::<Value>(s).ok());
    let response_id = error
        .partial_response
        .as_ref()
        .and_then(|r| r.execution_evidence.as_ref())
        .and_then(|e| e.provider_response_id.clone())
        .or_else(|| {
            raw_json
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let request_body = error.request_body.as_deref().map(|s| {
        let text = redact(serde_json::from_str(s).unwrap_or_else(|_| json!(s)), secret);
        text.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| text.to_string())
    });
    redact(
        json!({
            "kind":error.kind, "status":error.status, "message":error.message,
            "body_excerpt":raw.map(|s| s.chars().take(2000).collect::<String>()), "raw":raw,
            "provider_request_id":header("x-request-id").or_else(|| header("request-id")),
            "provider_response_id":response_id,
            "retry_after":classified.retry_after(), "code":error.code, "terminal_reason":error.terminal_reason,
            "headers":error.headers.iter().map(|(k,v)| (k, if sensitive(k) { "[REDACTED]" } else { v })).collect::<Vec<_>>(),
            "request_body":request_body, "output_started":error.output_started,
            "partial_response":error.partial_response, "context":format!("{:?}",error.context),
            "adapter_retry_verdict":format!("{:?}",error.retry_verdict),
            "classification":{"kind":classified.kind,"retry_verdict":format!("{:?}",classified.retry_verdict),"terminal_reason":classified.terminal_reason}
        }),
        secret,
    )
}

fn sensitive(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "api_key"
            | "api-key"
            | "x-api-key"
            | "access_token"
            | "cookie"
            | "set-cookie"
    )
}
pub(crate) fn redact(value: Value, secret: &str) -> Value {
    match value {
        Value::Object(values) => values
            .into_iter()
            .map(|(k, v)| {
                let v = if sensitive(&k) {
                    json!("[REDACTED]")
                } else if k == "body_excerpt" {
                    redact(v, secret)
                        .as_str()
                        .map(|s| json!(s.chars().take(2000).collect::<String>()))
                        .unwrap_or(Value::Null)
                } else if k == "request_body" {
                    match v {
                        Value::String(s) => {
                            let safe = redact(
                                serde_json::from_str(&s).unwrap_or_else(|_| json!(s)),
                                secret,
                            );
                            let text = safe
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| safe.to_string());
                            json!(text)
                        }
                        other => redact(other, secret),
                    }
                } else {
                    redact(v, secret)
                };
                (k, v)
            })
            .collect(),
        Value::Array(values) => values.into_iter().map(|v| redact(v, secret)).collect(),
        Value::String(s) if !secret.is_empty() => Value::String(s.replace(secret, "[REDACTED]")),
        other => other,
    }
}

pub(crate) fn trace_subscriber(
    path: &std::path::Path,
) -> anyhow::Result<impl tracing::Subscriber + Send + Sync + use<>> {
    let file = std::fs::File::create(path)?;
    Ok(tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "lash=debug,lash_core=debug,lash_provider_openai=debug,toolbench=debug".into()
            }),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .finish())
}

#[cfg(test)]
#[path = "provider_log_tests.rs"]
mod tests;

impl Capture {
    pub(crate) fn set_dump_prefix(&self, prefix: Option<std::path::PathBuf>) {
        *self.dump_prefix.lock().unwrap() = prefix;
    }
    pub(crate) fn dump(&self, attempt: usize, direction: &str, body: &Value) {
        let prefix = self.dump_prefix.lock().unwrap().clone();
        if let Some(prefix) = prefix {
            let path = prefix.with_extension(format!("turn-1-round-{attempt}-{direction}.json"));
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(path.parent().expect("dump parent"))?;
                std::fs::write(
                    &path,
                    serde_json::to_vec_pretty(&self.redact(body.clone()))?,
                )
            })();
            #[cfg(test)]
            if result.is_ok() {
                self.dump_writes
                    .lock()
                    .unwrap()
                    .push((attempt, direction.into()));
            }
            if let Err(error) = result {
                self.dump_errors
                    .lock()
                    .unwrap()
                    .push(format!("{}: {error}", path.display()));
            }
        }
    }
}
impl lash::tracing::TraceSink for Capture {
    fn append(
        &self,
        record: &lash::tracing::TraceRecord,
    ) -> Result<(), lash::tracing::TraceSinkError> {
        use lash::tracing::TraceEvent;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let attempt = entries.len();
        let Some(row) = entries.last_mut() else {
            return Ok(());
        };
        match &record.event {
            TraceEvent::ProviderRequest { event } => {
                let body = self.redact(event.body_json.clone().unwrap_or(Value::Null));
                if body.is_null() {
                    return Ok(());
                }
                row["wire_request"] = body.clone();
                row["request_sizes"] = if body.is_null() {
                    Value::Null
                } else {
                    crate::accounting::request_sizes(&body)
                };
                tracing::debug!(target: "toolbench", parent: &self.span(), attempt, request = %body, sizes = %row["request_sizes"], "wire request");
                self.dump(attempt, "request", &body);
            }
            TraceEvent::ProviderStreamEvent { event } => {
                let body = self.redact(event.raw_json.clone().unwrap_or(Value::Null));
                if !row["wire_responses"].is_array() {
                    row["wire_responses"] = json!([]);
                }
                row["wire_responses"]
                    .as_array_mut()
                    .unwrap()
                    .push(body.clone());
                tracing::debug!(target: "toolbench", parent: &self.span(), attempt, response = %body, "wire response chunk");
                // The provider completion or recorder teardown writes this row once.
            }
            _ => {}
        }
        Ok(())
    }
}
