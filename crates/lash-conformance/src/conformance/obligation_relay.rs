//! Store→engine delivery obligations and the recovery leader lease (ADR 0109
//! §1): the laws every store set owes over its obligation ledgers and its
//! lease, whatever engine delivers.
//!
//! The relay laws run the kernel's relay ([`relay_due`], [`deliver_now`]) over
//! a backend's `session_delete` ledger — the `session_meta` row of a session
//! the fixture creates — with a scripted delivery, on a test clock, so each
//! law pins one rule of the settlement: the claim token fences settlement,
//! a retryable failure backs off, the ceiling stalls, a refused or
//! undecodable delivery stalls at once without failing the page, and only
//! an explicit re-arm puts a stalled obligation back.
//!
//! The lease laws run on the database clock with short durations: a single
//! holder, failover after expiry, preemption by a higher rank only after the
//! holder's minimum tenure, and resignation.

use crate::conformance::DeploymentViewExt as _;
use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::ClockWallTime as _;
use lash_core::runtime::drive::relay::{
    DeliveryFailure, ObligationRelay, RelayPolicy, RelayVerdict, deliver_now, relay_due,
};
use lash_core::store::{
    HolderId, LeaseClaim, LeaseName, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationState, RecoveryLeaderStore, SettleOutcome, StallReason,
};
use lash_core::testing::TestClock;
use lash_sansio::SessionId;

/// What one relay law runs over: a backend's store set, fresh per law.
pub struct ObligationLawFixture {
    pub stores: Arc<dyn crate::StoreSet>,
    /// Distinguishes this law's rows from every other law's on a shared
    /// database.
    pub prefix: String,
}

const T0: u64 = 1_000_000;

fn policy(ceiling: u32) -> RelayPolicy {
    RelayPolicy {
        base_backoff_ms: 1_000,
        max_backoff_ms: 4_000,
        attempt_ceiling: NonZeroU32::new(ceiling).unwrap_or(NonZeroU32::MIN),
        claim_ttl_ms: 60_000,
    }
}

fn page(limit: usize) -> NonZeroUsize {
    NonZeroUsize::new(limit).unwrap_or(NonZeroUsize::MIN)
}

/// A delivery that answers each key from a script, recording every attempt.
struct ScriptedRelay {
    ledger: Arc<dyn ObligationLedger>,
    policy: RelayPolicy,
    answers: Mutex<BTreeMap<String, DeliveryFailure>>,
    attempts: Mutex<Vec<String>>,
}

impl ScriptedRelay {
    fn new(ledger: Arc<dyn ObligationLedger>, policy: RelayPolicy) -> Self {
        Self {
            ledger,
            policy,
            answers: Mutex::new(BTreeMap::new()),
            attempts: Mutex::new(Vec::new()),
        }
    }

    fn fail(&self, key: &ObligationKey, failure: DeliveryFailure) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key_label(key), failure);
    }

    fn heal(&self, key: &ObligationKey) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key_label(key));
    }

    fn attempts_for(&self, key: &ObligationKey) -> usize {
        let label = key_label(key);
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|attempt| **attempt == label)
            .count()
    }
}

fn key_label(key: &ObligationKey) -> String {
    format!("{key:?}")
}

