//! The per-provider gate between a model-call attempt and the host's
//! [`TokenSource`].
//!
//! The gate asks the source before every attempt and keeps no token cache of
//! its own: the host caches. It does two things a naive source cannot. It
//! single-flights replacements, so concurrent 401s holding one token epoch make
//! one host call between them. And it numbers token epochs, so per-token state
//! such as the Codex WebSocket session cache can evict what an older token
//! opened.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::ProviderRouteIdentity;
use lash_core::provider::{ProviderToken, TokenRequest, TokenRequestReason, TokenSource};
use lash_core::runtime::{Clock, SystemClock};
use lash_sansio::sync::MutexExt;

/// How close to its expiry a token may be before lash asks for another.
pub const TOKEN_EXPIRY_SKEW: Duration = Duration::from_secs(30);

/// One attempt's token and the epoch the gate numbered it with.
#[derive(Clone, Debug)]
pub struct TokenLease {
    pub token: ProviderToken,
    pub epoch: u64,
}

/// Shared by every clone of one provider through `Arc`.
pub struct TokenGate {
    source: Arc<dyn TokenSource>,
    provider: &'static str,
    clock: Arc<dyn Clock>,
    skew: Duration,
    replace: tokio::sync::Mutex<()>,
    /// The last token the source answered with, and its epoch. In memory only;
    /// it hands the waiters of one replacement the token their flight fetched.
    seen: std::sync::Mutex<Option<TokenLease>>,
}

impl std::fmt::Debug for TokenGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenGate")
            .field("source", &self.source)
            .field("provider", &self.provider)
            .field("epoch", &self.epoch())
            .finish()
    }
}

impl TokenGate {
    pub fn new(source: Arc<dyn TokenSource>, provider: &'static str) -> Self {
        Self::with_clock(source, provider, Arc::new(SystemClock))
    }

    pub fn with_clock(
        source: Arc<dyn TokenSource>,
        provider: &'static str,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            source,
            provider,
            clock,
            skew: TOKEN_EXPIRY_SKEW,
            replace: tokio::sync::Mutex::new(()),
            seen: std::sync::Mutex::new(None),
        }
    }

    /// The token for one attempt: one host call, plus one `Expiring` call when
    /// the answer is about to expire.
    pub async fn current(
        &self,
        route: &ProviderRouteIdentity,
    ) -> Result<TokenLease, LlmTransportError> {
        let token = self.ask(route, TokenRequestReason::Current, None).await?;
        let lease = self.lease_for(token);
        if self.expiring(&lease.token) {
            return Ok(self
                .replace(route, &lease, TokenRequestReason::Expiring)
                .await?
                .unwrap_or(lease));
        }
        Ok(lease)
    }

    /// After a pre-output 401 with `rejected`. Concurrent callers holding the
    /// same epoch make one host call between them. `Ok(None)` means the source
    /// has nothing newer, so the caller surfaces the provider's 401.
    pub async fn replace(
        &self,
        route: &ProviderRouteIdentity,
        rejected: &TokenLease,
        reason: TokenRequestReason,
    ) -> Result<Option<TokenLease>, LlmTransportError> {
        let _flight = self.replace.lock().await;
        if let Some(newer) = self
            .seen
            .lock_recover()
            .as_ref()
            .filter(|seen| seen.epoch > rejected.epoch)
        {
            // Someone replaced it while we waited: take what that flight got.
            return Ok((!newer.token.same_secret(&rejected.token)).then(|| newer.clone()));
        }
        let token = self.ask(route, reason, Some(&rejected.token)).await?;
        if token.same_secret(&rejected.token) {
            return Ok(None);
        }
        Ok(Some(self.lease_for(token)))
    }

    /// The epoch of the last token the source answered with.
    pub fn epoch(&self) -> u64 {
        self.seen
            .lock_recover()
            .as_ref()
            .map_or(0, |seen| seen.epoch)
    }

    async fn ask(
        &self,
        route: &ProviderRouteIdentity,
        reason: TokenRequestReason,
        stale: Option<&ProviderToken>,
    ) -> Result<ProviderToken, LlmTransportError> {
        self.source
            .token(TokenRequest::new(self.provider, route, reason, stale))
            .await
            .map_err(|error| error.into_transport_error())
    }

    /// Numbers `token`: a secret that differs from the last one seen opens a
    /// new epoch.
    fn lease_for(&self, token: ProviderToken) -> TokenLease {
        let mut seen = self.seen.lock_recover();
        let epoch = match seen.as_ref() {
            Some(last) if last.token.same_secret(&token) => last.epoch,
            Some(last) => last.epoch.saturating_add(1),
            None => 1,
        };
        let lease = TokenLease { token, epoch };
        *seen = Some(lease.clone());
        lease
    }

    fn expiring(&self, token: &ProviderToken) -> bool {
        let now = UNIX_EPOCH + Duration::from_millis(self.clock.timestamp_ms());
        token
            .expires_at()
            .is_some_and(|expires_at| expires_at <= now + self.skew)
    }
}

/// Whether `error` is a 401 the provider answered before any output escaped:
/// the one failure a replaced token may fix by resending the admitted body.
pub fn rejected_before_output(error: &LlmTransportError) -> bool {
    error.http_status == Some(401) && !error.output_started
}

#[cfg(test)]
#[path = "token_gate_tests.rs"]
mod tests;
