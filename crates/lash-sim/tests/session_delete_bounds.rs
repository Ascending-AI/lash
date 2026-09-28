//! The `SessionDelete` obligation's detection bounds (ADR 0109 §1.8, §4).
//!
//! A deployment's relay ticks every `T` = 10 s ±10%; each tick here is one
//! due pass of the session-delete relay on a virtual clock, at the extremes
//! of the jitter in turn. Every bound is asserted at the tick it must hold
//! by, and never before its earliest instant:
//!
//! - **Immediate.** The delete verb attempts the obligation before it
//!   returns.
//! - **Retryable failure.** Attempt `n + 1` lands in
//!   `[t_n + backoff(n), t_n + backoff(n) + T]`, and the obligation stalls at
//!   the attempt ceiling, never later than ≈ 1 h 47 min after the first
//!   attempt; a stalled delete is never attempted again until re-armed.
//! - **Lost immediate attempt.** A close whose verb died before its own
//!   attempt is claimed by `due_at + T`.
//! - **Lapsed claim.** A claim whose relay died is retaken by
//!   `claimed_at + claim_ttl + T`, never before its expiry.
//!
//! The session's cleanup is a scope-close obligation on its one root, armed
//! by the root's terminal transaction (ADR 0109 §3), left owed by a close
//! whose parent-end record the registry refused once, and delivered by hand
//! when the law releases the delete.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lash::LashCore;
use lash_core::drive::relay::{RelayPass, RelayPolicy, relay_due};
use lash_core::session_delete::SessionDeleteRelay;
use lash_core::store::{
    ObligationId, ObligationKind, ObligationSettlement, ObligationState, StallReason,
};
use lash_core::testing::TestClock;
use lash_core::{Backend, ClockWallTime, SessionId, TurnId};

const T_MIN_MS: u64 = 9_000;
const T_MAX_MS: u64 = 11_000;
const EPOCH_MS: u64 = 1_700_000_000_000;
/// The ADR's stall bound at the defaults: 16 attempts, never later.
const STALL_BOUND_MS: u64 = 107 * 60_000 + 3_000;

struct Deployment {
    /// The engine the roots run on; its server's virtual time is the clock.
    double: lash_restate_test::RestateTestBackend,
    clock: Arc<TestClock>,
    backend: Backend,
    core: LashCore,
    /// The next tick's interval, alternating the jitter's extremes.
    ticks: std::cell::Cell<u64>,
}

#[expect(
    clippy::expect_used,
    reason = "test fixture: a deployment that fails to build aborts the law"
)]
async fn deployment(turns: usize, session: &str, root: &str) -> Deployment {
    // The law's root's first parent-end record is refused once, so its
    // close step's immediate delivery fails and its scope close stays owed.
    // The refusal is layered under the engine, so the engine's own close
    // step meets it.
    let root_scope = lash_core::ScopeId::turn(session, root);
    let double = lash_restate_test::backend_with(
        0x5e55_de1e,
        lash_restate_test::ServerConfig {
            start_time_ms: EPOCH_MS,
            ..lash_restate_test::ServerConfig::default()
        },
        move |stores| {
            lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                .map_process_registry(|registry| {
                    lash_core::fail_parent_end_once(registry, root_scope)
                })
                .into_store_set()
        },
    )
    .await
    .expect("start the Restate double");
    let clock = double.test_clock();
    let backend = double.lash_backend();
    let scripts = (0..turns)
        .map(|_| {
            lash_sim::runtime_providers::runtime_script_for_text(
                lash_sim::runtime_providers::OPENAI_COMPATIBLE,
                "done",
            )
            .expect("script one text turn")
        })
        .collect::<Vec<_>>();
    let transport = Arc::new(
        lash_sim::ScriptedLlmHttpTransport::from_scripts(scripts).expect("provider scripts"),
    );
    let (provider, model, _) = lash_sim::runtime_providers::runtime_provider_components(
        lash_sim::runtime_providers::OPENAI_COMPATIBLE,
        &transport,
    )
    .expect("build the provider");
    let core = LashCore::standard_builder(backend.clone(), lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "session-delete-bounds",
            "session-delete-bounds-boot",
        ))
        .expect("build the core");
    Deployment {
        double,
        clock,
        backend,
        core,
        ticks: std::cell::Cell::new(0),
    }
}

/// A deletion's handler execution: the deployment's administration over the
/// controller a `SessionDelete` handler lent, as the engine's delete
/// workflow runs it.
struct HandlerExecution<'a> {
    administration: lash_core::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}

impl lash_core::SessionDeleteExecution for HandlerExecution<'_> {
    fn administration(&self) -> &lash_core::SessionAdministration {
        &self.administration
    }

    fn scoped<'run>(
        &'run self,
        _: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        Ok(self.scoped.clone())
    }
}

impl Deployment {
    fn now(&self) -> u64 {
        self.clock.timestamp_ms()
    }