#[async_trait::async_trait]
impl ObligationRelay for ScriptedRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(
        &self,
        _id: &ObligationId,
        key: &ObligationKey,
        _attempt: u32,
    ) -> Result<(), DeliveryFailure> {
        let label = key_label(key);
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(label.clone());
        match self
            .answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&label)
        {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a session the law cannot create is a harness defect"
)]
async fn armed_session(
    fixture: &ObligationLawFixture,
    ledger: &dyn ObligationLedger,
    name: &str,
    now_ms: u64,
) -> (ObligationKey, ObligationId) {
    let session_id = SessionId::from(format!("{}-{name}", fixture.prefix));
    fixture
        .stores
        .session_store_factory()
        .admit_view(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await
        .expect("create the session whose catalog row carries the obligation");
    let key = ObligationKey::SessionDelete { session_id };
    let id = ledger
        .arm(&key, now_ms)
        .await
        .expect("arm the session's obligation")
        .expect("a row that owes nothing arms");
    (key, id)
}

fn ledger_of(fixture: &ObligationLawFixture) -> Arc<dyn ObligationLedger> {
    fixture
        .stores
        .obligation_ledger(ObligationKind::SessionDelete)
}

/// Registrations arm their own start rows and a bounded due pass reaches
/// every row without scanning the process registry.
#[expect(
    clippy::expect_used,
    reason = "conformance law: every store result is asserted"
)]
pub async fn registered_processes_are_claimed_through_every_obligation_page(
    fixture: ObligationLawFixture,
) {
    use crate::{Lifetime, ProcessInput, ProcessProvenance, ProcessRegistration};
    use std::collections::BTreeSet;

    let registry = fixture.stores.process_registry();
    let ledger = fixture
        .stores
        .obligation_ledger(ObligationKind::ProcessStart);
    let mut registered = BTreeSet::new();
    for index in 0..260 {
        let process = registry
            .register_process(
                ProcessRegistration::new(
                    ProcessInput::Engine {
                        kind: "process-start-law".to_owned(),
                        payload: serde_json::json!({"index": index}),
                    },
                    ProcessProvenance::host(),
                    Lifetime::Detached,
                )
                .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new(
                    "process-start-law-env",
                ))),
            )
            .await
            .expect("register a process");
        registered.insert(process.id);
    }

    let mut delivered = BTreeSet::new();
    let mut pages = 0;
    let now = (i64::MAX / 4) as u64;
    loop {
        let claims = ledger
            .claim_due(now, 60_000, page(17))
            .await
            .expect("claim one bounded page");
        if claims.is_empty() {
            break;
        }
        pages += 1;
        assert!(claims.len() <= 17);
        for claim in claims {
            let ObligationKey::ProcessStart { process_id } = claim.key.expect("decode start")
            else {
                panic!("a process-start ledger returned another key");
            };
            assert!(registered.contains(&process_id));
            assert_eq!(claim.attempts, 1);
            assert!(delivered.insert(process_id));
            assert_eq!(
                ledger
                    .settle(
                        &claim.id,
                        &claim.token,
                        ObligationSettlement::Delivered,
                        now
                    )
                    .await
                    .expect("settle start"),
                SettleOutcome::Applied
            );
        }
    }
    assert!(pages > 1, "the law must exercise more than one page");
    assert_eq!(delivered, registered);
}

/// Arming touches only a row that owes nothing, and a missing row arms
/// nothing.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn arming_takes_only_an_idle_row(fixture: ObligationLawFixture) {
    let ledger = ledger_of(&fixture);
    let (key, id) = armed_session(&fixture, ledger.as_ref(), "arm", T0).await;
    assert_eq!(
        ledger.state(&id).await.expect("read the armed state"),
        Some(ObligationState::Due)
    );
    assert_eq!(
        ledger.arm(&key, T0).await.expect("re-arm"),
        None,
        "a row that already owes an obligation is not armed again"
    );
    let missing = ObligationKey::SessionDelete {
        session_id: SessionId::from(format!("{}-never-created", fixture.prefix)),
    };
    assert_eq!(
        ledger.arm(&missing, T0).await.expect("arm a missing row"),
        None
    );
    assert_eq!(
        ledger
            .state(&ObligationId::new("session_delete:never-minted"))
            .await
            .expect("read an unknown id"),
        None
    );
}

