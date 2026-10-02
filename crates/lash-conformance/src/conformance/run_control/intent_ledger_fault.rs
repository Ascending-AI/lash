//! A control intent's delivery over a ledger that fails the delivery's own
//! calls: the obligation stalls, and the intent holds its session no longer.
#![expect(
    clippy::expect_used,
    reason = "law preconditions and outcomes are assertions"
)]
use super::{Fixture, ManualClock, shift};
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::testing::Script;
use lash_sansio::TurnId;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// F09 (FIG-4648): whether an intent's engine half is owed has one home, so
/// a delivery whose own ledger call keeps failing — the claim of the
/// application, the acknowledgement, or the refusal's write — stalls the
/// obligation at the attempt ceiling and the cancel holds its session no
/// longer. The intent's state was never written, so it stays pending: it
/// owes nothing while its obligation is stalled, the session shifts its
/// next run, and re-arming the obligation makes the intent owed again.
pub async fn a_store_fault_in_an_intents_delivery_stalls_its_obligation_and_never_wedges_its_session(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for call in [
        StoreOp::claim_intent_application,
        StoreOp::acknowledge_intent,
        StoreOp::refuse_intent,
    ] {
        let name = format!("intent-ledger-fault-{call:?}").replace('_', "-");
        let f = Fixture::new(prefix, &name, &host, &stores).await;
        let next = f.parts.enqueue("behind", Some("behind-run")).await;
        let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
        let id = intent.obligation_id().cloned().expect("armed");
        let (work, close) = f.control(false, false);
        // The refusal's write runs only when the engine refuses for good.
        work.0
            .permanent
            .store(call == StoreOp::refuse_intent, Ordering::SeqCst);
        // Every call of the operation answers a transient store fault.
        let script = Script::new();
        script
            .on(call)
            .from_nth(1)
            .before()
            .fail(|| crate::StoreError::Contended);
        let faulty = script.wrap("relay", Arc::clone(&f.factory));
        let clock = Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(
            f.parts.host.clock.timestamp_ms(),
        )));
        let policy = lash_core::runtime::shift::relay::RelayPolicy {
            attempt_ceiling: std::num::NonZeroU32::new(2).expect("ceiling"),
            ..Default::default()
        };
        let relay_over = |deployment: Arc<dyn crate::DeploymentStore>| {
            lash_core::runtime::shift::ControlIntentRelay::new(
                Arc::clone(&f.intents),
                deployment,
                Arc::clone(&work) as Arc<dyn crate::SessionWorkEngine>,
                Arc::clone(&close) as Arc<dyn ScopeCloseSink>,
                Arc::new(f.scope_close_relay(&close)),
                Arc::clone(&clock) as Arc<dyn crate::Clock>,
            )
            .with_policy(policy)
        };
        let relay = relay_over(faulty);
        let page = NonZeroUsize::MIN.saturating_add(63);
        // Attempt 1, the verb's own: the fault hands the obligation back, and
        // the cancel still holds its session.
        assert_eq!(
            relay.deliver_intent(&intent).await.expect("deliver"),
            ControlIntentState::Pending,
            "{call:?}"
        );
        assert_eq!(
            f.obligation(&intent).await,
            Some(ObligationState::Due),
            "{call:?}"
        );
        assert!(f.owed(intent.id).await, "{call:?}");
        assert!(f.parts.epoch().await.control_pending, "{call:?}");
        // Attempt 2 is the ceiling: the obligation stalls.
        clock.0.fetch_add(policy.backoff_ms(1), Ordering::SeqCst);
        let pass = lash_core::runtime::shift::relay::relay_due(&relay, clock.as_ref(), page)
            .await
            .expect("the ceiling pass");
        assert_eq!((pass.claimed, pass.stalled), (1, 1), "{call:?}: {pass:?}");
        let stalled = f
            .intents
            .list_stalled(None, page)
            .await
            .expect("stalled")
            .into_iter()
            .find(|stalled| stalled.id == id)
            .expect("the obligation stalled");
        assert_eq!(stalled.reason, StallReason::AttemptsExhausted, "{call:?}");
        assert_eq!(
            stalled.last_error.map(|error| error.code),
            Some(crate::StoreError::Contended.runtime_code()),
            "{call:?}: the stall row keeps the store fault's code"
        );
        // Nothing wrote the intent, and it owes nothing: the store's session
        // gate and the intent agree, and the session is not held.
        assert_eq!(
            f.intent_state(intent.id).await,
            ControlIntentState::Pending,
            "{call:?}"
        );
        assert!(!f.owed(intent.id).await, "{call:?}");
        assert!(
            !f.parts.epoch().await.control_pending,
            "{call:?}: a stalled obligation left its cancel holding the session"
        );
        assert!(
            work.0.events.lock().expect("events").contains(&"schedule"),
            "{call:?}: the session is asked to work once the obligation stalled"
        );
        let outcome = shift(&f, &runner, "after-ledger-fault").await;
        assert_eq!(outcome.stop, ShiftStop::Idle, "{call:?}");
        assert_eq!(
            f.parts.applications().await,
            vec![(next, TurnId::from("behind-run"))],
            "{call:?}: the session shifts the run behind the stalled cancel"
        );
        // An operator re-arms it once the ledger answers again: the intent is
        // owed again with no state write, and its delivery completes it.
        let relay = relay_over(Arc::clone(&f.factory));
        work.0.permanent.store(false, Ordering::SeqCst);
        assert!(
            f.intents
                .rearm(&id, clock.0.load(Ordering::SeqCst))
                .await
                .expect("rearm")
        );
        assert!(f.owed(intent.id).await, "{call:?}");
        assert!(f.parts.epoch().await.control_pending, "{call:?}");
        let pass = lash_core::runtime::shift::relay::relay_due(&relay, clock.as_ref(), page)
            .await
            .expect("re-armed pass");
        assert_eq!(pass.delivered, 1, "{call:?}: {pass:?}");
        assert!(
            matches!(
                f.intent_state(intent.id).await,
                ControlIntentState::Acknowledged { .. }
            ),
            "{call:?}"
        );
        assert!(!f.parts.epoch().await.control_pending, "{call:?}");
    }
}

