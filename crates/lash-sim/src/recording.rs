use lash_sansio::sync::MutexExt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use chrono::{SecondsFormat, Utc};
use lash_core::{ProviderFailureKind, facade_support::LlmTransportError};
use lash_llm_transport::{
    LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport,
};
use serde_json::Value;

use crate::provider::{
    PROVIDER_WIRE_SCRIPT_SCHEMA, ProviderWireEndpoint, ProviderWireEvent, ProviderWireHeader,
    ProviderWireProvenance, ProviderWireProvenanceKind, ProviderWireRequestMatch,
    ProviderWireScript,
};

const REDACTED: &str = "[redacted]";

/// Configuration for recording provider HTTP exchanges as Provider Wire Scripts.
///
/// The recorder never persists the request body. Callers provide only stable,
/// non-sensitive matchers that are useful during replay. Response headers require
/// a caller-owned allow-list; none are retained unless explicitly selected.
/// Known credentials and user markers are redacted, including provenance notes.
/// A final pattern scan refuses possible secrets before publication. Pattern
/// scanning is a guard, not proof of absence: arbitrary secrets and encoded
/// binary content may not match a recognizable pattern.
#[derive(Clone, Debug)]
pub struct ProviderRecordingConfig {
    output_dir: PathBuf,
    name_prefix: String,
    provider_kind: String,
    request_match: ProviderWireRequestMatch,
    response_header_allow_list: Vec<String>,
    user_content_markers: Vec<String>,
    notes: Option<String>,
}

impl ProviderRecordingConfig {
    pub fn new(
        output_dir: impl Into<PathBuf>,
        name_prefix: impl Into<String>,
        provider_kind: impl Into<String>,
    ) -> Self {
        Self {
            output_dir: output_dir.into(),
            name_prefix: name_prefix.into(),
            provider_kind: provider_kind.into(),
            request_match: ProviderWireRequestMatch::default(),
            response_header_allow_list: Vec::new(),
            user_content_markers: Vec::new(),
            notes: None,
        }
    }

    pub fn with_request_match(mut self, request_match: ProviderWireRequestMatch) -> Self {
        self.request_match = request_match;
        self
    }

    pub fn with_user_content_markers(
        mut self,
        markers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.user_content_markers = markers.into_iter().map(Into::into).collect();
        self
    }

    /// Select response header names to persist (case-insensitive). Unselected
    /// credential headers refuse the recording; other unselected headers are
    /// omitted. Selected credential values are always redacted.
    pub fn with_response_header_allow_list(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.response_header_allow_list = names.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }
}

