//! Released claims drop contended session and process projections.
use super::*;

#[tokio::test]
async fn a_contended_reader_releases_the_projection_when_the_claim_ends() {
    let cx = ActorContext::unavailable();
    let head = Arc::new(
        lash_core_store::session_state::RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        )),
    );
    let witness = Arc::downgrade(&head);
    *cx.inner.session_projection.lock().await.unwrap() =
        Some(super::super::owner_state::OwnerState::loaded(
            cx.actor().clone(),
            cx.epoch(),
            lash_durable::StateRevision(0),
            super::super::owner_state::SessionProjection {
                head,
                prompt: Default::default(),
                composition: None,
                fleet: lash_core_store::store::FleetFormat::current(),
                turn: None,
                phase: None,
            },
        ));
    let claim = cx.session_claim_guard();
    // This escaped reader owns the lock when another task ends the claim.
    let reader = cx.inner.session_projection.lock().await.unwrap();
    crate::task::spawn(async move {
        drop(claim);
    })
    .await
    .unwrap();
    drop(reader);
    assert!(
        witness.upgrade().is_none(),
        "the escaped context retained the released head"
    );
    assert!(cx.session_projection().await.is_err());
}

/// LO-EVICT: release while a process commit holds the projection must not
/// leave an escaped context retaining the claim's continuation.
#[tokio::test]
async fn a_contended_process_projection_drops_when_the_claim_ends() {
    let cx = ActorContext::unavailable();
    *cx.inner.process_projection.lock().await.unwrap() =
        Some(super::super::owner_state::OwnerState::loaded(
            cx.actor().clone(),
            cx.epoch(),
            lash_durable::StateRevision(0),
            super::super::process::continuation::Continuation::default(),
        ));
    let claim = cx.session_claim_guard();
    let reader = cx.inner.process_projection.lock().await.unwrap();
    crate::task::spawn(async move {
        drop(claim);
    })
    .await
    .unwrap();
    drop(reader);
    assert!(
        !cx.inner
            .process_projection
            .retains_value_for_testing()
            .await
    );
    assert!(cx.process_continuation().await.is_err());
}