/// A claim's token fences its settlement: once a lapsed claim is retaken,
/// the first claimant's settle is refused and the second one's applies.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn the_claim_token_fences_settlement(fixture: ObligationLawFixture) {
    let ledger = ledger_of(&fixture);
    let (key, id) = armed_session(&fixture, ledger.as_ref(), "fence", T0).await;
    let first = ledger
        .claim_due(T0, 1_000, page(64))
        .await
        .expect("first claim")
        .into_iter()
        .find(|claimed| claimed.id == id)
        .expect("the due obligation is claimed");
    assert_eq!(first.attempts, 1);
    assert_eq!(first.key.as_ref().ok(), Some(&key));
    assert!(
        !ledger
            .claim_due(T0 + 999, 1_000, page(64))
            .await
            .expect("claim within the first claim's window")
            .iter()
            .any(|claimed| claimed.id == id),
        "a live claim is not retaken"
    );
    let second = ledger
        .claim_due(T0 + 1_000, 1_000, page(64))
        .await
        .expect("retake the lapsed claim")
        .into_iter()
        .find(|claimed| claimed.id == id)
        .expect("a lapsed claim is due again");
    assert_eq!(second.attempts, 2);
    assert_ne!(second.token, first.token);
    assert_eq!(
        ledger
            .settle(
                &id,
                &first.token,
                ObligationSettlement::Delivered,
                T0 + 1_001
            )
            .await
            .expect("stale settle"),
        SettleOutcome::ClaimLost,
        "the superseded claimant cannot settle"
    );
    assert_eq!(
        ledger
            .state(&id)
            .await
            .expect("state after the stale settle"),
        Some(ObligationState::Claimed)
    );
    assert_eq!(
        ledger
            .settle(
                &id,
                &second.token,
                ObligationSettlement::Delivered,
                T0 + 1_002
            )
            .await
            .expect("live settle"),
        SettleOutcome::Applied
    );
    assert_eq!(
        ledger.state(&id).await.expect("state after delivery"),
        Some(ObligationState::Delivered)
    );
    assert_eq!(
        ledger
            .settle(
                &id,
                &second.token,
                ObligationSettlement::Delivered,
                T0 + 1_003
            )
            .await
            .expect("settle a settled claim"),
        SettleOutcome::ClaimLost,
        "a settled claim does not settle twice"
    );
}

/// A retryable failure hands the claim back due after the capped
/// exponential backoff; the relay does not touch it before then.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn a_retryable_failure_backs_off(fixture: ObligationLawFixture) {
    let ledger = ledger_of(&fixture);
    let clock = TestClock::new(T0);
    let (key, id) = armed_session(&fixture, ledger.as_ref(), "backoff", T0).await;
    let relay = ScriptedRelay::new(Arc::clone(&ledger), policy(16));
    relay.fail(
        &key,
        DeliveryFailure::Retryable("engine unreachable".to_owned()),
    );
    // Attempt n is retried after min(1 s · 2^(n-1), 4 s).
    let mut due = T0;
    for (attempt, backoff) in [(1_usize, 1_000_u64), (2, 2_000), (3, 4_000), (4, 4_000)] {
        clock.set(due.saturating_sub(1));
        if attempt > 1 {
            relay_due(&relay, &clock, page(64))
                .await
                .expect("pass before the backoff elapsed");
            assert_eq!(
                relay.attempts_for(&key),
                attempt - 1,
                "attempt {attempt} must wait for its backoff"
            );
        }
        clock.set(due);
        relay_due(&relay, &clock, page(64))
            .await
            .expect("pass once due");
        assert_eq!(relay.attempts_for(&key), attempt);
        assert_eq!(
            ledger.state(&id).await.expect("state after a retry"),
            Some(ObligationState::Due)
        );
        due += backoff;
    }
    relay.heal(&key);
    clock.set(due);
    relay_due(&relay, &clock, page(64))
        .await
        .expect("pass after the engine heals");
    assert_eq!(
        ledger.state(&id).await.expect("state after delivery"),
        Some(ObligationState::Delivered)
    );
}