    /// Run `attempt` over `session`'s delete context inside one
    /// `SessionDelete` handler on the deployment's engine.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: a handler that cannot open or close aborts the law"
    )]
    async fn in_delete_handler<T>(
        &self,
        session: &str,
        attempt: impl AsyncFnOnce(lash_core::SessionDeleteContext<'_>) -> T,
    ) -> T {
        let handler = self
            .double
            .open_handler(lash_core::AdmittedScope::session_delete(SessionId::from(
                session,
            )))
            .await
            .expect("open the delete handler");
        let outcome = {
            let execution = HandlerExecution {
                administration: self.core.session_administration().await,
                scoped: handler.scoped(),
            };
            attempt(
                lash_core::SessionDeleteContext::from_execution(&execution, session)
                    .expect("delete context"),
            )
            .await
        };
        handler.close().await.expect("close the delete handler");
        outcome
    }

    /// Run root `root` of `session` to its end: its terminal transaction
    /// arms the root's scope close (ADR 0109 §3), and the refused parent-end
    /// record fails the close step's immediate delivery, so the close stays
    /// owed.
    #[expect(
        clippy::expect_used,
        reason = "test fixture: a session that fails to run aborts the law"
    )]
    async fn ended_root(&self, session: &str, root: &str) -> ObligationId {
        let handle = self.core.session(session).open().await.expect("open");
        handle
            .send(lash::TurnInput::text("end a root"))
            .id(root)
            .output()
            .await
            .expect("run the root");
        drop(handle);
        // The handle answered at the root's final commit; its close step
        // runs after (FIG-3979).
        self.double
            .settle_session_drive(&SessionId::from(session))
            .await;
        let id = lash_core::store::scope_close_obligation_id(
            &SessionId::from(session),
            &TurnId::from(root),
        );
        assert_eq!(
            self.backend
                .obligation_ledger(ObligationKind::ScopeClose)
                .state(&id)
                .await
                .expect("read the scope close"),
            Some(ObligationState::Due),
            "the root's failed close left its scope close owed"
        );
        id
    }

    #[expect(clippy::expect_used, reason = "test fixture")]
    async fn deliver(&self, kind: ObligationKind, id: &ObligationId) {
        let ledger = self.backend.obligation_ledger(kind);
        let claimed = ledger
            .claim(id, self.now(), 60_000)
            .await
            .expect("claim")
            .expect("due");
        ledger
            .settle(
                id,
                &claimed.token,
                ObligationSettlement::Delivered,
                self.now(),
            )
            .await
            .expect("settle");
    }

    async fn relay(&self, policy: RelayPolicy) -> SessionDeleteRelay {
        SessionDeleteRelay::with_policy(self.core.session_administration().await, policy)
    }

    /// Advance to the next tick and run one due pass.
    #[expect(clippy::expect_used, reason = "test fixture")]
    async fn tick(&self, relay: &SessionDeleteRelay) -> RelayPass {
        let n = self.ticks.get();
        self.ticks.set(n + 1);
        self.double
            .server()
            .advance(std::time::Duration::from_millis(if n.is_multiple_of(2) {
                T_MAX_MS
            } else {
                T_MIN_MS
            }));
        relay_due(relay, self.clock.as_ref(), NonZeroUsize::MIN)
            .await
            .expect("relay pass")
    }

    #[expect(clippy::expect_used, reason = "test fixture")]
    async fn was_deleted(&self, session: &str) -> bool {
        self.core
            .session(session)
            .durable()
            .await
            .expect("durable handle")
            .was_deleted()
            .await
            .expect("read the tombstone")
    }

    #[expect(clippy::expect_used, reason = "test fixture")]
    async fn delete_obligation(&self, session: &str) -> Option<(ObligationId, ObligationState)> {
        self.backend
            .session_delete_ledger()
            .delete_obligation(&SessionId::from(session))
            .await
            .expect("read the delete obligation")
            .map(|obligation| (obligation.id, obligation.state))
    }
}