/// F09 (FIG-4648): re-arming the stalled obligation of an intent the engine
/// refused returns the intent to pending through its typed state encoding,
/// so it is owed again and its next delivery completes it.
pub async fn re_arming_a_refused_intent_makes_it_owed_again_and_its_delivery_completes_it(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "rearmed-refusal", &host, &stores).await;
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
    let id = intent.obligation_id().cloned().expect("armed");
    let (work, close) = f.control(false, false);
    work.0.permanent.store(true, Ordering::SeqCst);
    let refused = f.apply(&work, &close, &intent).await;
    assert!(
        matches!(
            &refused,
            ControlIntentState::Refused { cause }
                if cause.code == crate::RuntimeErrorCode::PluginSessionManager
        ),
        "{refused:?}"
    );
    assert!(!f.owed(intent.id).await);
    assert!(!f.parts.epoch().await.control_pending);
    work.0.permanent.store(false, Ordering::SeqCst);
    assert!(
        f.intents
            .rearm(&id, f.parts.host.clock.timestamp_ms())
            .await
            .expect("rearm")
    );
    assert_eq!(f.intent_state(intent.id).await, ControlIntentState::Pending);
    assert!(f.owed(intent.id).await);
    assert!(f.parts.epoch().await.control_pending);
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(Fixture::intent_pass(&report).delivered, 1);
    assert!(matches!(
        f.intent_state(intent.id).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        f.obligation(&intent).await,
        Some(ObligationState::Delivered)
    );
    assert!(!f.parts.epoch().await.control_pending);
}
