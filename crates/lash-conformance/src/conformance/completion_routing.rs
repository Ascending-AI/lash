//! FIG-3429 item 5 — completion-routing pairwise refusal.
//!
//! A retained tool child records *how its completion was routed* at admission
//! ([`ToolChildCompletionRouting`]), the host it lands on at recovery decides
//! whether it can mint the key that routing needs
//! ([`AwaitEventResolver::prepare_completion_key`]), and the registry the key
//! was minted in decides whether a *presented* key is one of its own (the
//! HMAC issuer check inside `resolve`/`peek`/`await`). Those are three
//! pairwise boundaries: every (routing mode × host kind × registry identity)
//! pair has a ruling, and every incompatible pair refuses **before** a key is
//! issued.
//!
//! The [`IssuanceSpy`] records every key the issuance seam hands out, so
//! "refused before any key is issued" is a count, not a vibe. A refusal that
//! still minted — the C1 finding this oracle names — is visible here as a
//! nonzero issue count on a refused cell.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_sansio::sync::MutexExt;
use tokio_util::sync::CancellationToken;

use crate::runtime::effect::ToolChildCompletionRouting;
use crate::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    EffectHost, ExecutionScope, Resolution, ResolveOutcome, RuntimeError, RuntimeErrorCode,
};

/// Records every key issued through a resolver boundary, plus every direct
/// mint call, so a refused cell can prove nothing was issued on its behalf.
struct IssuanceSpy {
    inner: Arc<dyn AwaitEventResolver>,
    key_calls: AtomicUsize,
    issued: Mutex<Vec<AwaitEventKey>>,
}

impl IssuanceSpy {
    fn new(inner: Arc<dyn AwaitEventResolver>) -> Self {
        Self {
            inner,
            key_calls: AtomicUsize::new(0),
            issued: Mutex::new(Vec::new()),
        }
    }

    /// The durable-authority identity this resolver claims, if any. A host
    /// that names one asserts its keys outlive the minting process; a host
    /// that names none must never be treated as a durable routing target.
    fn authority(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    fn key_calls(&self) -> usize {
        self.key_calls.load(Ordering::Relaxed)
    }

    fn issued(&self) -> Vec<AwaitEventKey> {
        self.issued.lock_recover().clone()
    }

    async fn prepare(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        let answer = self
            .inner
            .prepare_completion_key(scope, wait, may_defer)
            .await?;
        if let CompletionKeyPreparation::Issued(key) = &answer {
            self.issued.lock_recover().push(key.clone());
        }
        Ok(answer)
    }

    async fn mint(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.key_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek(&self, key: &AwaitEventKey) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn wait(
        &self,
        key: &AwaitEventKey,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inner
            .await_await_event(key, CancellationToken::new(), deadline)
            .await
    }
}

/// The routing facts a retained child can record, mapped to the `may_defer`
/// question the issuance gate is actually asked: `Inline` never needs a key;
/// `Durable` always does.
fn modes() -> [(ToolChildCompletionRouting, bool); 2] {
    [
        (ToolChildCompletionRouting::Inline, false),
        (ToolChildCompletionRouting::Durable, true),
    ]
}

/// The host the pairwise matrix covers: whatever the fixture's `make`
/// produces. Every host journals (ADR 0102, D1), so whether it issues a
/// deferring key follows from the durable authority it reports.
struct HostArm {
    name: &'static str,
    resolver: IssuanceSpy,
}

fn preparation_name(answer: &CompletionKeyPreparation) -> &'static str {
    match answer {
        CompletionKeyPreparation::Issued(_) => "issued",
        CompletionKeyPreparation::NotNeeded => "not-needed",
        CompletionKeyPreparation::Unsupported => "unsupported",
    }
}

fn assert_no_issuance(spy: &IssuanceSpy, cell: &str) {
    assert_eq!(
        spy.issued(),
        Vec::new(),
        "{cell}: the refused pair issued a key anyway — the C1 leak class"
    );
    assert_eq!(
        spy.key_calls(),
        0,
        "{cell}: the refused pair reached the mint path"
    );
}

/// The registry-identity half of the matrix: a key minted under one registry
/// must resolve under that registry and be refused — at resolve, peek, and
/// wait alike — under a foreign one, which itself never issues a substitute.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_foreign_registry_refuses(
    issued_key: &AwaitEventKey,
    foreign: &IssuanceSpy,
    cell: &str,
) {
    let resolution = Resolution::Ok(serde_json::json!({"cell": cell}));
    assert_eq!(
        foreign
            .resolve(issued_key, resolution.clone())
            .await
            .expect("foreign resolve answers rather than hanging"),
        ResolveOutcome::UnknownOrRevoked,
        "{cell}: a foreign registry accepted a key it did not mint"
    );
    let peek = foreign
        .peek(issued_key)
        .await
        .expect_err("foreign peek refuses a foreign-minted key");
    assert_eq!(
        peek.code,
        RuntimeErrorCode::AwaitEventUnknownOrRevoked,
        "{cell}: foreign peek must refuse by name"
    );
    let waited = foreign
        .wait(issued_key, Some(Instant::now()))
        .await
        .expect_err("foreign wait refuses a foreign-minted key");
    assert_eq!(
        waited.code,
        RuntimeErrorCode::AwaitEventUnknownOrRevoked,
        "{cell}: foreign wait must refuse by name, before parking a waiter"
    );
    assert_eq!(
        foreign.key_calls(),
        0,
        "{cell}: the foreign registry minted a substitute key — a second \
         dispatch of an opaque tool body is exactly what the refusal exists \
         to prevent"
    );
    assert!(
        foreign.issued().is_empty(),
        "{cell}: the foreign registry issued while refusing"
    );
}