/// Immediate, retryable failure and the stall: the verb's attempt is the
/// first; each retry lands in its window; the sixteenth attempt stalls it
/// within the ADR's bound, and nothing attempts it after; a re-arm after the
/// cleanup settles deletes the session within one tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_delete_retries_within_its_windows_and_stalls_by_its_bound() {
    const SESSION: &str = "bounds-held";
    let deployment = deployment(1, SESSION, "held-root").await;
    let scope_close = deployment.ended_root(SESSION, "held-root").await;
    let policy = RelayPolicy::default();

    let first_attempt = deployment.now();
    let deletion = deployment
        .in_delete_handler(SESSION, async |context| {
            LashCore::delete_session(context).await.expect("delete")
        })
        .await;
    let lash::SessionDeletion::Closing(closing) = deletion else {
        panic!("the undelivered scope close holds the delete, got {deletion:?}");
    };
    assert!(
        matches!(closing.waiting, lash::SessionDeleteWait::Cleanup(_)),
        "the verb attempted the delete before it returned: {:?}",
        closing.waiting
    );
    let delete = closing.obligation.expect("the close armed the delete");

    let relay = deployment.relay(policy).await;
    let mut last_attempt = first_attempt;
    let mut attempts = 1_u32;
    let stalled_at = loop {
        let pass = deployment.tick(&relay).await;
        let now = deployment.now();
        if pass.claimed == 0 {
            assert!(
                now < last_attempt + policy.backoff_ms(attempts) + T_MAX_MS,
                "attempt {} is late: {now} past {last_attempt} + {} + T",
                attempts + 1,
                policy.backoff_ms(attempts)
            );
            continue;
        }
        assert!(
            now >= last_attempt + policy.backoff_ms(attempts),
            "attempt {} is early: {now} before {last_attempt} + {}",
            attempts + 1,
            policy.backoff_ms(attempts)
        );
        attempts += 1;
        last_attempt = now;
        if pass.stalled == 1 {
            break now;
        }
        assert_eq!(pass.retried, 1, "attempt {attempts}: {pass:?}");
    };
    assert_eq!(attempts, policy.attempt_ceiling.get());
    assert!(
        stalled_at - first_attempt <= STALL_BOUND_MS,
        "stalled {} ms after the first attempt, past the bound",
        stalled_at - first_attempt
    );
    let stalled = deployment
        .core
        .stalled_obligations(ObligationKind::SessionDelete, None, NonZeroUsize::MIN)
        .await
        .expect("list stalled");
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].id, delete);
    assert_eq!(stalled[0].reason, StallReason::AttemptsExhausted);
    assert_eq!(stalled[0].attempts, policy.attempt_ceiling.get());
    for _ in 0..100 {
        assert_eq!(
            deployment.tick(&relay).await.claimed,
            0,
            "a stalled delete is never attempted again"
        );
    }
    assert!(!deployment.was_deleted(SESSION).await);

    deployment
        .deliver(ObligationKind::ScopeClose, &scope_close)
        .await;
    let rearmed_at = deployment.now();
    assert!(
        deployment
            .core
            .rearm_obligation(ObligationKind::SessionDelete, &delete)
            .await
            .expect("re-arm")
    );
    let pass = deployment.tick(&relay).await;
    assert_eq!((pass.claimed, pass.claim_lost), (1, 1), "{pass:?}");
    assert!(deployment.now() - rearmed_at <= T_MAX_MS);
    assert!(deployment.was_deleted(SESSION).await);
    assert_eq!(deployment.delete_obligation(SESSION).await, None);
}

/// Lost immediate attempt: a close whose deletion died before its own
/// attempt leaves the delete due at the acknowledgement; the first tick
/// after it, within `T`, claims and delivers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_whose_verb_died_is_claimed_within_one_tick() {
    const SESSION: &str = "bounds-lost";
    let deployment = deployment(1, SESSION, "lost-root").await;
    let scope_close = deployment.ended_root(SESSION, "lost-root").await;
    deployment
        .deliver(ObligationKind::ScopeClose, &scope_close)
        .await;
    deployment
        .in_delete_handler(SESSION, async |context| {
            lash_core::session_close::close_session(&context)
                .await
                .expect("close")
                .expect("the session exists")
        })
        .await;
    let due_at = deployment.now();
    let (_, state) = deployment
        .delete_obligation(SESSION)
        .await
        .expect("the close armed the delete");
    assert_eq!(state, ObligationState::Due);

    let relay = deployment.relay(RelayPolicy::default()).await;
    let pass = deployment.tick(&relay).await;
    assert_eq!((pass.claimed, pass.claim_lost), (1, 1), "{pass:?}");
    assert!(deployment.now() - due_at <= T_MAX_MS);
    assert!(deployment.was_deleted(SESSION).await);
}

/// Lapsed claim: a claim whose relay died is not retaken before its expiry,
/// and is retaken by `claimed_at + claim_ttl + T`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lapsed_claim_is_retaken_within_its_bound() {
    const SESSION: &str = "bounds-lapsed";
    let deployment = deployment(1, SESSION, "lapsed-root").await;
    let scope_close = deployment.ended_root(SESSION, "lapsed-root").await;
    deployment
        .deliver(ObligationKind::ScopeClose, &scope_close)
        .await;
    deployment
        .in_delete_handler(SESSION, async |context| {
            lash_core::session_close::close_session(&context)
                .await
                .expect("close")
                .expect("the session exists")
        })
        .await;
    let (delete, _) = deployment
        .delete_obligation(SESSION)
        .await
        .expect("the close armed the delete");
    let policy = RelayPolicy::default();
    let claimed_at = deployment.now();
    deployment
        .backend
        .obligation_ledger(ObligationKind::SessionDelete)
        .claim(&delete, claimed_at, policy.claim_ttl_ms)
        .await
        .expect("claim")
        .expect("a relay that dies holding its claim");

    let relay = deployment.relay(policy).await;
    loop {
        let pass = deployment.tick(&relay).await;
        let now = deployment.now();
        if pass.claimed == 0 {
            assert!(
                now < claimed_at + policy.claim_ttl_ms + T_MAX_MS,
                "the lapsed claim is late at {now}"
            );
            continue;
        }
        assert!(
            now >= claimed_at + policy.claim_ttl_ms,
            "a live claim was retaken at {now}"
        );
        assert_eq!(pass.claim_lost, 1, "{pass:?}");
        break;
    }
    assert!(deployment.was_deleted(SESSION).await);
}
