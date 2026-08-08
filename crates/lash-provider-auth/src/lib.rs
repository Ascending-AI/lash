//! Shared OAuth primitives used by provider crates that implement
//! OAuth-based auth (Codex, Google). API-key backends bypass this module.
//!
//! Provider-specific endpoints, device-code flows, PKCE helpers, and
//! refresh logic live in each provider crate under `oauth.rs`.

use base64::Engine;
use sha2::{Digest, Sha256};

mod credential;

pub use credential::{
    Credential, CredentialCallError, CredentialError, CredentialErrorKind, CredentialExecuteError,
    CredentialManager, CredentialPolicy, CredentialRefresher, Lease, RefreshCause,
};

#[derive(Debug)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Token exchange failed: {0}")]
    TokenExchange(String),
    #[error("Token endpoint returned HTTP {status}: {message}")]
    TokenEndpoint {
        status: u16,
        message: String,
        error_code: Option<OAuthTokenErrorCode>,
    },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthTokenErrorCode {
    InvalidGrant,
    InvalidClient,
    InvalidRequest,
    UnauthorizedClient,
    UnsupportedGrantType,
    InvalidScope,
}

impl OAuthTokenErrorCode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "invalid_grant" => Some(Self::InvalidGrant),
            "invalid_client" => Some(Self::InvalidClient),
            "invalid_request" => Some(Self::InvalidRequest),
            "unauthorized_client" => Some(Self::UnauthorizedClient),
            "unsupported_grant_type" => Some(Self::UnsupportedGrantType),
            "invalid_scope" => Some(Self::InvalidScope),
            _ => None,
        }
    }
}

impl OAuthError {
    pub fn token_endpoint(status: u16, response_body: &str, fallback_message: &str) -> Self {
        let body = serde_json::from_str::<serde_json::Value>(response_body).ok();
        let error_code = body
            .as_ref()
            .and_then(|body| body["error"].as_str())
            .and_then(OAuthTokenErrorCode::parse);
        let message = body
            .as_ref()
            .and_then(|body| {
                body["error_description"]
                    .as_str()
                    .or(body["error"].as_str())
            })
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let response_body = response_body.trim();
                if response_body.is_empty() {
                    fallback_message.to_owned()
                } else {
                    response_body.to_owned()
                }
            });
        Self::TokenEndpoint {
            status,
            message,
            error_code,
        }
    }
}

/// Convert a provider OAuth refresh failure into the credential error
/// categories used by credential refresh and retry policy.
pub fn classify_oauth_refresh_error(error: OAuthError) -> CredentialError {
    if matches!(
        &error,
        OAuthError::TokenEndpoint {
            error_code: Some(OAuthTokenErrorCode::InvalidGrant),
            ..
        }
    ) {
        CredentialError::invalid_grant()
    } else if matches!(
        error,
        OAuthError::Http(_)
            | OAuthError::TokenEndpoint {
                status: 408 | 429 | 500..=599,
                ..
            }
    ) {
        CredentialError::transient()
    } else {
        CredentialError::new(CredentialErrorKind::Other, false)
    }
}

/// Generate a PKCE code verifier and challenge pair. PKCE verifier is
/// 32 bytes of OS entropy (via two UUID v4s) base64url-encoded; the
/// challenge is its SHA-256 base64url-encoded.
pub fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = Vec::with_capacity(32);
    verifier_bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    verifier_bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&verifier_bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    (verifier, challenge)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Form-urlencoded body encoder for OAuth token endpoints.
