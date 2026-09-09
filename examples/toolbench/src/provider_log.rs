//! Transport evidence captured before Lash normalizes failures.
use lash::provider::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct Capture {
    pub(crate) entries: Arc<Mutex<Vec<Value>>>,
    secret: Arc<Mutex<String>>,
    span: Arc<Mutex<Option<tracing::Span>>>,
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
        self.capture.entries.lock().unwrap_or_else(|e| e.into_inner()).push(json!({"request_id":request_id, "request_ms":null, "cost":null, "response":null, "error":null}));
        let started = std::time::Instant::now();
        let result = self.inner.complete(request).await;
        let (error, response) = match &result {
            Ok(response) => (Value::Null, Some(response)),
            Err(error) => (
                error_object(error, &self.secret),
                error.partial_response.as_deref(),
            ),
        };
        let row = json!({"request_id":request_id, "request_ms":started.elapsed().as_millis(), "error":error,
            "cost":response.and_then(|r| r.provider_usage.as_ref()).and_then(|u| u.get("cost")).filter(|v| v.is_number()),
            "response":response.map(|r| redact(serde_json::to_value(r).expect("response serializes"), &self.secret))});
        tracing::debug!(target: "toolbench", parent: &self.capture.span(), attempt_index, evidence = %row, "provider response");
        self.capture
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())[attempt_index - 1] = row;
        result
    }
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
    let request_body = error.request_body.as_deref().map(|s| {
        let text = redact(serde_json::from_str(s).unwrap_or_else(|_| json!(s)), secret);
        let text = text
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| text.to_string());
        bounded_bytes(&text, 4096).to_owned()
    });
    redact(
        json!({
            "kind":error.kind, "status":error.status, "message":error.message,
            "body_excerpt":raw.map(|s| s.chars().take(2000).collect::<String>()), "raw":raw,
            "provider_request_id":header("x-request-id").or_else(|| header("request-id")),
            "provider_response_id":error.partial_response.as_ref().and_then(|r| r.execution_evidence.as_ref()).and_then(|e| e.provider_response_id.as_ref()),
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

fn bounded_bytes(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
                            json!(bounded_bytes(&text, 4096))
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