/// HTTP transport decorator that records each real exchange into the existing
/// Provider Wire Script v1 replay format.
#[derive(Clone, Debug)]
pub struct RecordingLlmHttpTransport {
    inner: Arc<dyn LlmHttpTransport>,
    config: ProviderRecordingConfig,
    next_exchange: Arc<AtomicUsize>,
    recorded_paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl RecordingLlmHttpTransport {
    pub fn new(inner: Arc<dyn LlmHttpTransport>, config: ProviderRecordingConfig) -> Self {
        Self {
            inner,
            config,
            next_exchange: Arc::new(AtomicUsize::new(1)),
            recorded_paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn recorded_paths(&self) -> Result<Vec<PathBuf>, LlmTransportError> {
        Ok(self.recorded_paths.lock_recover().clone())
    }
}

#[async_trait]
impl LlmHttpTransport for RecordingLlmHttpTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        timeout: Option<Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let exchange_number = self.next_exchange.fetch_add(1, Ordering::SeqCst);
        let started_at = Instant::now();
        let capture = RecordingExchange::new(
            self.config.clone(),
            exchange_number,
            &request,
            self.recorded_paths.clone(),
        )?;

        match self.inner.send(request, timeout).await {
            Ok(response) => capture.wrap_response(response, started_at),
            Err(error) => {
                capture.write_transport_error(&error, started_at.elapsed())?;
                Err(error)
            }
        }
    }
}

#[derive(Debug)]
struct RecordingExchange {
    config: ProviderRecordingConfig,
    exchange_number: usize,
    endpoint: ProviderWireEndpoint,
    request_match: ProviderWireRequestMatch,
    scrubber: CaptureScrubber,
    recorded_paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl RecordingExchange {
    fn new(
        config: ProviderRecordingConfig,
        exchange_number: usize,
        request: &LlmHttpRequest,
        recorded_paths: Arc<Mutex<Vec<PathBuf>>>,
    ) -> Result<Self, LlmTransportError> {
        validate_name_prefix(&config.name_prefix)?;
        let scrubber = CaptureScrubber::new(&request.headers, &config.user_content_markers);
        let request_match = scrubber.redact_request_match(&config.request_match)?;
        Ok(Self {
            endpoint: ProviderWireEndpoint {
                method: request.method.as_str().to_string(),
                path: request_path(&request.url),
            },
            config,
            exchange_number,
            request_match,
            scrubber,
            recorded_paths,
        })
    }

    fn wrap_response(
        self,
        response: LlmHttpResponse,
        request_started_at: Instant,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        let response_started_after = request_started_at.elapsed();
        let LlmHttpResponse {
            status,
            headers,
            body,
        } = response;
        match body {
            LlmHttpBody::Buffered(bytes) => {
                self.write_response(
                    status,
                    &headers,
                    &bytes,
                    response_started_after,
                    response_started_after,
                    None,
                )?;
                Ok(LlmHttpResponse {
                    status,
                    headers,
                    body: LlmHttpBody::Buffered(bytes),
                })
            }
            LlmHttpBody::Streamed(stream) => Ok(LlmHttpResponse {
                status,
                headers: headers.clone(),
                body: LlmHttpBody::streamed(RecordingByteStream {
                    inner: stream,
                    exchange: Some(self),
                    status,
                    headers,
                    request_started_at,
                    response_started_after,
                    body: BytesMut::new(),
                }),
            }),
        }
    }

    fn write_response(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
        response_started_after: Duration,
        response_finished_after: Duration,
        stream_error: Option<&LlmTransportError>,
    ) -> Result<(), LlmTransportError> {
        let response_started_at = elapsed_millis(response_started_after);
        let response_finished_at = elapsed_millis(response_finished_after);
        let headers = self
            .scrubber
            .redact_headers(headers, &self.config.response_header_allow_list)?;
        let body = self.scrubber.redact_body(body)?;
        let timeline = if !(200..300).contains(&status) {
            vec![ProviderWireEvent::HttpError {
                at: response_finished_at,
                status,
                headers,
                body,
            }]
        } else {
            let mut timeline = vec![ProviderWireEvent::ResponseStart {
                at: response_started_at,
                status,
                headers,
            }];
            if !body.is_empty() {
                timeline.push(ProviderWireEvent::Chunk {
                    at: response_finished_at,
                    payload: crate::provider::ProviderWireChunkPayload::Data(body),
                });
            }
            timeline.push(match stream_error {
                Some(error) => recorded_stream_error(error, response_finished_at, &self.scrubber),
                None => ProviderWireEvent::End {
                    at: response_finished_at,
                },
            });
            timeline
        };
        self.write_script(timeline)
    }

    fn write_transport_error(
        &self,
        error: &LlmTransportError,
        elapsed: Duration,
    ) -> Result<(), LlmTransportError> {
        let at = elapsed_millis(elapsed);
        let event = match error.kind {
            ProviderFailureKind::Timeout => ProviderWireEvent::Timeout {
                at,
                message: Some(self.scrubber.redact_text(&error.message)),
            },
            _ => ProviderWireEvent::TransportError {
                at,
                message: self.scrubber.redact_text(&error.message),
                retryable: Some(error.is_retryable()),
            },
        };
        self.write_script(vec![event])
    }

    fn write_script(&self, timeline: Vec<ProviderWireEvent>) -> Result<(), LlmTransportError> {
        fs::create_dir_all(&self.config.output_dir)
            .map_err(|_| recording_error("could not create provider recording directory"))?;
        let name = format!("{}.{:03}", self.config.name_prefix, self.exchange_number);
        let path = self.config.output_dir.join(format!("{name}.json"));
        if path.exists() {
            return Err(recording_error("provider recording already exists"));
        }
        let mut script = ProviderWireScript::from_parts(
            PROVIDER_WIRE_SCRIPT_SCHEMA.to_string(),
            name,
            self.config.provider_kind.clone(),
            self.endpoint.clone(),
            self.request_match.clone(),
            timeline,
        );
        script.provenance = Some(ProviderWireProvenance {
            kind: ProviderWireProvenanceKind::CapturedLive,
            source: self.endpoint.path.clone(),
            captured_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
            notes: self
                .config
                .notes
                .as_deref()
                .map(|notes| self.scrubber.redact_text(notes)),
        });
        let mut encoded = serde_json::to_vec_pretty(&script).map_err(|error| {
            recording_error(format!("could not serialize provider recording: {error}"))
        })?;
        self.scrubber.check_serialized_artifact(&encoded)?;
        script.validate()?;
        encoded.push(b'\n');
        write_new_file(&path, &encoded)?;
        self.recorded_paths.lock_recover().push(path);
        Ok(())
    }
}

#[derive(Debug)]
struct RecordingByteStream {
    inner: Box<dyn LlmByteStream>,
    exchange: Option<RecordingExchange>,
    status: u16,
    headers: Vec<(String, String)>,
    request_started_at: Instant,
    response_started_after: Duration,
    body: BytesMut,
}

#[async_trait]
impl LlmByteStream for RecordingByteStream {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
        match self.inner.next_chunk().await {
            Ok(Some(chunk)) => {
                self.body.extend_from_slice(&chunk);
                Ok(Some(chunk))
            }
            Ok(None) => {
                self.finish(None)?;
                Ok(None)
            }
            Err(error) => {
                self.finish(Some(&error))?;
                Err(error)
            }
        }
    }
}

impl RecordingByteStream {
    fn finish(&mut self, error: Option<&LlmTransportError>) -> Result<(), LlmTransportError> {
        let Some(exchange) = self.exchange.take() else {
            return Ok(());
        };
        exchange.write_response(
            self.status,
            &self.headers,
            &self.body,
            self.response_started_after,
            self.request_started_at.elapsed(),
            error,
        )
    }
}

#[derive(Clone)]
struct CaptureScrubber {
    literals: Vec<String>,
}

impl std::fmt::Debug for CaptureScrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureScrubber")
            .field("literal_count", &self.literals.len())
            .finish()
    }
}

