//! Merge host-owned wire fields after the adapter has written its request.

use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict};
use lash_core::{GenerationOptionOutcome, ProviderFailureKind, TurnFailureCode};
use serde_json::{Map, Value};

/// Host-supplied route headers. Debug output never exposes names or values.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ExtraHeaders(Vec<(String, String)>);

impl ExtraHeaders {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ExtraHeaders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtraHeaders")
            .field("count", &self.0.len())
            .finish()
    }
}

impl std::ops::Deref for ExtraHeaders {
    type Target = [(String, String)];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Vec<(String, String)>> for ExtraHeaders {
    fn from(value: Vec<(String, String)>) -> Self {
        Self(value)
    }
}

/// Raw controls must not restore a value the protocol removed or the model pins.
pub fn reserved_generation_paths(
    request: &lash_core::llm::types::LlmRequest,
    stop_path: &'static str,
    temperature_path: &'static str,
) -> Vec<&'static str> {
    let mut reserved = Vec::new();
    if request.generation.stop_sequences_suppressed_by_protocol() {
        reserved.push(stop_path);
    }
    if request.model_capability.sampling == lash_core::provider::SamplingCapability::Pinned {
        reserved.push(temperature_path);
    }
    reserved
}

fn conflict(detail: String) -> LlmTransportError {
    LlmTransportError::new(format!("passthrough conflicts with adapter-owned {detail}"))
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(TurnFailureCode::PassthroughConflict)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}

fn pointer_part(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn check_object(
    body: &Map<String, Value>,
    extra: &Map<String, Value>,
    prefix: &str,
    reserved: &[&str],
) -> Result<(), LlmTransportError> {
    for (key, incoming) in extra {
        let path = format!("{prefix}/{}", pointer_part(key));
        if reserved
            .iter()
            .any(|reserved| path == *reserved || path.starts_with(&format!("{reserved}/")))
        {
            return Err(conflict(format!("JSON path {path}")));
        }
        match (body.get(key), incoming) {
            (None, Value::Object(additional)) => {
                check_object(&Map::new(), additional, &path, reserved)?;
            }
            (None, _)
                if reserved
                    .iter()
                    .any(|reserved| reserved.starts_with(&format!("{path}/"))) =>
            {
                return Err(conflict(format!("JSON path {path}")));
            }
            (None, _) => {}
            (Some(Value::Object(existing)), Value::Object(additional)) => {
                check_object(existing, additional, &path, reserved)?;
            }
            (Some(_), _) => return Err(conflict(format!("JSON path {path}"))),
        }
    }
    Ok(())
}

fn merge_object(body: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (key, incoming) in extra {
        match (body.get_mut(key), incoming) {
            (Some(Value::Object(existing)), Value::Object(additional)) => {
                merge_object(existing, additional)
            }
            _ => {
                body.insert(key.clone(), incoming.clone());
            }
        }
    }
}

/// Objects merge recursively. Arrays, null and scalars are atomic: if Lash
/// wrote the path, any host value at that path is refused, even if identical.
/// `reserved` contains paths Lash deliberately omitted or pinned.
pub fn merge_extra_body(
    body: &mut Value,
    extra: &Map<String, Value>,
    reserved: &[&str],
) -> Result<GenerationOptionOutcome, LlmTransportError> {
    if extra.is_empty() {
        return Ok(GenerationOptionOutcome::NotRequested);
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| conflict("JSON root".to_string()))?;
    check_object(object, extra, "", reserved)?;
    merge_object(object, extra);
    Ok(GenerationOptionOutcome::Applied)
}

/// Validate all names before mutating headers. Only `anthropic-beta` is
/// additive; each comma-separated token is retained once in first-seen order.
pub fn merge_extra_headers(
    headers: &mut Vec<(String, String)>,
    extra: &[(String, String)],
    additive_beta: bool,
) -> Result<GenerationOptionOutcome, LlmTransportError> {
    if extra.is_empty() {
        return Ok(GenerationOptionOutcome::NotRequested);
    }
    for (index, (name, _)) in extra.iter().enumerate() {
        if let Some((written, _)) = headers
            .iter()
            .find(|(written, _)| written.eq_ignore_ascii_case(name))
            && !(additive_beta && name.eq_ignore_ascii_case("anthropic-beta"))
        {
            return Err(conflict(format!("header {written}")));
        }
        if extra[..index]
            .iter()
            .any(|(written, _)| written.eq_ignore_ascii_case(name))
            && !(additive_beta && name.eq_ignore_ascii_case("anthropic-beta"))
        {
            return Err(conflict(format!("header {name}")));
        }
    }
    for (name, value) in extra {
        if additive_beta
            && name.eq_ignore_ascii_case("anthropic-beta")
            && let Some((_, existing)) = headers
                .iter_mut()
                .find(|(written, _)| written.eq_ignore_ascii_case(name))
        {
            let mut tokens: Vec<String> = existing
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect();
            for token in value
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                if !tokens.iter().any(|current| current == token) {
                    tokens.push(token.to_string());
                }
            }
            *existing = tokens.join(",");
            continue;
        }
        headers.push((name.clone(), value.clone()));
    }
    Ok(GenerationOptionOutcome::Applied)
}

