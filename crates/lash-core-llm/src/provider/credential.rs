//! The host-owned credential seam (D-HOSTCREDENTIALS).
//!
//! Lash manages no OAuth. A host owns login, refresh, rotation and secret
//! storage, and hands each provider a [`TokenSource`]. The provider asks it for
//! a [`ProviderToken`] before every model-call attempt and once more, with
//! [`TokenRequestReason::Rejected`], after a 401 that arrives before any
//! output. A fixed API key is the trivial source: a [`ProviderToken`] answers
//! every request with itself.

use std::fmt;
use std::time::{Duration, SystemTime};

use lash_http_transport::{LlmTransportError, TransportRetryVerdict};
use lash_sansio::Redacted;
use lash_sansio::llm::types::{ProviderFailureKind, ProviderRouteIdentity};
use lash_sansio::session_model::TurnFailureCode;

/// A credential the host minted for one provider route. Lash holds it only for
/// one model-call attempt and never records it: it has no `Serialize`, and its
/// `Debug` prints neither the secret nor the bound account.
#[derive(Clone)]
pub struct ProviderToken {
    secret: Redacted,
    expires_at: Option<SystemTime>,
    account: Option<Redacted>,
    principal: Option<String>,
}

impl ProviderToken {
    pub fn new(secret: impl Into<Redacted>) -> Self {
        Self {
            secret: secret.into(),
            expires_at: None,
            account: None,
            principal: None,
        }
    }

    /// When the issuer says the token stops working. Lash asks the source once
    /// more, with [`TokenRequestReason::Expiring`], when it is this close.
    pub fn expiring_at(mut self, at: SystemTime) -> Self {
        self.expires_at = Some(at);
        self
    }

    /// Identity bound to this token by its issuer, sent where the provider's
    /// wire needs it (Codex: `ChatGPT-Account-ID`). Secret-grade.
    pub fn with_account(mut self, account: impl Into<Redacted>) -> Self {
        self.account = Some(account.into());
        self
    }

    /// A stable, non-secret name for the principal (for example a hashed
    /// account id), used only to partition provider-side caches such as
    /// Google uploads. Never sent on the wire.
    pub fn with_principal(mut self, principal: impl Into<String>) -> Self {
        self.principal = Some(principal.into());
        self
    }

    pub fn secret(&self) -> &Redacted {
        &self.secret
    }

    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }

    pub fn account(&self) -> Option<&Redacted> {
        self.account.as_ref()
    }

    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    /// Whether `other` carries the same secret: a source answering a
    /// replacement request with it has nothing newer.
    pub fn same_secret(&self, other: &Self) -> bool {
        self.secret == other.secret
    }
}

impl fmt::Debug for ProviderToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderToken")
            .field("secret", &self.secret)
            .field("expires_at", &self.expires_at)
            .field("account", &self.account)
            .field("principal", &self.principal)
            .finish()
    }
}

/// Why lash is asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenRequestReason {
    /// Before an attempt. Answer with the current token; refresh only if the
    /// host's own cache says so.
    Current,
    /// The token lash was given expires within lash's skew (30 s).
    Expiring,
    /// The provider answered 401 before any output with the `stale` token.
    Rejected,
}

/// What lash knows when it asks. It carries no secret except the stale token.
#[derive(Debug)]
#[non_exhaustive]
pub struct TokenRequest<'a> {
    /// The provider kind (`Provider::kind`): "codex", "google_oauth",
    /// "anthropic", ...
    pub provider: &'static str,
    /// The route the token will be sent to (endpoint and model identity).
    pub route: &'a ProviderRouteIdentity,
    pub reason: TokenRequestReason,
    /// For `Expiring` and `Rejected`: the token lash used. If it is no longer
    /// the host's current token, return the current one without refreshing
    /// (compare-and-refresh). Returning the same secret means "nothing newer".
    pub stale: Option<&'a ProviderToken>,
}

impl<'a> TokenRequest<'a> {
    pub fn new(
        provider: &'static str,
        route: &'a ProviderRouteIdentity,
        reason: TokenRequestReason,
        stale: Option<&'a ProviderToken>,
    ) -> Self {
        Self {
            provider,
            route,
            reason,
            stale,
        }
    }
}

/// The host's token source. Hosts own login, refresh, rotation and storage.
/// Lash calls it before every attempt and once more after a pre-output 401.
/// It must be cheap when nothing changed, and safe to call concurrently.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync + fmt::Debug + 'static {
    async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError>;
}