impl CaptureScrubber {
    fn new(
        request_headers: &[(String, lash_llm_transport::HttpHeaderValue)],
        user_content_markers: &[String],
    ) -> Self {
        let mut literals = user_content_markers
            .iter()
            .filter(|marker| !marker.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        for (_, value) in request_headers {
            if value.is_sensitive() && !value.as_str().is_empty() {
                literals.push(value.as_str().to_string());
                if let Some((scheme, credential)) = value.as_str().split_once(' ')
                    && scheme.eq_ignore_ascii_case("bearer")
                    && !credential.is_empty()
                {
                    literals.push(credential.to_string());
                }
            }
        }
        literals.sort_by_key(|literal| std::cmp::Reverse(literal.len()));
        literals.dedup();
        Self { literals }
    }

    fn redact_headers(
        &self,
        headers: &[(String, String)],
        allow_list: &[String],
    ) -> Result<Vec<ProviderWireHeader>, LlmTransportError> {
        let mut retained = Vec::new();
        for (name, value) in headers {
            let selected = allow_list
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name));
            let sensitive = sensitive_json_key(name);
            if !selected {
                if sensitive {
                    return Err(recording_error(
                        "refusing provider recording: credential response header outside caller allow-list",
                    ));
                }
                continue;
            }
            retained.push(ProviderWireHeader {
                name: name.clone(),
                value: if sensitive {
                    REDACTED.to_string()
                } else {
                    self.redact_text(value)
                },
            });
        }
        Ok(retained)
    }

    fn check_serialized_artifact(&self, encoded: &[u8]) -> Result<(), LlmTransportError> {
        // Inspect the exact artifact, decoding JSON escapes in both keys and
        // values. Diagnostics contain only a static category, never a value or
        // an artifact path (a key or filename can itself contain a secret).
        let artifact: Value = serde_json::from_slice(encoded)
            .map_err(|_| recording_error("could not inspect serialized provider recording"))?;
        self.check_artifact_value(&artifact)
    }

    fn check_artifact_value(&self, value: &Value) -> Result<(), LlmTransportError> {
        match value {
            Value::String(text) => self.check_artifact_text(text),
            Value::Array(items) => items
                .iter()
                .try_for_each(|item| self.check_artifact_value(item)),
            Value::Object(fields) => fields.iter().try_for_each(|(key, value)| {
                self.check_artifact_text(key)?;
                self.check_artifact_value(value)
            }),
            _ => Ok(()),
        }
    }

    fn check_artifact_text(&self, text: &str) -> Result<(), LlmTransportError> {
        let finding = if self.literals.iter().any(|literal| text.contains(literal)) {
            Some("known sensitive marker")
        } else {
            secret_pattern(text)
        };
        match finding {
            Some(category) => Err(recording_error(format!(
                "refusing provider recording: possible secret ({category})"
            ))),
            None => Ok(()),
        }
    }

    fn redact_body(&self, body: &[u8]) -> Result<String, LlmTransportError> {
        let body = std::str::from_utf8(body).map_err(|_| {
            recording_error(
                "provider response was not UTF-8; refusing to persist an uninspectable body",
            )
        })?;
        let redacted = self.redact_text(body);
        Ok(redact_json_or_sse(&redacted))
    }

    fn redact_text(&self, input: &str) -> String {
        self.literals
            .iter()
            .fold(input.to_owned(), |text, literal| {
                text.replace(literal, REDACTED)
            })
    }

    fn redact_request_match(
        &self,
        request_match: &ProviderWireRequestMatch,
    ) -> Result<ProviderWireRequestMatch, LlmTransportError> {
        let value = serde_json::to_value(request_match).map_err(|error| {
            recording_error(format!(
                "could not inspect provider request matchers: {error}"
            ))
        })?;
        let redacted = redact_json_value(value, self);
        serde_json::from_value(redacted).map_err(|error| {
            recording_error(format!(
                "could not redact provider request matchers: {error}"
            ))
        })
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn redact_json_or_sse(input: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        return serde_json::to_string(&redact_sensitive_json_fields(value))
            .expect("serializing a JSON value is infallible");
    }

    input
        .split_inclusive('\n')
        .map(|line| {
            let Some(data) = line.strip_prefix("data: ") else {
                return line.to_string();
            };
            let (payload, newline) = data
                .strip_suffix('\n')
                .map_or((data, ""), |payload| (payload, "\n"));
            serde_json::from_str::<Value>(payload).map_or_else(
                |_| line.to_string(),
                |value| {
                    format!(
                        "data: {}{newline}",
                        serde_json::to_string(&redact_sensitive_json_fields(value))
                            .expect("serializing a JSON value is infallible")
                    )
                },
            )
        })
        .collect()
}