/// At the attempt ceiling a retryable failure stalls with
/// `attempts_exhausted`, is listed and counted, and is never claimed again.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn the_attempt_ceiling_stalls(fixture: ObligationLawFixture) {
    let ledger = ledger_of(&fixture);
    let clock = TestClock::new(T0);
    let (key, id) = armed_session(&fixture, ledger.as_ref(), "ceiling", T0).await;
    let relay = ScriptedRelay::new(Arc::clone(&ledger), policy(3));
    relay.fail(
        &key,
        DeliveryFailure::Retryable("still unreachable".to_owned()),
    );
    let before = ledger.count_stalled().await.expect("count before");
    for _ in 0..3 {
        relay_due(&relay, &clock, page(64)).await.expect("pass");
        clock.advance(4_000);
    }
    assert_eq!(relay.attempts_for(&key), 3);
    assert_eq!(
        ledger.state(&id).await.expect("state at the ceiling"),
        Some(ObligationState::Stalled)
    );
    let stalled = ledger
        .list_stalled(None, page(1_000))
        .await
        .expect("list stalled")
        .into_iter()
        .find(|stalled| stalled.id == id)
        .expect("the stalled obligation is listed");
    assert_eq!(stalled.reason, StallReason::AttemptsExhausted);
    assert_eq!(stalled.attempts, 3);
    assert_eq!(stalled.kind, ObligationKind::SessionDelete);
    assert_eq!(stalled.key.as_ref().ok(), Some(&key));
    assert_eq!(stalled.last_error.as_deref(), Some("still unreachable"));
    assert_eq!(
        ledger.count_stalled().await.expect("count after"),
        before + 1
    );
    clock.advance(3_600_000);
    relay_due(&relay, &clock, page(64))
        .await
        .expect("later pass");
    assert_eq!(
        relay.attempts_for(&key),
        3,
        "a stalled obligation is never retried on its own"
    );
}

/// A refused or undecodable delivery stalls in the pass that claims it, and
/// the rows behind it on the same page are still delivered.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn a_refused_or_undecodable_row_stalls_without_failing_the_page(
    fixture: ObligationLawFixture,
) {
    let ledger = ledger_of(&fixture);
    let clock = TestClock::new(T0);
    let (refused, refused_id) = armed_session(&fixture, ledger.as_ref(), "page-refused", T0).await;
    let (poison, poison_id) = armed_session(&fixture, ledger.as_ref(), "page-poison", T0).await;
    let (_, healthy_id) = armed_session(&fixture, ledger.as_ref(), "page-healthy", T0).await;
    let relay = ScriptedRelay::new(Arc::clone(&ledger), policy(16));
    relay.fail(&refused, DeliveryFailure::Refused("target gone".to_owned()));
    relay.fail(
        &poison,
        DeliveryFailure::Undecodable("payload from a newer build".to_owned()),
    );
    let pass = relay_due(&relay, &clock, page(64)).await.expect("one pass");
    assert!(pass.claimed >= 3, "the pass claims every due row: {pass:?}");
    assert!(pass.stalled >= 2 && pass.delivered >= 1, "{pass:?}");
    assert_eq!(
        ledger.state(&healthy_id).await.expect("healthy state"),
        Some(ObligationState::Delivered)
    );
    let stalled = ledger
        .list_stalled(None, page(1_000))
        .await
        .expect("list stalled");
    let reason = |id: &ObligationId| {
        stalled
            .iter()
            .find(|stalled| &stalled.id == id)
            .map(|stalled| stalled.reason)
    };
    assert_eq!(reason(&refused_id), Some(StallReason::Refused));
    assert_eq!(reason(&poison_id), Some(StallReason::Undecodable));
}