/// A fixed API key is the trivial source: it always answers with itself.
#[async_trait::async_trait]
impl TokenSource for ProviderToken {
    async fn token(&self, _request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
        Ok(self.clone())
    }
}

/// The host's own classification of why it cannot supply a token.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct TokenError {
    pub kind: TokenErrorKind,
    /// Host-authored and non-secret.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TokenErrorKind {
    /// The login was revoked or expired; a person must sign in again.
    #[error("sign in again")]
    ReauthRequired,
    /// The host's store or identity provider is briefly unavailable.
    #[error("credential source unavailable for now")]
    Transient { retry_after: Option<Duration> },
    /// Anything else the host cannot fix.
    #[error("credential unavailable")]
    Unavailable,
}

impl TokenError {
    pub fn new(kind: TokenErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The typed transport failure a model call surfaces for this error.
    pub fn into_transport_error(self) -> LlmTransportError {
        let (code, kind, verdict) = match self.kind {
            TokenErrorKind::ReauthRequired => (
                TurnFailureCode::CredentialReauthRequired,
                ProviderFailureKind::Auth,
                TransportRetryVerdict::Forbidden,
            ),
            TokenErrorKind::Transient {
                retry_after: Some(retry_after),
            } => (
                TurnFailureCode::CredentialSourceTransient,
                ProviderFailureKind::Transport,
                TransportRetryVerdict::RetryableThrottle {
                    retry_after: Some(retry_after),
                },
            ),
            TokenErrorKind::Transient { retry_after: None } => (
                TurnFailureCode::CredentialSourceTransient,
                ProviderFailureKind::Transport,
                TransportRetryVerdict::RetryableTransient,
            ),
            TokenErrorKind::Unavailable => (
                TurnFailureCode::CredentialUnavailable,
                ProviderFailureKind::Auth,
                TransportRetryVerdict::Forbidden,
            ),
        };
        LlmTransportError::new(self.to_string())
            .with_kind(kind)
            .with_lash_code(code)
            .with_retry_verdict(verdict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ProviderToken` must never reach a serialized record. `Probe<T>`'s
    /// inherent `SERIALIZES` exists only when `T: Serialize`; otherwise the
    /// blanket trait constant answers `false`.
    trait NotSerialize {
        const SERIALIZES: bool = false;
    }
    impl<T> NotSerialize for T {}
    struct Probe<T>(std::marker::PhantomData<T>);
    impl<T: serde::Serialize> Probe<T> {
        #[expect(dead_code, reason = "resolves only for a serializable probe")]
        const SERIALIZES: bool = true;
    }

    #[test]
    fn a_provider_token_cannot_serialize_and_debug_redacts_every_secret() {
        const { assert!(!<Probe<ProviderToken>>::SERIALIZES) };
        let token = ProviderToken::new("secret-sentinel")
            .with_account("account-sentinel")
            .with_principal("principal-hash");
        let debug = format!("{token:?}");
        assert!(!debug.contains("secret-sentinel"), "leaked: {debug}");
        assert!(!debug.contains("account-sentinel"), "leaked: {debug}");
        assert!(debug.contains("principal-hash"));
    }

    #[test]
    fn each_host_token_error_maps_to_its_typed_failure_code() {
        let cases = [
            (
                TokenErrorKind::ReauthRequired,
                "lash:credential_reauth_required",
                ProviderFailureKind::Auth,
                TransportRetryVerdict::Forbidden,
            ),
            (
                TokenErrorKind::Transient { retry_after: None },
                "lash:credential_source_transient",
                ProviderFailureKind::Transport,
                TransportRetryVerdict::RetryableTransient,
            ),
            (
                TokenErrorKind::Transient {
                    retry_after: Some(Duration::from_secs(3)),
                },
                "lash:credential_source_transient",
                ProviderFailureKind::Transport,
                TransportRetryVerdict::RetryableThrottle {
                    retry_after: Some(Duration::from_secs(3)),
                },
            ),
            (
                TokenErrorKind::Unavailable,
                "lash:credential_unavailable",
                ProviderFailureKind::Auth,
                TransportRetryVerdict::Forbidden,
            ),
        ];
        for (kind, code, failure_kind, verdict) in cases {
            let error = TokenError::new(kind, "host says no").into_transport_error();
            assert_eq!(
                error.code.as_ref().map(ToString::to_string).as_deref(),
                Some(code)
            );
            assert_eq!(error.kind, failure_kind);
            assert_eq!(error.retry_verdict, verdict);
        }
    }
}