fn redact_json_value(value: Value, scrubber: &CaptureScrubber) -> Value {
    match value {
        Value::String(text) => Value::String(scrubber.redact_text(&text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json_value(item, scrubber))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, redact_json_value(value, scrubber)))
                .collect(),
        ),
        other => other,
    }
}

fn redact_sensitive_json_fields(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(redact_sensitive_json_fields)
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let value = if sensitive_json_key(&key) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_sensitive_json_fields(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        other => other,
    }
}

fn sensitive_json_key(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "authorization"
            | "proxy_authorization"
            | "api_key"
            | "apikey"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "client_secret"
            | "password"
            | "cookie"
            | "set_cookie"
    ) || normalized.ends_with("_api_key")
        || normalized.ends_with("_access_token")
        || normalized.ends_with("_refresh_token")
}

fn recorded_stream_error(
    error: &LlmTransportError,
    at: u64,
    scrubber: &CaptureScrubber,
) -> ProviderWireEvent {
    match error.kind {
        ProviderFailureKind::Timeout => ProviderWireEvent::Timeout {
            at,
            message: Some(scrubber.redact_text(&error.message)),
        },
        ProviderFailureKind::Stream => ProviderWireEvent::Disconnect {
            at,
            message: Some(scrubber.redact_text(&error.message)),
            retryable: Some(error.is_retryable()),
        },
        _ => ProviderWireEvent::TransportError {
            at,
            message: scrubber.redact_text(&error.message),
            retryable: Some(error.is_retryable()),
        },
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), LlmTransportError> {
    write_new_file_before_publish(path, bytes, || Ok(()))
}

fn write_new_file_before_publish(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), LlmTransportError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::Builder::new()
        .prefix(".provider-recording-")
        .tempfile_in(parent)
        .map_err(|_| recording_error("could not create provider recording temporary file"))?;
    file.write_all(bytes)
        .map_err(|_| recording_error("could not write provider recording temporary file"))?;
    file.as_file()
        .sync_all()
        .map_err(|_| recording_error("could not sync provider recording temporary file"))?;
    before_publish().map_err(|_| recording_error("provider recording publication interrupted"))?;
    // A hard link atomically claims the final name without replacing an
    // existing recording. A crash before this point leaves only a randomly
    // named sibling, so the final path is free for a retry.
    fs::hard_link(file.path(), path).map_err(|_| {
        recording_error("could not publish provider recording without replacing an existing file")
    })?;
    file.close()
        .map_err(|_| recording_error("could not clean up provider recording temporary file"))
}

