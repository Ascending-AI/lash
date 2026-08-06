//! Codex failure classification.
//!
//! One responsibility: turn a Codex error body into the message a user should
//! read. The default classifier already maps status to kind and retryability,
//! so Codex's only delta is rendering the quota/usage-limit and refusal bodies
//! into one friendly line.

use serde_json::Value;

use lash_core::llm::transport::ProviderFailure;
use lash_core::provider::{DefaultProviderFailureClassifier, ProviderFailureClassifier};

use super::CodexProvider;

impl CodexProvider {
    /// Translate a Codex error body into a user-friendly one-line message.
    /// Mirrors pi-mono's `openai-codex-responses.ts:880-904`: for a
    /// `usage_limit_reached`/`rate_limit_exceeded` code (or any 429),
    /// parse the `plan_type` and `resets_at` epoch and render
    /// `"You have hit your ChatGPT usage limit (plus plan). Try again in
    /// ~12 min."`. Returns `None` when the body isn't parseable or the
    /// status doesn't match the pattern, so the caller falls back to the
    /// raw status.
    pub(super) fn codex_error_summary(status: u16, body_text: &str) -> Option<String> {
        let parsed: Value = serde_json::from_str(body_text).ok()?;
        if let Some(detail) = parsed.get("detail").and_then(|v| v.as_str()) {
            return Some(format!("Codex request failed with {status}: {detail}"));
        }
        let err = parsed.get("error")?;
        let code = err
            .get("code")
            .and_then(|v| v.as_str())
            .or_else(|| err.get("type").and_then(|v| v.as_str()))
            .unwrap_or("");
        let code_matches = {
            let lc = code.to_ascii_lowercase();
            lc.contains("usage_limit_reached")
                || lc.contains("usage_not_included")
                || lc.contains("rate_limit_exceeded")
        };
        if !code_matches && status != 429 {
            // Prefer the raw `error.message` if the server gave us one —
            // useful for refusals, invalid-request errors, etc.
            let msg = err.get("message").and_then(|v| v.as_str())?;
            return Some(format!("Codex request failed with {status}: {msg}"));
        }

        let plan = err
            .get("plan_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|p| format!(" ({} plan)", p.to_ascii_lowercase()))
            .unwrap_or_default();
        let resets_at_secs = err.get("resets_at").and_then(|v| v.as_i64());
        let mins = resets_at_secs.and_then(|ts| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            let delta_secs = ts - now;
            if delta_secs <= 0 {
                Some(0)
            } else {
                Some(((delta_secs + 30) / 60).max(0))
            }
        });
        let when = match mins {
            Some(m) => format!(" Try again in ~{m} min."),
            None => String::new(),
        };
        Some(format!(
            "You have hit your ChatGPT usage limit{plan}.{when}"
        ))
    }
}

#[derive(Debug)]
pub(super) struct CodexFailureClassifier;

impl ProviderFailureClassifier for CodexFailureClassifier {
    fn classify(&self, failure: ProviderFailure) -> ProviderFailure {
        // The default classifier already covers everything Codex needs from a
        // status/text standpoint: HTTP-status → kind/retryability, the
        // usage-limit/quota and content-filter text markers, and context
        // overflow. Codex's only genuine delta is rewriting the user-facing
        // message into a friendly "you hit your ChatGPT usage limit" form.
        let status = failure
            .status
            .or_else(|| failure.code.as_deref().and_then(|code| code.parse().ok()));
        let summary = status.and_then(|status| {
            CodexProvider::codex_error_summary(
                status,
                failure
                    .raw
                    .as_deref()
                    .map(String::as_str)
                    .unwrap_or_default(),
            )
        });
        let mut failure = DefaultProviderFailureClassifier.classify(failure);
        if let Some(summary) = summary {
            failure.message = summary;
        }
        failure
    }
}
