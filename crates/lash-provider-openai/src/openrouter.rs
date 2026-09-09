//! OpenRouter-specific after-the-fact usage lookup (FIG-2765).
//!
//! OpenRouter bills a generation even when the client closes the stream
//! before the terminal usage chunk arrives — the exact shape of an RLM cell
//! boundary abort under ADR 0074's no-wire-stop rule. Its generation record
//! (`GET {base_url}/generation?id=<id>`) reports the native token counts and
//! total cost afterwards, so a host can turn the typed unreported hole into a
//! reconciled correction row. The lookup is host-invoked through
//! `Provider::reconcile_usage`, bounded by one timeout and one retry, and
//! never runs on the turn's hot path.

use crate::config::OpenAiCompatibleProvider;
use crate::support::*;
use lash_core::provider::ReconciledUsage;
use lash_llm_transport::{LlmHttpMethod, LlmHttpRequest, read_http_body_text};

/// Per-attempt bound on the generation lookup. Two attempts at most.
pub(crate) const GENERATION_LOOKUP_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

pub(crate) async fn reconcile_generation_usage(
    provider: &OpenAiCompatibleProvider,
    generation_id: &str,
) -> Result<Option<ReconciledUsage>, LlmTransportError> {
    // Bounded: one retry, whether the first attempt failed or the record
    // was not there yet, then report what the last attempt said.
    let mut last = lookup_generation(provider, generation_id).await;
    if !matches!(last, Ok(Some(_))) {
        last = lookup_generation(provider, generation_id).await;
    }
    last
}

#[derive(Debug)]
enum Lookup {
    Found(ReconciledUsage),
    /// The record is not there (yet): OpenRouter answers 404 for a short
    /// window after a generation ends. Retried once, then reported as `None`.
    Missing,
}

async fn lookup_generation(
    provider: &OpenAiCompatibleProvider,
    generation_id: &str,
) -> Result<Option<ReconciledUsage>, LlmTransportError> {
    let url = format!(
        "{}/generation?id={}",
        provider.base_url.trim_end_matches('/'),
        percent_encode(generation_id)
    );
    let request = LlmHttpRequest::new(LlmHttpMethod::Get, url, Vec::<u8>::new())
        .with_header(
            provider.wire.auth_header_name.clone(),
            format!(
                "{}{}",
                provider.wire.auth_value_prefix,
                provider.api_key.expose_secret()
            ),
        )
        .with_header("Accept", "application/json")
        .with_response_start_timeout_message("OpenRouter generation lookup timed out");
    let response = provider
        .transport
        .send(request, Some(GENERATION_LOOKUP_TIMEOUT))
        .await?;
    let status = response.status;
    let body = read_http_body_text(
        response.body,
        Some(GENERATION_LOOKUP_TIMEOUT),
        "OpenRouter generation lookup body timed out",
    )
    .await?;
    match classify(status, &body)? {
        Lookup::Found(usage) => Ok(Some(usage)),
        Lookup::Missing => Ok(None),
    }
}

fn classify(status: u16, body: &str) -> Result<Lookup, LlmTransportError> {
    if status == 404 {
        return Ok(Lookup::Missing);
    }
    if !(200..300).contains(&status) {
        return Err(LlmTransportError::new(format!(
            "OpenRouter generation lookup failed with HTTP {status}"
        ))
        .with_status(status)
        .with_raw(crate::request_work::body_excerpt(body))
        .with_kind(if status == 429 {
            ProviderFailureKind::Quota
        } else {
            ProviderFailureKind::Http
        }));
    }
    let value: Value = serde_json::from_str(body).map_err(|error| {
        LlmTransportError::new(format!(
            "OpenRouter generation lookup returned malformed JSON: {error}"
        ))
        .with_raw(crate::request_work::body_excerpt(body))
    })?;
    let Some(data) = value.get("data").filter(|data| data.is_object()) else {
        return Err(LlmTransportError::new(
            "OpenRouter generation lookup returned no `data` object",
        )
        .with_raw(crate::request_work::body_excerpt(body)));
    };
    Ok(Lookup::Found(ReconciledUsage {
        usage: usage_from_generation(data),
        provider_usage: data.clone(),
    }))
}

/// OpenRouter reports both normalized (`tokens_*`) and native (`native_tokens_*`)
/// counts; billing follows the native counts, so those win when present.
fn usage_from_generation(data: &Value) -> LlmUsage {
    let count = |native: &str, normalized: &str| {
        data.get(native)
            .and_then(Value::as_i64)
            .or_else(|| data.get(normalized).and_then(Value::as_i64))
            .unwrap_or(0)
    };
    let cache_read_input_tokens = count("native_tokens_cached", "tokens_cached");
    LlmUsage {
        input_tokens: count("native_tokens_prompt", "tokens_prompt")
            .saturating_sub(cache_read_input_tokens),
        output_tokens: count("native_tokens_completion", "tokens_completion"),
        cache_read_input_tokens,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: count("native_tokens_reasoning", "tokens_reasoning"),
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_usage_prefers_native_counts_and_separates_cached_input() {
        let data = json!({
            "tokens_prompt": 100,
            "tokens_completion": 40,
            "native_tokens_prompt": 120,
            "native_tokens_completion": 35,
            "native_tokens_reasoning": 7,
            "native_tokens_cached": 20,
            "total_cost": 0.0042,
            "cancelled": true,
        });
        assert_eq!(
            usage_from_generation(&data),
            LlmUsage {
                input_tokens: 100,
                output_tokens: 35,
                cache_read_input_tokens: 20,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 7,
            }
        );
    }

    #[test]
    fn generation_ids_are_percent_encoded_in_the_query() {
        assert_eq!(percent_encode("gen-1 2/3"), "gen-1%202%2F3");
    }

    #[test]
    fn non_success_statuses_other_than_404_are_errors() {
        assert!(matches!(classify(404, "").unwrap(), Lookup::Missing));
        let error = classify(500, "{\"error\":\"boom\"}").unwrap_err();
        assert_eq!(error.status, Some(500));
        assert_eq!(error.kind, ProviderFailureKind::Http);
        assert!(classify(200, "not json").is_err());
        assert!(classify(200, "{\"data\":null}").is_err());
    }
}
