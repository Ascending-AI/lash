use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// A host source that keeps one credential per route (named after the route's
/// model), rotates it on an `Expiring`/`Rejected` ask whose stale token is
/// still that route's current one, and records every ask with its route.
#[derive(Debug, Default)]
struct RoutedSource {
    generations: Mutex<HashMap<Box<str>, u32>>,
    asks: Mutex<Vec<(Box<str>, TokenRequestReason)>>,
    /// The next `Current` ask snapshots its answer, then waits for `release`.
    hold_next_current: AtomicBool,
    hold_replacements: bool,
    held: AtomicUsize,
    release: tokio::sync::Notify,
}

impl RoutedSource {
    fn token(model: &str, generation: u32) -> ProviderToken {
        ProviderToken::new(format!("{model}-{generation}"))
    }

    fn asks(&self) -> Vec<(Box<str>, TokenRequestReason)> {
        self.asks.lock_recover().clone()
    }

    fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }

    async fn until_held(&self, asks: usize) {
        for _ in 0..1_000 {
            if self.held() == asks {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{} asks reached the source, expected {asks}", self.held());
    }
}

#[async_trait::async_trait]
impl TokenSource for RoutedSource {
    async fn token(&self, request: TokenRequest<'_>) -> Result<ProviderToken, TokenError> {
        let model = request.route.model.clone();
        self.asks
            .lock_recover()
            .push((model.clone(), request.reason));
        if request.reason == TokenRequestReason::Current {
            let generation = *self
                .generations
                .lock_recover()
                .entry(model.clone())
                .or_insert(1);
            if self.hold_next_current.swap(false, Ordering::SeqCst) {
                self.held.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
            }
            return Ok(Self::token(&model, generation));
        }
        if self.hold_replacements {
            self.held.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
        }
        let mut generations = self.generations.lock_recover();
        let generation = generations.entry(model.clone()).or_insert(1);
        if request
            .stale
            .is_some_and(|stale| stale.same_secret(&Self::token(&model, *generation)))
        {
            *generation += 1;
        }
        Ok(Self::token(&model, *generation))
    }
}

fn route_for(model: &str) -> ProviderRouteIdentity {
    ProviderRouteIdentity::for_endpoint("test", "https://provider.test", model)
}

fn secret(lease: &TokenLease) -> &str {
    lease.token.secret().expose_secret()
}

/// A rejection on one route asks the source for that route, whatever token
/// another route obtained in between.
#[tokio::test]
async fn a_rejection_is_replaced_with_its_own_routes_token() {
    let source = Arc::new(RoutedSource::default());
    let gate = TokenGate::new(source.clone(), "test");
    let (a, b) = (route_for("a"), route_for("b"));
    let lease_a = gate.current(&a).await.expect("a's token");
    let lease_b = gate.current(&b).await.expect("b's token");
    assert_eq!((secret(&lease_a), secret(&lease_b)), ("a-1", "b-1"));

    let replaced = gate
        .replace(&a, &lease_a, TokenRequestReason::Rejected)
        .await
        .expect("replacement succeeds")
        .expect("a newer token");
    assert_eq!(secret(&replaced), "a-2");
    assert_eq!(
        source.asks().last(),
        Some(&("a".into(), TokenRequestReason::Rejected))
    );
    assert_eq!(
        secret(&gate.current(&b).await.expect("b's token")),
        "b-1",
        "a's replacement leaves b's credential alone"
    );
}

/// Rejections on two routes are two flights: both reach the source at once,
/// and each caller gets the replacement for its own route.
#[tokio::test]
async fn concurrent_rejections_on_two_routes_each_ask_for_their_own_route() {
    let source = Arc::new(RoutedSource {
        hold_replacements: true,
        ..RoutedSource::default()
    });
    let gate = Arc::new(TokenGate::new(source.clone(), "test"));
    let mut callers = Vec::new();
    for model in ["a", "b"] {
        let route = route_for(model);
        let lease = gate.current(&route).await.expect("current token");
        let gate = Arc::clone(&gate);
        callers.push(tokio::spawn(async move {
            gate.replace(&route, &lease, TokenRequestReason::Rejected)
                .await
        }));
    }
    source.until_held(2).await;
    source.release.notify_waiters();
    let mut replaced = Vec::new();
    for caller in callers {
        let lease = caller
            .await
            .expect("caller joins")
            .expect("replacement succeeds")
            .expect("a newer token");
        replaced.push(secret(&lease).to_owned());
    }
    assert_eq!(replaced, ["a-2", "b-2"]);
}

/// A `Current` answer the source snapshotted before a replacement, and
/// delivered after it, is not published: the replacement stays the route's
/// newest epoch, so later rejections are answered from it and never with the
/// token it replaced.
async fn a_current_answer_older_than_a_replacement_is_not_republished(reason: TokenRequestReason) {
    let source = Arc::new(RoutedSource::default());
    let gate = Arc::new(TokenGate::new(source.clone(), "test"));
    let route = route_for("a");
    let first = gate.current(&route).await.expect("current token");
    assert_eq!((secret(&first), first.epoch), ("a-1", 1));

    source.hold_next_current.store(true, Ordering::SeqCst);
    let delayed = tokio::spawn({
        let (gate, route) = (Arc::clone(&gate), route.clone());
        async move { gate.current(&route).await }
    });
    source.until_held(1).await;

    let second = gate
        .replace(&route, &first, reason)
        .await
        .expect("replacement succeeds")
        .expect("a newer token");
    assert_eq!((secret(&second), second.epoch), ("a-2", 2));

    source.release.notify_waiters();
    let delayed = delayed.await.expect("caller joins").expect("current token");
    assert_eq!((secret(&delayed), delayed.epoch), ("a-2", 2));

    let replacement_asks = || {
        source
            .asks()
            .iter()
            .filter(|(_, reason)| *reason != TokenRequestReason::Current)
            .count()
    };
    // A rejection queued with the replaced token takes the replacement.
    let queued = gate
        .replace(&route, &first, TokenRequestReason::Rejected)
        .await
        .expect("replacement succeeds")
        .expect("the replacement");
    assert_eq!((secret(&queued), queued.epoch), ("a-2", 2));
    assert_eq!(replacement_asks(), 1);

    // A rejection of the replacement asks the source and never gets a-1 back.
    let third = gate
        .replace(&route, &second, TokenRequestReason::Rejected)
        .await
        .expect("replacement succeeds")
        .expect("a newer token");
    assert_eq!((secret(&third), third.epoch), ("a-3", 3));
    assert_eq!(replacement_asks(), 2);
}

#[tokio::test]
async fn a_current_answer_older_than_a_rejection_replacement_is_not_republished() {
    a_current_answer_older_than_a_replacement_is_not_republished(TokenRequestReason::Rejected)
        .await;
}

#[tokio::test]
async fn a_current_answer_older_than_an_expiry_replacement_is_not_republished() {
    a_current_answer_older_than_a_replacement_is_not_republished(TokenRequestReason::Expiring)
        .await;
}