/// Check adapter-owned names before credentials, uploads or network calls.
pub fn validate_extra_headers(
    extra: &[(String, String)],
    reserved: &[&str],
    additive_beta: bool,
) -> Result<(), LlmTransportError> {
    let mut written = reserved
        .iter()
        .map(|name| ((*name).to_string(), String::new()))
        .collect();
    merge_extra_headers(&mut written, extra, additive_beta).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object fixture")
    }

    #[test]
    fn recursive_merge_preserves_adapter_fields_and_is_atomic_on_conflict() {
        let mut body = json!({"model":"m", "nested":{"owned":1}, "array":[1], "nothing":null});
        let outcome = merge_extra_body(
            &mut body,
            &fields(json!({"nested":{"host":2},"new":{"value":true}})),
            &[],
        )
        .unwrap();
        assert_eq!(outcome, GenerationOptionOutcome::Applied);
        assert_eq!(body["nested"], json!({"owned":1,"host":2}));
        for extra in [
            json!({"model":"other"}),
            json!({"nested":{"owned":2}}),
            json!({"nested":null}),
            json!({"array":[2]}),
            json!({"array":null}),
            json!({"nothing":{"child":1}}),
        ] {
            let before = body.clone();
            let error = merge_extra_body(&mut body, &fields(extra), &[]).unwrap_err();
            assert_eq!(
                error.code.as_ref().map(ToString::to_string).as_deref(),
                Some("lash:passthrough_conflict")
            );
            assert_eq!(body, before);
        }
    }

    #[test]
    fn reserved_paths_cannot_be_reintroduced() {
        let mut body = json!({"model":"m"});
        for extra in [
            json!({"stop":["END"]}),
            json!({"temperature":0.9}),
            json!({"generationConfig":{"stopSequences":["END"]}}),
            json!({"generationConfig":null}),
        ] {
            assert!(
                merge_extra_body(
                    &mut body,
                    &fields(extra),
                    &["/stop", "/temperature", "/generationConfig/stopSequences"]
                )
                .is_err()
            );
        }
        assert_eq!(body, json!({"model":"m"}));
    }

    #[test]
    fn header_names_are_case_insensitive_and_beta_tokens_are_additive() {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        assert!(
            merge_extra_headers(
                &mut headers,
                &[("cOnTeNt-TyPe".into(), "other".into())],
                false
            )
            .is_err()
        );
        assert_eq!(headers.len(), 1);
        assert!(
            merge_extra_headers(
                &mut headers,
                &[("X-Host".into(), "a".into()), ("x-host".into(), "b".into())],
                false
            )
            .is_err()
        );
        assert_eq!(headers.len(), 1);
        let mut headers = vec![("anthropic-beta".to_string(), "a,b".to_string())];
        merge_extra_headers(
            &mut headers,
            &[("ANTHROPIC-BETA".into(), "b,c,a".into())],
            true,
        )
        .unwrap();
        assert_eq!(
            headers,
            [("anthropic-beta".to_string(), "a,b,c".to_string())]
        );
        let secret: ExtraHeaders = vec![("x-host-key".into(), "secret-marker".into())].into();
        let debug = format!("{secret:?}");
        assert!(!debug.contains("x-host-key"));
        assert!(!debug.contains("secret-marker"));
    }
}