/// Conservative recognizable credential patterns. No environment-derived or
/// default header policy is consulted here.
fn secret_pattern(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----") {
        return Some("private key");
    }
    let mut bearer_tokens = text
        .split(|ch: char| {
            !ch.is_ascii_alphanumeric() && !matches!(ch, '.' | '_' | '~' | '+' | '/' | '=' | '-')
        })
        .filter(|token| !token.is_empty());
    while let Some(scheme) = bearer_tokens.next() {
        if scheme.eq_ignore_ascii_case("bearer")
            && bearer_tokens
                .next()
                .is_some_and(|credential| credential.len() >= 16)
        {
            return Some("bearer token");
        }
    }
    for token in text.split(|ch: char| !ch.is_ascii_alphanumeric() && !matches!(ch, '_' | '-')) {
        if token.starts_with("sk-") && token.len() >= 23 {
            return Some("API key");
        }
        if token.starts_with("AIza") && token.len() >= 24 {
            return Some("Google API key");
        }
        if (token.starts_with("AKIA") || token.starts_with("ASIA"))
            && token.len() == 20
            && token
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return Some("AWS access key");
        }
        if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
            .iter()
            .any(|prefix| token.starts_with(prefix))
            && token.len() >= 24
        {
            return Some("GitHub token");
        }
    }
    None
}

fn validate_name_prefix(name: &str) -> Result<(), LlmTransportError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(recording_error(
            "provider recording name must contain only ASCII letters, digits, `-`, or `_`",
        ));
    }
    Ok(())
}