/// ADR 0115 §5: a cleanup row whose referrer kind a later build wrote. The
/// backend writes it due, as that build would, as obligation `id` with
/// referrer kind `label`, in a store that owes no other cleanup. Reading it
/// is refused `Incompatible(UnknownVocabulary)`; the relay stalls it
/// `undecodable` in the pass that claims it, without a delivery; and nothing
/// counts it as absent: it stays stalled, listed and refused typed, and no
/// later pass takes it again.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn an_unknown_referrer_kind_is_refused_typed_and_stalled(
    stores: Arc<dyn crate::StoreSet>,
    id: ObligationId,
    label: &str,
) {
    let typed = |error: &lash_core::store::StoreError| {
        matches!(
            error,
            lash_core::store::StoreError::Incompatible {
                refusal: lash_core::compat::CompatRefusal::UnknownVocabulary { label: found, .. }
            } if found == label
        )
    };
    let cleanup = stores.artifact_cleanup();
    let ledger = stores.obligation_ledger(ObligationKind::ArtifactCleanup);
    let refused = cleanup
        .load_cleanup(&id)
        .await
        .expect_err("a later build's kind is refused, never read as absent");
    assert!(typed(&refused), "the refusal is typed: {refused:?}");
    let before = ledger.count_stalled().await.expect("count before");

    let clock = TestClock::new(T0);
    let relay = ScriptedRelay::new(Arc::clone(&ledger), policy(16));
    let pass = relay_due(&relay, &clock, page(64)).await.expect("one pass");
    assert_eq!(
        (pass.claimed, pass.stalled, pass.delivered),
        (1, 1, 0),
        "{pass:?}"
    );
    assert!(
        relay
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a row this build cannot name is never delivered"
    );
    let stalled = ledger
        .list_stalled(None, page(1_000))
        .await
        .expect("list stalled")
        .into_iter()
        .find(|stalled| stalled.id == id)
        .expect("the stalled row is listed");
    assert_eq!(stalled.kind, ObligationKind::ArtifactCleanup);
    assert_eq!(stalled.reason, StallReason::Undecodable);
    let detail = stalled
        .key
        .expect_err("the key names a kind this build does not know")
        .detail;
    assert!(
        detail.contains(&format!("label `{label}` is unknown to this build")),
        "the stall names the typed refusal: {detail}"
    );
    assert_eq!(stalled.last_error.as_deref(), Some(detail.as_str()));
    assert_eq!(
        ledger.count_stalled().await.expect("count after"),
        before + 1
    );

    clock.advance(3_600_000);
    let later = relay_due(&relay, &clock, page(64))
        .await
        .expect("later pass");
    assert_eq!(later.claimed, 0, "a stalled row waits for an operator");
    assert_eq!(
        ledger.state(&id).await.expect("state after"),
        Some(ObligationState::Stalled)
    );
    let refused = cleanup
        .load_cleanup(&id)
        .await
        .expect_err("the stalled row is still refused, never absent");
    assert!(typed(&refused), "the refusal is typed: {refused:?}");
}

/// Only an explicit re-arm puts a stalled obligation back: due now, its
/// attempts reset, and a second re-arm of a live obligation does nothing.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn a_rearm_returns_a_stalled_obligation_to_due(fixture: ObligationLawFixture) {
    let ledger = ledger_of(&fixture);
    let clock = TestClock::new(T0);
    let (key, id) = armed_session(&fixture, ledger.as_ref(), "rearm", T0).await;
    let relay = ScriptedRelay::new(Arc::clone(&ledger), policy(16));
    relay.fail(&key, DeliveryFailure::Refused("refused once".to_owned()));
    relay_due(&relay, &clock, page(64)).await.expect("stall it");
    assert_eq!(
        ledger.state(&id).await.expect("stalled"),
        Some(ObligationState::Stalled)
    );
    relay.heal(&key);
    clock.advance(10);
    assert!(
        ledger
            .rearm(&id, clock.timestamp_ms())
            .await
            .expect("re-arm"),
        "a stalled obligation re-arms"
    );
    assert!(
        !ledger
            .rearm(&id, clock.timestamp_ms())
            .await
            .expect("re-arm a due obligation"),
        "only a stalled obligation re-arms"
    );
    let reclaimed = ledger
        .claim(&id, clock.timestamp_ms(), 60_000)
        .await
        .expect("claim the re-armed obligation")
        .expect("a re-armed obligation is due");
    assert_eq!(reclaimed.attempts, 1, "a re-arm resets the attempt count");
    assert_eq!(
        ledger
            .settle(
                &id,
                &reclaimed.token,
                ObligationSettlement::Delivered,
                clock.timestamp_ms()
            )
            .await
            .expect("settle"),
        SettleOutcome::Applied
    );
    assert!(
        !ledger
            .rearm(&id, clock.timestamp_ms())
            .await
            .expect("re-arm a delivered obligation"),
        "a delivered obligation does not re-arm"
    );
}

