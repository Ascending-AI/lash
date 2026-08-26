//! Codex OAuth credential material and its refresh path.
//!
//! One responsibility: hold the ChatGPT OAuth tokens Codex authenticates with,
//! keep them out of every debug/display rendering, and exchange a refresh token
//! for a new access token when the credential manager asks.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_provider_auth::{
    Credential, CredentialError, CredentialErrorKind, CredentialRefresher, RefreshCause,
    classify_oauth_refresh_error,
};

use super::oauth;

#[derive(Clone)]
pub(super) struct CodexCredential {
    pub(super) access_token: String,
    pub(super) refresh_token: String,
    pub(super) expires_at: u64,
    pub(super) account_id: Option<String>,
}

impl std::fmt::Debug for CodexCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCredential")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl std::fmt::Display for CodexCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CodexCredential([REDACTED])")
    }
}

impl Credential for CodexCredential {
    fn expires_at(&self) -> Option<SystemTime> {
        (self.expires_at != 0)
            .then(|| UNIX_EPOCH.checked_add(Duration::from_secs(self.expires_at)))
            .flatten()
    }
}

#[derive(Debug)]
pub(super) struct CodexCredentialRefresher;

#[async_trait]
impl CredentialRefresher<CodexCredential> for CodexCredentialRefresher {
    async fn refresh(
        &self,
        current: &CodexCredential,
        _cause: RefreshCause,
    ) -> Result<CodexCredential, CredentialError> {
        let tokens = oauth::refresh_tokens(&current.refresh_token)
            .await
            .map_err(classify_oauth_refresh_error)?;
        Ok(CodexCredential {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
            account_id: tokens.account_id.or_else(|| current.account_id.clone()),
        })
    }
}

pub(super) fn credential_transport_error(error: CredentialError) -> LlmTransportError {
    let code = match error.kind {
        CredentialErrorKind::InvalidGrant => "credential_invalid_grant",
        CredentialErrorKind::Transient => "credential_refresh_transient",
        CredentialErrorKind::Other => "credential_refresh_failed",
    };
    let retry_verdict = if error.retryable {
        TransportRetryVerdict::RetryableTransient
    } else {
        TransportRetryVerdict::Forbidden
    };
    let failure_kind = if error.retryable {
        ProviderFailureKind::Transport
    } else {
        ProviderFailureKind::Auth
    };
    LlmTransportError::new(error.to_string())
        .with_kind(failure_kind)
        .with_code(code)
        .with_retry_verdict(retry_verdict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::failure::CodexFailureClassifier;
    use lash_core::provider::ProviderFailureClassifier;

    #[test]
    fn credential_failures_preserve_transient_and_permission_retry_verdicts() {
        let transient = CodexFailureClassifier
            .classify(credential_transport_error(CredentialError::transient()));
        assert_eq!(transient.kind, ProviderFailureKind::Transport);
        assert_eq!(
            transient.code.as_deref(),
            Some("credential_refresh_transient")
        );
        assert_eq!(
            transient.retry_verdict,
            TransportRetryVerdict::RetryableTransient
        );

        let permission = CodexFailureClassifier
            .classify(credential_transport_error(CredentialError::invalid_grant()));
        assert_eq!(permission.kind, ProviderFailureKind::Auth);
        assert_eq!(permission.code.as_deref(), Some("credential_invalid_grant"));
        assert_eq!(permission.retry_verdict, TransportRetryVerdict::Forbidden);
        assert!(permission.message.contains("sign in again"));
    }
}
