//! The per-provider gate between a model-call attempt and the host's
//! [`TokenSource`].
//!
//! The gate asks the source before every attempt and keeps no token cache of
//! its own: the host caches. It does two things a naive source cannot. It
//! single-flights replacements, so concurrent 401s holding one token epoch make
//! one host call between them. And it numbers token epochs, so per-token state
//! such as the Codex WebSocket session cache can evict what an older token
//! opened.
//!
//! A token is minted for one route, so all of that is kept per route: a
//! replacement on one route never waits on, or answers with, another route's
//! token. Within a route, epoch order is replacement order: a `Current` answer
//! that a replacement overtook is not published.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::ProviderRouteIdentity;
use lash_core::provider::{ProviderToken, TokenRequest, TokenRequestReason, TokenSource};
use lash_core::runtime::{Clock, SystemClock};
use lash_sansio::sync::MutexExt;

/// Proactive token renewal policy, configurable on each provider constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenPolicy {
    /// Renew when expiry is at most this far away. Zero renews only expired tokens.
    pub expiry_skew: Duration,
}
impl TokenPolicy {
    /// Renew 30 seconds before expiry. This historical cushion avoids expiry
    /// during a call; no provider-neutral workload measurement backs 30 seconds.
    pub fn standard() -> Self {
        Self {
            expiry_skew: Duration::from_secs(30),
        }
    }
}
impl Default for TokenPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

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
    bindings: std::sync::Mutex<Bindings>,
}

/// What the gate knows per route. In memory only.
#[derive(Default)]
struct Bindings {
    /// The newest epoch any route opened. Current answers may share a secret's
    /// epoch across routes; each replacement opens a fresh epoch.
    newest_epoch: u64,
    by_route: HashMap<ProviderRouteIdentity, Binding>,
}

#[derive(Default)]
struct Binding {
    /// Held across the host call of one replacement on this route.
    flight: Arc<tokio::sync::Mutex<()>>,
    /// How many replacements this route has published. A `Current` answer is
    /// published only if this did not move while the source was answering.
    replacements: u64,
    /// The last token published for this route, and its epoch. It hands the
    /// waiters of one replacement the token their flight fetched.
    lease: Option<TokenLease>,
}

/// What a `Current` answer must not have been overtaken by, or `Replacement`.
#[derive(Clone, Copy)]
enum Publication {
    Current { replacements: u64 },
    Replacement,
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
            skew: TokenPolicy::standard().expiry_skew,
            bindings: std::sync::Mutex::default(),
        }
    }

    /// Configure a new gate for the same source and clock, with fresh token epochs.
    /// Provider builders call this before use; existing provider clones retain their policy.
    pub fn configured(&self, policy: TokenPolicy) -> Self {
        let mut gate = Self::with_clock(self.source.clone(), self.provider, self.clock.clone());
        gate.skew = policy.expiry_skew;
        gate
    }

    /// The token for one attempt: one host call, plus one `Expiring` call when
    /// the answer is about to expire. An answer that a replacement on this
    /// route overtook may predate it, so the source is asked again.
    pub async fn current(
        &self,
        route: &ProviderRouteIdentity,
    ) -> Result<TokenLease, LlmTransportError> {
        let lease = loop {
            let replacements = self
                .bindings
                .lock_recover()
                .by_route
                .get(route)
                .map_or(0, |binding| binding.replacements);
            let token = self.ask(route, TokenRequestReason::Current, None).await?;
            if let Some(lease) = self.publish(route, token, Publication::Current { replacements }) {
                break lease;
            }
        };
        if self.expiring(&lease.token) {
            return Ok(self
                .replace(route, &lease, TokenRequestReason::Expiring)
                .await?
                .unwrap_or(lease));
        }
        Ok(lease)
    }

    /// After a pre-output 401 with `rejected`. Concurrent callers on one route
    /// holding the same epoch make one host call between them. `Ok(None)` means the source
    /// has nothing newer, so the caller surfaces the provider's 401.
    pub async fn replace(
        &self,
        route: &ProviderRouteIdentity,
        rejected: &TokenLease,
        reason: TokenRequestReason,
    ) -> Result<Option<TokenLease>, LlmTransportError> {
        let flight = self.flight(route);
        let _flight = flight.lock().await;
        let newer = self
            .bindings
            .lock_recover()
            .by_route
            .get(route)
            .and_then(|binding| binding.lease.clone())
            .filter(|lease| lease.epoch > rejected.epoch);
        if let Some(newer) = newer {
            // Someone replaced it while we waited: take what that flight got.
            return Ok((!newer.token.same_secret(&rejected.token)).then_some(newer));
        }
        let token = self.ask(route, reason, Some(&rejected.token)).await?;
        if token.same_secret(&rejected.token) {
            return Ok(None);
        }
        Ok(self.publish(route, token, Publication::Replacement))
    }

    /// The newest epoch the gate has opened on any route.
    pub fn epoch(&self) -> u64 {
        self.bindings.lock_recover().newest_epoch
    }

    fn flight(&self, route: &ProviderRouteIdentity) -> Arc<tokio::sync::Mutex<()>> {
        let mut bindings = self.bindings.lock_recover();
        if let Some(binding) = bindings.by_route.get(route) {
            return Arc::clone(&binding.flight);
        }
        Arc::clone(&bindings.by_route.entry(route.clone()).or_default().flight)
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

    /// Numbers `token` and makes it `route`'s newest lease: a replacement or a
    /// secret no route holds opens a new epoch. `None` when a `Current` answer was overtaken
    /// by a replacement and is not the token that replacement published.
    fn publish(
        &self,
        route: &ProviderRouteIdentity,
        token: ProviderToken,
        publication: Publication,
    ) -> Option<TokenLease> {
        let mut bindings = self.bindings.lock_recover();
        let Bindings {
            newest_epoch,
            by_route,
        } = &mut *bindings;
        let binding = by_route.get(route);
        let overtaken = match publication {
            Publication::Current { replacements } => {
                binding.map_or(0, |binding| binding.replacements) != replacements
            }
            Publication::Replacement => false,
        };
        if overtaken {
            return binding
                .and_then(|binding| binding.lease.as_ref())
                .filter(|lease| lease.token.same_secret(&token))
                .cloned();
        }
        let epoch = by_route
            .values()
            .filter_map(|binding| binding.lease.as_ref())
            .filter(|lease| {
                matches!(publication, Publication::Current { .. })
                    && lease.token.same_secret(&token)
            })
            .map(|lease| lease.epoch)
            .max()
            .unwrap_or_else(|| {
                *newest_epoch = newest_epoch.saturating_add(1);
                *newest_epoch
            });
        let lease = TokenLease { token, epoch };
        let binding = by_route.entry(route.clone()).or_default();
        if matches!(publication, Publication::Replacement) {
            binding.replacements += 1;
        }
        binding.lease = Some(lease.clone());
        Some(lease)
    }

    fn expiring(&self, token: &ProviderToken) -> bool {
        let now = UNIX_EPOCH + Duration::from_millis(self.clock.timestamp_ms());
        token.expires_at().is_some_and(|expires_at| {
            now.checked_add(self.skew)
                .is_none_or(|refresh_at| expires_at <= refresh_at)
        })
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