/// A producer's immediate attempt claims only a `due` obligation, whatever
/// its backoff, and finds a delivered one not due.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn immediate_delivery_takes_only_a_due_obligation(fixture: ObligationLawFixture) {
    let ledger = ledger_of(&fixture);
    let clock = TestClock::new(T0);
    let (key, id) = armed_session(&fixture, ledger.as_ref(), "immediate", T0 + 30_000).await;
    let relay = ScriptedRelay::new(Arc::clone(&ledger), policy(16));
    assert_eq!(
        deliver_now(&relay, &id, &clock).await.expect("deliver now"),
        RelayVerdict::Delivered,
        "the producer's own attempt does not wait for the due instant"
    );
    assert_eq!(relay.attempts_for(&key), 1);
    assert_eq!(
        deliver_now(&relay, &id, &clock)
            .await
            .expect("deliver again"),
        RelayVerdict::NotDue
    );
    assert_eq!(relay.attempts_for(&key), 1);
}

/// FIG-4098: the host's withdrawal of an open turn input settles its ingress
/// obligation in the same write. Nothing admits a withdrawn row, so the
/// withdrawal is the row's last delivery: whatever the obligation stood at
/// (due, claimed by a relay's ask, or stalled), it is delivered, no claim on
/// it remains, and a relay holding the old claim answers `ClaimLost`.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn withdrawing_an_open_input_delivers_its_ingress_obligation(
    fixture: ObligationLawFixture,
) {
    use lash_core::store::ingress_obligation::ingress_obligation_id;

    let session_id = SessionId::from(format!("{}-ingress-withdrawal", fixture.prefix));
    let store = crate::conformance::law_session_store(fixture.stores.as_ref(), &session_id).await;
    let ingress = fixture.stores.obligation_ledger(ObligationKind::Ingress);
    let now = fixture.stores.clock().timestamp_ms();
    let enqueue = |text: &'static str| {
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        async move {
            store
                .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                    session_id,
                    crate::TurnInputIngress::next_turn(),
                    crate::TurnInput::text(text),
                ))
                .await
                .expect("accept the law's input")
                .input_id
        }
    };

    // One open row per obligation state, each withdrawn by a different
    // host cancel: by id, in bulk, and as a suffix.
    let due = enqueue("due").await;
    let claimed = enqueue("claimed").await;
    let stalled = enqueue("stalled").await;
    let claim = |input: &crate::InputId| {
        let ingress = Arc::clone(&ingress);
        let id = ingress_obligation_id(input.as_str());
        async move {
            ingress
                .claim(&id, now, 3_600_000)
                .await
                .expect("claim the obligation")
                .expect("the obligation is due")
        }
    };
    let claimed_claim = claim(&claimed).await;
    let stall = claim(&stalled).await;
    assert_eq!(
        ingress
            .settle(
                &ingress_obligation_id(stalled.as_str()),
                &stall.token,
                ObligationSettlement::Stall {
                    reason: StallReason::Refused,
                    error: "stalled before its withdrawal".to_string(),
                },
                now,
            )
            .await
            .expect("stall the obligation"),
        SettleOutcome::Applied
    );
    for (input, expected) in [
        (&due, ObligationState::Due),
        (&claimed, ObligationState::Claimed),
        (&stalled, ObligationState::Stalled),
    ] {
        assert_eq!(
            ingress
                .state(&ingress_obligation_id(input.as_str()))
                .await
                .expect("read the obligation"),
            Some(expected),
            "{input}'s obligation before its withdrawal"
        );
    }

    assert!(
        store
            .cancel_pending_turn_input(&session_id, due.as_str())
            .await
            .expect("withdraw the due row")
            .is_cancelled()
    );
    let bulk = store
        .cancel_pending_turn_inputs(
            &session_id,
            &[crate::PendingTurnInputCancelTarget::input_id(
                claimed.as_str(),
            )],
        )
        .await
        .expect("withdraw the claimed row");
    assert!(bulk.iter().all(|receipt| receipt.outcome.is_cancelled()));
    match store
        .cancel_pending_turn_input_suffix(
            &session_id,
            &crate::PendingTurnInputCancelTarget::input_id(stalled.as_str()),
        )
        .await
        .expect("withdraw the stalled row")
    {
        crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } => {
            assert_eq!(outcomes.len(), 1);
            assert!(
                outcomes
                    .iter()
                    .all(crate::PendingTurnInputCancelOutcome::is_cancelled)
            );
        }
        other => panic!("the suffix anchor is the stalled row: {other:?}"),
    }

    for input in [&due, &claimed, &stalled] {
        let id = ingress_obligation_id(input.as_str());
        assert_eq!(
            ingress.state(&id).await.expect("read the obligation"),
            Some(ObligationState::Delivered),
            "the withdrawal settled {input}'s obligation"
        );
        assert!(
            ingress
                .claim(&id, now, 3_600_000)
                .await
                .expect("claim the settled obligation")
                .is_none(),
            "{input}'s settled obligation is not due"
        );
    }
    assert_eq!(
        ingress
            .settle(
                &ingress_obligation_id(claimed.as_str()),
                &claimed_claim.token,
                ObligationSettlement::Delivered,
                now,
            )
            .await
            .expect("settle the relay's lapsed claim"),
        SettleOutcome::ClaimLost,
        "no claim on the withdrawn row remains"
    );
}