fn request_path(url: &str) -> String {
    let without_origin = url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or(url);
    let path = without_origin.split('?').next().unwrap_or(without_origin);
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn recording_error(message: impl Into<String>) -> LlmTransportError {
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Transport)
        .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::NotRetryable)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::provider::{HeaderMatcher, JsonMatcher, ScriptedLlmHttpTransport};
    use lash_llm_transport::{LlmHttpMethod, read_http_body_text};

    const REQUEST_SECRET: &str = "sk-live-recording-secret";
    const HEADER_SECRET: &str = "response-cookie-secret";
    const USER_MARKER: &str = "private recording prompt";

    #[derive(Debug)]
    struct StaticTransport {
        status: u16,
        headers: Vec<(String, String)>,
        chunks: Mutex<VecDeque<Bytes>>,
    }

    #[async_trait]
    impl LlmHttpTransport for StaticTransport {
        async fn send(
            &self,
            _request: LlmHttpRequest,
            _timeout: Option<Duration>,
        ) -> Result<LlmHttpResponse, LlmTransportError> {
            let chunks = self.chunks.lock_recover().drain(..).collect();
            Ok(LlmHttpResponse {
                status: self.status,
                headers: self.headers.clone(),
                body: LlmHttpBody::streamed(StaticByteStream { chunks }),
            })
        }
    }

    #[derive(Debug)]
    struct StaticByteStream {
        chunks: VecDeque<Bytes>,
    }

    #[async_trait]
    impl LlmByteStream for StaticByteStream {
        async fn next_chunk(&mut self) -> Result<Option<Bytes>, LlmTransportError> {
            Ok(self.chunks.pop_front())
        }
    }

    #[tokio::test]
    async fn recorder_redacts_before_writing_and_produces_a_replayable_v1_script() {
        let output = tempfile::tempdir().expect("recording directory");
        let inner = Arc::new(StaticTransport {
            status: 429,
            headers: vec![
                ("set-cookie".to_string(), HEADER_SECRET.to_string()),
                ("x-request-id".to_string(), "req-safe".to_string()),
            ],
            chunks: Mutex::new(VecDeque::from([
                Bytes::from_static(b"{\"access_token\":\"response-token\",\"echo\":\"sk-live-"),
                Bytes::from_static(
                    b"recording-secret\",\"content\":\"private recording prompt\",\"safe\":\"kept\"}",
                ),
            ])),
        });
        let request_match = ProviderWireRequestMatch {
            any: false,
            body: [(
                "messages".to_string(),
                JsonMatcher {
                    contains: Some(USER_MARKER.to_string()),
                    ..JsonMatcher::default()
                },
            )]
            .into_iter()
            .collect(),
            headers: [(
                "authorization".to_string(),
                HeaderMatcher {
                    equals: Some(format!("Bearer {REQUEST_SECRET}")),
                    ..HeaderMatcher::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let recorder = RecordingLlmHttpTransport::new(
            inner,
            ProviderRecordingConfig::new(output.path(), "openai_rate_limit", "openai")
                .with_request_match(request_match)
                .with_response_header_allow_list(["set-cookie", "X-Request-ID"])
                .with_user_content_markers([USER_MARKER])
                .with_notes(format!(
                    "capture-time redaction: {REQUEST_SECRET}; {USER_MARKER}"
                )),
        );
        let request = LlmHttpRequest {
            method: LlmHttpMethod::Post,
            url: "https://api.example/v1/responses?api_key=query-secret".to_string(),
            headers: vec![
                (
                    "authorization".to_string(),
                    lash_llm_transport::HttpHeaderValue::sensitive(format!(
                        "Bearer {REQUEST_SECRET}"
                    )),
                ),
                ("content-type".to_string(), "application/json".into()),
                (
                    "x-private-context".to_string(),
                    lash_llm_transport::HttpHeaderValue::sensitive(HEADER_SECRET),
                ),
            ],
            body: Bytes::from(format!(
                r#"{{"messages":[{{"role":"user","content":"{USER_MARKER}"}}]}}"#
            )),
            body_for_error: None,
            response_start_timeout_message: None,
        };

        let response = recorder
            .send(request.clone(), None)
            .await
            .expect("response");
        let original = read_http_body_text(response.body, 16 * 1024 * 1024, None, "read response")
            .await
            .expect("original response body");
        assert!(original.contains(REQUEST_SECRET));
        assert!(original.contains(USER_MARKER));

        let paths = recorder.recorded_paths().expect("recorded paths");
        assert_eq!(paths.len(), 1);
        let recorded = fs::read_to_string(&paths[0]).expect("recorded script");
        for sensitive in [
            REQUEST_SECRET,
            HEADER_SECRET,
            USER_MARKER,
            "response-token",
            "query-secret",
        ] {
            assert!(
                !recorded.contains(sensitive),
                "capture persisted sensitive marker `{sensitive}`"
            );
        }
        assert!(recorded.contains("req-safe"));
        assert!(recorded.contains("kept"));
        assert!(recorded.contains(REDACTED));

        let script = ProviderWireScript::from_json_str(&recorded).expect("valid v1 script");
        assert_eq!(script.endpoint.path, "/v1/responses");
        assert!(matches!(
            script.provenance,
            Some(ProviderWireProvenance {
                kind: ProviderWireProvenanceKind::CapturedLive,
                ..
            })
        ));

        // Redacted content matchers cannot match the original request. Clearing
        // them demonstrates that the captured response itself is directly
        // consumable by the existing replay transport.
        let mut replay_script = script;
        replay_script.request_match = ProviderWireRequestMatch::default();
        let replay = ScriptedLlmHttpTransport::new(replay_script).expect("valid replay script");
        let replayed = replay.send(request, None).await.expect("replayed response");
        let replayed_body =
            read_http_body_text(replayed.body, 16 * 1024 * 1024, None, "read replay")
                .await
                .expect("replayed body");
        assert_eq!(replayed.status, 429);
        assert!(replayed_body.contains(REDACTED));
        assert!(replayed_body.contains("kept"));
    }

    #[tokio::test]
    async fn recorder_rejects_invalid_request_matcher_before_writing() {
        let output = tempfile::tempdir().expect("recording directory");
        let inner = Arc::new(StaticTransport {
            status: 200,
            headers: Vec::new(),
            chunks: Mutex::new(VecDeque::new()),
        });
        let request_match = ProviderWireRequestMatch {
            any: true,
            body: Default::default(),
            headers: [("x-request-id".to_string(), HeaderMatcher::default())]
                .into_iter()
                .collect(),
        };
        let recorder = RecordingLlmHttpTransport::new(
            inner,
            ProviderRecordingConfig::new(output.path(), "invalid_matcher", "openai")
                .with_request_match(request_match),
        );
        let response = recorder
            .send(
                LlmHttpRequest {
                    method: LlmHttpMethod::Post,
                    url: "https://api.example/v1/responses".to_string(),
                    headers: Vec::new(),
                    body: Bytes::new(),
                    body_for_error: None,
                    response_start_timeout_message: None,
                },
                None,
            )
            .await
            .expect("response before recording is finalized");

        let error = read_http_body_text(response.body, 16 * 1024 * 1024, None, "read response")
            .await
            .expect_err("invalid matcher must prevent recording");
        assert_eq!(error.kind, ProviderFailureKind::Validation);
        assert!(error.message.contains("request matcher cannot combine"));
        assert!(
            recorder
                .recorded_paths()
                .expect("recorded paths")
                .is_empty()
        );
    }

    #[test]
    fn recorder_refuses_authorization_outside_caller_header_allow_list() {
        let output = tempfile::tempdir().expect("recording directory");
        let exchange = recording_exchange(ProviderRecordingConfig::new(
            output.path(),
            "secret",
            "openai",
        ));
        let error = exchange
            .write_response(
                200,
                &[(
                    "Authorization".into(),
                    "Bearer unlisted-response-credential".into(),
                )],
                b"safe",
                Duration::ZERO,
                Duration::ZERO,
                None,
            )
            .expect_err("unlisted authorization must refuse publication");
        assert!(!error.message.contains("unlisted-response-credential"));
        assert!(!output.path().join("secret.001.json").exists());
        assert!(exchange.recorded_paths.lock_recover().is_empty());
    }

    #[test]
    fn interrupted_publication_leaves_final_absent_and_retry_succeeds() {
        let output = tempfile::tempdir().expect("recording directory");
        let path = output.path().join("recording.json");
        write_new_file_before_publish(&path, b"complete", || {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "injected interruption after write",
            ))
        })
        .expect_err("interrupted publication");
        assert!(
            !path.exists(),
            "a failed write must not claim the final name"
        );
        write_new_file(&path, b"retry").expect("retry publication");
        assert_eq!(fs::read(&path).expect("published bytes"), b"retry");
        write_new_file(&path, b"replacement").expect_err("existing recording is never overwritten");
        assert_eq!(fs::read(&path).expect("original bytes"), b"retry");
        assert_eq!(fs::read_dir(output.path()).expect("directory").count(), 1);
    }

    fn recording_exchange(config: ProviderRecordingConfig) -> RecordingExchange {
        RecordingExchange::new(
            config,
            1,
            &LlmHttpRequest {
                method: LlmHttpMethod::Post,
                url: "https://api.example/v1/responses".into(),
                headers: vec![(
                    "authorization".into(),
                    lash_llm_transport::HttpHeaderValue::sensitive(format!(
                        "Bearer {REQUEST_SECRET}"
                    )),
                )],
                body: Bytes::new(),
                body_for_error: None,
                response_start_timeout_message: None,
            },
            Arc::new(Mutex::new(Vec::new())),
        )
        .expect("exchange")
    }

    #[test]
    fn recorder_keeps_only_selected_headers_and_sanitizes_provenance_notes() {
        let output = tempfile::tempdir().expect("recording directory");
        for (prefix, allow_list) in [
            ("empty", vec![]),
            ("selected", vec!["X-Request-ID", "Authorization"]),
        ] {
            let exchange = recording_exchange(
                ProviderRecordingConfig::new(output.path(), prefix, "openai")
                    .with_response_header_allow_list(allow_list)
                    .with_notes(format!("capture {REQUEST_SECRET}")),
            );
            let mut headers = vec![
                ("x-request-id".into(), "request-safe".into()),
                ("content-type".into(), "text/plain".into()),
            ];
            if prefix == "selected" {
                headers.push(("authorization".into(), "opaque-response-credential".into()));
            }
            exchange
                .write_response(200, &headers, b"safe", Duration::ZERO, Duration::ZERO, None)
                .expect("recording");
            let encoded = fs::read_to_string(output.path().join(format!("{prefix}.001.json")))
                .expect("artifact");
            assert!(!encoded.contains(REQUEST_SECRET));
            assert!(!encoded.contains("opaque-response-credential"));
            assert!(!encoded.contains("content-type"));
            assert_eq!(encoded.contains("request-safe"), prefix == "selected");
            assert!(encoded.contains(REDACTED));
        }
    }

    #[test]
    fn recorder_refuses_secret_patterns_in_serialized_artifact_without_echoing_values() {
        let output = tempfile::tempdir().expect("recording directory");
        for secret in [
            "Bearer unknownCredential123456789",
            "Bearer   unknownCredential123456789",
            r#"{"echo":"Bearer unknownCredential123456789"}"#,
            "https://provider.test?key=sk-unknownCredential123456789",
            "sk-unknownCredential123456789",
            "sk-ant-unknownCredential123456789",
            "AIzaUnknownCredential123456789",
            "AKIA1234567890ABCDEF",
            "ghp_unknownCredential123456789",
            "-----BEGIN RSA PRIVATE KEY-----",
        ] {
            for surface in ["body", "notes", "matcher-key"] {
                let mut config = ProviderRecordingConfig::new(output.path(), "secret", "openai");
                if surface == "notes" {
                    config = config.with_notes(secret);
                }
                if surface == "matcher-key" {
                    config = config.with_request_match(ProviderWireRequestMatch {
                        any: false,
                        body: [(
                            secret.into(),
                            JsonMatcher {
                                equals: Some(Value::Bool(true)),
                                ..Default::default()
                            },
                        )]
                        .into_iter()
                        .collect(),
                        ..Default::default()
                    });
                }
                let exchange = recording_exchange(config);
                let body = if surface == "body" {
                    secret.as_bytes()
                } else {
                    b"safe"
                };
                let error = exchange
                    .write_response(200, &[], body, Duration::ZERO, Duration::ZERO, None)
                    .expect_err("secret must prevent publication");
                assert!(error.message.contains("possible secret"));
                assert!(!error.message.contains(secret));
                assert!(error.raw.is_none());
                assert!(!output.path().join("secret.001.json").exists());
                assert!(exchange.recorded_paths.lock_recover().is_empty());
            }
        }
    }
}
