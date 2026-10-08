use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use lash_core::provider::{TokenError, TokenErrorKind};

use super::*;

/// A host source that rotates on every `Expiring`/`Rejected` ask whose stale
/// token is still its current one, and records every ask.
#[derive(Debug)]
struct RotatingSource {
    current: Mutex<(u32, Option<SystemTime>)>,
    asks: Mutex<Vec<TokenRequestReason>>,
    replacements: AtomicUsize,
    release: tokio::sync::Notify,
    hold_replacement: bool,
}

impl RotatingSource {
    fn new(expires_at: Option<SystemTime>, hold_replacement: bool) -> Self {
        Self {
            current: Mutex::new((1, expires_at)),
            asks: Mutex::new(Vec::new()),
            replacements: AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
            hold_replacement,
        }
    }

    fn token(generation: u32, expires_at: Option<SystemTime>) -> ProviderToken {
        let token = ProviderToken::new(format!("token-{generation}"));
        match expires_at {
            Some(at) => token.expiring_at(at),
            None => token,
        }
    }

    fn asks(&self) -> Vec<TokenRequestReason> {
        self.asks.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl TokenSource for RotatingSource {
    async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
        self.asks.lock_recover().push(request.reason);
        if request.reason == TokenRequestReason::Current {
            let (generation, expires_at) = *self.current.lock_recover();
            return Ok(Self::token(generation, expires_at));
        }
        if self.hold_replacement {
            self.release.notified().await;
        }
        self.replacements.fetch_add(1, Ordering::SeqCst);
        let mut current = self.current.lock_recover();
        let stale_is_current = request
            .stale
            .is_some_and(|stale| stale.same_secret(&Self::token(current.0, None)));
        if stale_is_current {
            *current = (current.0 + 1, None);
        }
        Ok(Self::token(current.0, current.1))
    }
}

fn route() -> ProviderRouteIdentity {
    ProviderRouteIdentity::for_endpoint("test", "https://provider.test", "model")
}

/// Concurrent 401s on one epoch make one host call between them, and every
/// caller gets the one replacement that call produced.
#[tokio::test]
async fn concurrent_rejections_of_one_epoch_make_one_host_call() {
    let source = Arc::new(RotatingSource::new(None, true));
    let gate = Arc::new(TokenGate::new(source.clone(), "test"));
    let route = route();
    let lease = gate.current(&route).await.expect("current token");
    assert_eq!(lease.epoch, 1);

    let callers = (0..8)
        .map(|_| {
            let gate = Arc::clone(&gate);
            let lease = lease.clone();
            let route = route.clone();
            tokio::spawn(async move {
                gate.replace(&route, &lease, TokenRequestReason::Rejected)
                    .await
            })
        })
        .collect::<Vec<_>>();
    // Every caller is queued on the flight before the host answers.
    while source.asks().len() < 2 {
        tokio::task::yield_now().await;
    }
    source.release.notify_one();
    for caller in callers {
        let replaced = caller
            .await
            .expect("caller joins")
            .expect("replacement succeeds")
            .expect("a newer token");
        assert_eq!(replaced.token.secret().expose_secret(), "token-2");
        assert_eq!(replaced.epoch, 2);
    }
    assert_eq!(source.replacements.load(Ordering::SeqCst), 1);
    assert_eq!(
        source.asks(),
        vec![TokenRequestReason::Current, TokenRequestReason::Rejected]
    );
}

/// A static key answers a rejection with itself: the gate reports "nothing
/// newer" so the provider surfaces its own 401.
#[tokio::test]
async fn a_static_key_has_nothing_newer_after_a_rejection() {
    let gate = TokenGate::new(Arc::new(ProviderToken::new("sk-static")), "test");
    let route = route();
    let lease = gate.current(&route).await.expect("current token");
    let replaced = gate
        .replace(&route, &lease, TokenRequestReason::Rejected)
        .await
        .expect("static source answers");
    assert!(replaced.is_none());
    assert_eq!(gate.epoch(), 1);
}

/// A token inside the 30 s skew is replaced with one `Expiring` ask before
/// the attempt sends it.
#[tokio::test]
async fn a_token_about_to_expire_is_replaced_before_the_attempt() {
    let soon = SystemTime::now() + Duration::from_secs(5);
    let source = Arc::new(RotatingSource::new(Some(soon), false));
    let gate = TokenGate::new(source.clone(), "test");
    let lease = gate.current(&route()).await.expect("current token");
    assert_eq!(lease.token.secret().expose_secret(), "token-2");
    assert_eq!(lease.epoch, 2);
    assert_eq!(
        source.asks(),
        vec![TokenRequestReason::Current, TokenRequestReason::Expiring]
    );
}

/// A host failure reaches the attempt as its typed failure code.
#[tokio::test]
async fn a_host_reauth_failure_surfaces_as_its_typed_code() {
    #[derive(Debug)]
    struct SignedOut;
    #[async_trait::async_trait]
    impl TokenSource for SignedOut {
        async fn token(&self, _request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
            Err(TokenError::new(
                TokenErrorKind::ReauthRequired,
                "sign in to the account again",
            ))
        }
    }
    let gate = TokenGate::new(Arc::new(SignedOut), "test");
    let error = gate.current(&route()).await.expect_err("signed out");
    assert_eq!(
        error.code.as_ref().map(ToString::to_string).as_deref(),
        Some("lash:credential_reauth_required")
    );
    assert_eq!(error.kind, lash_core::ProviderFailureKind::Auth);
    assert!(!error.is_retryable());
}