/// One host's full row of the pairwise matrix: every routing mode's
/// preparation ruling, then — for every cell that issues — the registry
/// checks that make the issued key an authority and not just a string.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn exercise_host_arm(arm: &HostArm, second: &IssuanceSpy, foreign: &IssuanceSpy) {
    let spy = &arm.resolver;
    let scope = ExecutionScope::turn(
        format!("completion-routing-{}", arm.name),
        "completion-routing-turn",
    );

    // The capability signal must be truthful before it can be a gate: a host
    // that names a durable authority must issue deferring keys, and one that
    // names none must refuse them.
    let authority = spy.authority();
    for (mode, may_defer) in modes() {
        let cell = format!("({mode:?} × {})", arm.name);
        // One wait identity per cell: two deferring modes on one host must
        // not share a key, or the second cell would read the first's terminal.
        let wait = AwaitEventWaitIdentity::tool_completion(format!("call-{}-{mode:?}", arm.name));
        let issued_before = spy.issued().len();
        let answer = spy
            .prepare(&scope, wait.clone(), may_defer)
            .await
            .expect("preparation answers capability, never an untyped failure");
        let answered = preparation_name(&answer);
        if !may_defer {
            assert!(
                matches!(answer, CompletionKeyPreparation::NotNeeded),
                "{cell}: an inline child must never be issued a completion key, \
                 answered {answered}"
            );
            assert_no_issuance(spy, &cell);
            continue;
        }
        let expects_issue = authority.is_some();
        assert_eq!(
            matches!(answer, CompletionKeyPreparation::Issued(_)),
            expects_issue,
            "{cell}: preparation answered {answered}; the pairwise ruling \
             for this host is {}",
            if expects_issue { "issued" } else { "refused" },
        );
        let CompletionKeyPreparation::Issued(key) = answer else {
            assert_no_issuance(spy, &cell);
            continue;
        };
        let cell_issued = &spy.issued()[issued_before..];
        assert_eq!(
            cell_issued,
            std::slice::from_ref(&key),
            "{cell}: exactly one key reaches the caller per issued cell"
        );

        // Registry identity — minting arm: the key authenticates, is
        // byte-identical on re-mint (the registry is a fact, not a call), and
        // accepts the completion it was minted to carry.
        let resolution = Resolution::Ok(serde_json::json!({"cell": cell}));
        assert_eq!(
            spy.resolve(&key, resolution.clone())
                .await
                .expect("the minting registry resolves its own key"),
            ResolveOutcome::Accepted,
            "{cell}: the minting registry refused its own key"
        );
        assert_eq!(
            spy.peek(&key)
                .await
                .expect("the minting registry reads its own key"),
            Some(resolution),
            "{cell}: the minted key routed the completion back"
        );
        let reminted = spy
            .mint(&scope, wait.clone())
            .await
            .expect("re-minting under one registry is deterministic");
        assert_eq!(
            reminted, key,
            "{cell}: one registry mints one key for one (scope, wait) — a \
             second derivation is the same identity, not a second issuance"
        );

        // Registry identity — foreign arm: a key presented under a registry
        // that did not mint it is refused at every read path, and the foreign
        // registry issues nothing in its place.
        assert_foreign_registry_refuses(&key, foreign, &cell).await;

        // Registry identity — second handle over the same substrate: a
        // durable-authority host authenticates its keys from any handle,
        // which is what "durable routing" means.
        assert_eq!(
            second
                .peek(&key)
                .await
                .expect("a durable registry reads its own key from any handle"),
            Some(Resolution::Ok(serde_json::json!({"cell": cell}))),
            "{cell}: a second handle over one durable substrate must see the \
             same registry"
        );
    }
}

/// Every (routing mode × host kind × registry identity) pair refuses
/// incompatibly or routes correctly — and every refusal happens before the
/// issuance seam produces a key.
///
/// The pairwise ruling table, reviewed cell by cell:
///
/// | mode | host | minting registry | foreign registry |
/// |---|---|---|---|
/// | `Inline` | any | `NotNeeded`, zero issuance | no key exists to present |
/// | `Durable` | tier (durable authority) | `Issued`; resolves on the minting registry and on a second handle over the same substrate | key refused |
/// | `Durable` | tier (no durable authority) | `Unsupported`, zero issuance | n/a |
///
/// `make` returns fresh hosts over the tier's substrate; `make_foreign` a host
/// over a different substrate, whose registry did not mint the tier's keys.
pub async fn completion_routing_pairwise_refusal<F, G>(make: F, make_foreign: G)
where
    F: Fn() -> Arc<dyn EffectHost>,
    G: Fn() -> Arc<dyn EffectHost>,
{
    let first = make();
    let second = make();
    crate::assert_fresh_instances(&first, &second, "completion_routing");

    // The registry-identity axis a durable substrate makes interesting: a
    // second handle over the same substrate is the same registry, and a host
    // over another substrate is a foreign one. The row runs once — re-running
    // would re-resolve the same key.
    let second_spy = IssuanceSpy::new(second as Arc<dyn AwaitEventResolver>);
    let foreign_spy = IssuanceSpy::new(make_foreign() as Arc<dyn AwaitEventResolver>);
    let tier_arm = HostArm {
        name: "tier",
        resolver: IssuanceSpy::new(first as Arc<dyn AwaitEventResolver>),
    };
    exercise_host_arm(&tier_arm, &second_spy, &foreign_spy).await;
}