// ---------------------------------------------------------------------------
// The recovery leader lease
// ---------------------------------------------------------------------------

/// What one lease law runs over: a backend's lease store, and a lease name
/// no other law uses.
pub struct LeaseLawFixture {
    pub store: Arc<dyn RecoveryLeaderStore>,
    pub name: String,
}

fn claim(
    fixture: &LeaseLawFixture,
    holder: &str,
    rank: i64,
    ttl_ms: u64,
    tenure_ms: u64,
) -> LeaseClaim {
    LeaseClaim {
        name: LeaseName::new(fixture.name.clone()),
        holder: HolderId::new(format!("{}:{holder}", fixture.name)),
        generation_rank: rank,
        ttl_ms,
        min_tenure_ms: tenure_ms,
    }
}

/// One holder leads at a time: a second claimant is refused while the lease
/// is live, and only the holder renews its term.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn the_lease_has_a_single_holder(fixture: LeaseLawFixture) {
    let a = claim(&fixture, "a", 0, 60_000, 0);
    let b = claim(&fixture, "b", 0, 60_000, 0);
    let first = fixture.store.acquire(&a).await.expect("a acquires");
    assert!(first.leader);
    let term = first.row.as_ref().expect("the elected row").term;
    let second = fixture.store.acquire(&b).await.expect("b tries");
    assert!(!second.leader, "a live lease has one holder");
    assert_eq!(second.row.as_ref().map(|row| &row.holder), Some(&a.holder));
    assert!(
        fixture
            .store
            .renew(&a, term)
            .await
            .expect("a renews")
            .leader,
        "the holder renews"
    );
    assert!(
        !fixture
            .store
            .renew(&b, term)
            .await
            .expect("b renews")
            .leader,
        "only the holder renews"
    );
    assert!(
        !fixture
            .store
            .renew(&a, term + 1)
            .await
            .expect("a renews a term it never held")
            .leader,
        "a renew names the term it holds"
    );
    assert!(
        fixture
            .store
            .acquire(&a)
            .await
            .expect("a acquires again")
            .leader
    );
}

