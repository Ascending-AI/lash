use super::{LlmResponse, LlmTerminalReason};
use serde::Serialize;

/// Error details from a failed LLM call.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct LlmCallError {
    #[serde(serialize_with = "serialize_provider_failure_message")]
    pub message: String,
    pub retryable: bool,
    /// Required transport classification. Non-provider failures explicitly
    /// carry `ProviderFailureKind::Unknown`; missing or future kinds are refused.
    pub kind: crate::llm::types::ProviderFailureKind,
    #[serde(default, skip_serializing, skip_deserializing)]
    pub raw: Option<String>,
    /// Namespaced failure code: `provider` spellings are provider-owned,
    /// `lash` spellings are Lash-authored (pre-cutover `adapter`/`refusal`
    /// namespaces decode as Lash vocabulary), and every other namespace is a
    /// foreign-owned pair carried verbatim.
    pub code: Option<crate::session_model::FailureCode>,
    pub terminal_reason: LlmTerminalReason,
    pub request_body: Option<String>,
    /// Output and usage observed before the failed stream ended. Partial tool
    /// calls in this response are retained for diagnosis but never executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_response: Option<Box<LlmResponse>>,
}

fn serialize_provider_failure_message<S>(_: &String, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str("provider call failed")
}

#[cfg(test)]
mod llm_call_error_tests {
    use super::LlmCallError;
    use crate::llm::types::ProviderFailureKind;

    #[test]
    fn journaled_provider_error_omits_free_text() {
        const SECRET: &str = "api_key= secret Authorization: Basic abc";
        let error = LlmCallError {
            message: SECRET.to_string(),
            retryable: false,
            kind: ProviderFailureKind::Http,
            raw: Some(SECRET.to_string()),
            code: Some(crate::session_model::FailureCode::provider(
                "rate_limit_exceeded",
            )),
            terminal_reason: crate::llm::types::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: None,
        };
        let journaled = serde_json::to_string(&error).expect("serialize effect result");
        assert!(!journaled.contains(SECRET));
        assert!(!journaled.contains("Basic abc"));
        assert!(journaled.contains("provider:rate_limit_exceeded"));
        let replayed: LlmCallError = serde_json::from_str(&journaled).expect("replay error");
        assert_eq!(replayed.message, "provider call failed");
        assert_eq!(replayed.raw, None);
    }

    #[test]
    fn llm_call_error_requires_a_recognized_journal_kind() {
        let mut wire = serde_json::json!({"message":"rate limited","retryable":true,"raw":null,"code":"429","terminal_reason":"provider_error","request_body":null});
        assert!(
            serde_json::from_value::<LlmCallError>(wire.clone())
                .unwrap_err()
                .to_string()
                .contains("kind")
        );
        wire["kind"] = serde_json::json!("future_kind");
        assert!(
            serde_json::from_value::<LlmCallError>(wire.clone())
                .unwrap_err()
                .to_string()
                .contains("future_kind")
        );
        for (literal, expected) in [
            ("transport", ProviderFailureKind::Transport),
            ("timeout", ProviderFailureKind::Timeout),
            ("http", ProviderFailureKind::Http),
            ("stream", ProviderFailureKind::Stream),
            ("auth", ProviderFailureKind::Auth),
            ("validation", ProviderFailureKind::Validation),
            ("quota", ProviderFailureKind::Quota),
            ("unsupported", ProviderFailureKind::Unsupported),
            ("unknown", ProviderFailureKind::Unknown),
        ] {
            wire["kind"] = serde_json::json!(literal);
            let decoded = serde_json::from_value::<LlmCallError>(wire.clone()).unwrap();
            assert_eq!(decoded.kind, expected);
            assert_eq!(serde_json::to_value(decoded).unwrap()["kind"], literal);
        }
    }
}