pub fn url_form_encode(pairs: &[(&str, &str)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(pairs);
    serializer.finish()
}

/// Extract the value of a given query parameter from a URL or raw
/// query-string. Returns `None` if the key is absent.
pub fn extract_query_param(url_or_query: &str, key: &str) -> Option<String> {
    let input = url_or_query
        .split_once('#')
        .map_or(url_or_query, |(input, _)| input);
    let query = if let Some(idx) = input.find('?') {
        &input[idx + 1..]
    } else {
        input
    };

    form_urlencoded::parse(query.as_bytes())
        .find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn form_encoding_round_trips_reserved_empty_and_unicode_values() {
        let pairs = [
            ("reserved&key", "&=?+% /"),
            ("unicode", "Grüße 雪"),
            ("empty", ""),
        ];

        let encoded = url_form_encode(&pairs);

        assert_eq!(
            encoded,
            "reserved%26key=%26%3D%3F%2B%25+%2F&unicode=Gr%C3%BC%C3%9Fe+%E9%9B%AA&empty="
        );
        assert_eq!(
            form_urlencoded::parse(encoded.as_bytes())
                .into_owned()
                .collect::<Vec<_>>(),
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn malformed_percent_sequences_remain_literal_in_query_extraction() {
        // Lenient-literal decoding is deliberate for user-pasted input: the
        // authoritative validation is the remote token endpoint, and a local
        // hard failure could wrongly reject a valid value containing `%`.
        let cases = [
            ("code=%", "%"),
            ("code=%2", "%2"),
            ("code=%GG", "%GG"),
            ("code=ok%20bad%2Gtail", "ok bad%2Gtail"),
            ("code=literal%25%", "literal%%"),
        ];

        for (query, expected) in cases {
            assert_eq!(
                extract_query_param(query, "code").as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn query_extraction_handles_adversarial_redirect_urls() {
        let cases = [
            (
                "https://localhost/callback?state=x&code=a%26b%3Dc%3Fd%2Be%25f%E9%9B%AA#code=wrong",
                "code",
                Some("a&b=c?d+e%f雪"),
            ),
            ("?code=", "code", Some("")),
            ("code=first&code=second", "code", Some("first")),
            ("c%6Fde=encoded+key", "code", Some("encoded key")),
            (
                "code=good%ZZ%26still-value&admin=true",
                "code",
                Some("good%ZZ&still-value"),
            ),
            (
                "https://localhost/callback?state=x#code=fragment",
                "code",
                None,
            ),
            (
                "https://localhost/callback#fragment?code=fragment",
                "code",
                None,
            ),
            ("https://localhost/callback?other=value", "code", None),
        ];

        for (input, key, expected) in cases {
            assert_eq!(extract_query_param(input, key).as_deref(), expected);
        }
    }

    #[test]
    fn token_endpoint_parses_all_rfc_6749_error_codes() {
        let cases = [
            ("invalid_grant", OAuthTokenErrorCode::InvalidGrant),
            ("invalid_client", OAuthTokenErrorCode::InvalidClient),
            ("invalid_request", OAuthTokenErrorCode::InvalidRequest),
            (
                "unauthorized_client",
                OAuthTokenErrorCode::UnauthorizedClient,
            ),
            (
                "unsupported_grant_type",
                OAuthTokenErrorCode::UnsupportedGrantType,
            ),
            ("invalid_scope", OAuthTokenErrorCode::InvalidScope),
        ];

        for (code, expected) in cases {
            let error = OAuthError::token_endpoint(
                400,
                &format!(r#"{{"error":"{code}"}}"#),
                "token refresh failed",
            );

            assert!(matches!(
                error,
                OAuthError::TokenEndpoint {
                    status: 400,
                    error_code: Some(actual),
                    ..
                } if actual == expected
            ));
        }
    }

    #[test]
    fn invalid_grant_maps_to_visible_non_retryable_credential_error() {
        let error =
            OAuthError::token_endpoint(400, r#"{"error":"invalid_grant"}"#, "token refresh failed");

        let error = classify_oauth_refresh_error(error);

        assert_eq!(error.kind, CredentialErrorKind::InvalidGrant);
        assert!(!error.retryable);
        assert!(error.to_string().contains("sign in again"));
    }

    #[test]
    fn unparseable_body_mentioning_invalid_grant_is_not_invalid_grant() {
        let error = OAuthError::token_endpoint(
            400,
            "<html>proxy could not determine whether this was an invalid grant</html>",
            "token refresh failed",
        );

        assert!(matches!(
            &error,
            OAuthError::TokenEndpoint { status: 400, .. }
        ));
        let error = classify_oauth_refresh_error(error);
        assert_eq!(error.kind, CredentialErrorKind::Other);
        assert!(!error.retryable);
    }

    #[test]
    fn ambiguous_400_error_codes_are_not_invalid_grant() {
        let bodies = [
            r#"{"error":"invalid_client"}"#,
            r#"{"error":"unauthorized_client"}"#,
            r#"{"error":"provider_extension","error_description":"invalid_grant"}"#,
        ];

        for body in bodies {
            let error = classify_oauth_refresh_error(OAuthError::token_endpoint(
                400,
                body,
                "token refresh failed",
            ));

            assert_eq!(error.kind, CredentialErrorKind::Other);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn rate_limit_and_server_errors_are_retryable() {
        for status in [429, 500, 503, 599] {
            let error = classify_oauth_refresh_error(OAuthError::token_endpoint(
                status,
                r#"{"error":"provider_failure"}"#,
                "token refresh failed",
            ));

            assert_eq!(error.kind, CredentialErrorKind::Transient);
            assert!(error.retryable);
        }
    }

    #[tokio::test]
    async fn network_failure_is_retryable() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let request_error = reqwest::get(format!("http://{address}")).await.unwrap_err();
        assert!(request_error.is_connect());

        let error = classify_oauth_refresh_error(OAuthError::Http(request_error));

        assert_eq!(error.kind, CredentialErrorKind::Transient);
        assert!(error.retryable);
    }

    #[tokio::test]
    async fn timeout_failure_is_retryable() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(25))
            .build()
            .unwrap();
        let request_error = client
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap_err();
        assert!(request_error.is_timeout());
        server.join().unwrap();

        let error = classify_oauth_refresh_error(OAuthError::Http(request_error));

        assert_eq!(error.kind, CredentialErrorKind::Transient);
        assert!(error.retryable);
    }
}