/// An expired lease fails over: the next claimant takes it with the term
/// bumped, and the old holder's renew is refused.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn an_expired_lease_fails_over(fixture: LeaseLawFixture) {
    let a = claim(&fixture, "a", 0, 200, 0);
    let b = claim(&fixture, "b", 0, 60_000, 0);
    let first = fixture.store.acquire(&a).await.expect("a acquires");
    assert!(first.leader);
    let term = first.row.expect("a's row").term;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let taken = fixture.store.acquire(&b).await.expect("b takes over");
    assert!(taken.leader, "an expired lease fails over");
    assert_eq!(taken.row.as_ref().expect("b's row").term, term + 1);
    assert!(
        !fixture
            .store
            .renew(&a, term)
            .await
            .expect("a renews late")
            .leader,
        "the expired holder's renew is refused"
    );
}

/// A higher rank preempts a live holder only after the holder's minimum
/// tenure; an equal or lower rank never does.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn a_newer_generation_preempts_after_the_minimum_tenure(fixture: LeaseLawFixture) {
    let old = claim(&fixture, "old", 1, 60_000, 400);
    let peer = claim(&fixture, "peer", 1, 60_000, 400);
    let new = claim(&fixture, "new", 2, 60_000, 400);
    let elected = fixture.store.acquire(&old).await.expect("old acquires");
    assert!(elected.leader);
    let term = elected.row.expect("old's row").term;
    assert!(
        !fixture
            .store
            .acquire(&new)
            .await
            .expect("new, too early")
            .leader,
        "a higher rank waits for the holder's minimum tenure"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !fixture.store.acquire(&peer).await.expect("peer").leader,
        "an equal rank never preempts a live holder"
    );
    let preempted = fixture.store.acquire(&new).await.expect("new preempts");
    assert!(preempted.leader, "a higher rank preempts after the tenure");
    let row = preempted.row.expect("new's row");
    assert_eq!(row.generation_rank, 2);
    assert_eq!(row.term, term + 1);
    assert!(
        !fixture
            .store
            .renew(&old, term)
            .await
            .expect("old renews")
            .leader,
        "the preempted holder learns it lost at its next renew"
    );
}

/// A holder that resigns hands the lease over at once with the term bumped;
/// a resign that does not hold the lease changes nothing.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn a_resigned_lease_is_taken_at_once(fixture: LeaseLawFixture) {
    let a = claim(&fixture, "a", 0, 60_000, 0);
    let b = claim(&fixture, "b", 0, 60_000, 0);
    let term = fixture
        .store
        .acquire(&a)
        .await
        .expect("a acquires")
        .row
        .expect("a's row")
        .term;
    assert!(
        !fixture
            .store
            .resign(&b.name, &b.holder, term)
            .await
            .expect("b resigns a lease it does not hold"),
        "only the holder resigns"
    );
    assert!(
        fixture
            .store
            .resign(&a.name, &a.holder, term)
            .await
            .expect("a resigns")
    );
    assert!(
        !fixture
            .store
            .resign(&a.name, &a.holder, term)
            .await
            .expect("a resigns again")
    );
    let taken = fixture.store.acquire(&b).await.expect("b acquires");
    assert!(taken.leader, "a resigned lease is free at once");
    assert_eq!(
        taken.row.expect("b's row").term,
        term + 1,
        "a resignation keeps the term monotone"
    );
    assert!(
        !fixture
            .store
            .renew(&a, term)
            .await
            .expect("a renews")
            .leader,
        "a resigned holder cannot renew"
    );
}
