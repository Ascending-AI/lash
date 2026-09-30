#![expect(clippy::expect_used, reason = "conformance fixture assertions")]
use super::*;

/// A moved head refuses the real turn commit; accounting is already detached.
pub async fn refused_superseded_keeps_each_paid_call_once(tier: &UsageAccountingTier) {
    let mut world = World::new(tier, "refused-superseded-spend", Script::completed(2));
    world.supersede = true;
    let error = world
        .run()
        .await
        .expect_err("the advanced head refuses this root");
    assert_eq!(error.code, crate::RuntimeErrorCode::StoreCommitSuperseded);
    let terminal = tier
        .stores
        .session_store_factory()
        .root_terminal(&world.session_id, &world.turn_id())
        .await
        .expect("terminal")
        .expect("refused root ended");
    assert!(matches!(
        terminal.cause,
        crate::store::RootTerminalCause::Refused {
            code: crate::RuntimeErrorCode::StoreCommitSuperseded,
            ..
        }
    ));
    assert_each_returned_attempt_once(&world, 0, "refused superseded").await;
    assert_eq!(world.facts().await.len(), 2);
    assert_eq!(world.invocations(), 2);
}

async fn paid_unfinished(tier: &UsageAccountingTier, label: &str) -> World {
    let world = World::new(tier, label, Script::completed(3));
    let (report, _reports) = tokio::sync::mpsc::unbounded_channel();
    tier.runner
        .run_turn_until_crash(
            world.admitted(),
            park::attempt(&world, false, report, 2),
            world.kill.clone(),
        )
        .await;
    assert_eq!(world.settled().await.completeness.open_runs, 0);
    assert_eq!(world.facts().await.len(), 2);
    assert_eq!(world.invocations(), 2);
    world
}

/// The substrate's failed-run evidence ends a started root and keeps its spend.
pub async fn substrate_lost_keeps_each_paid_call_once(tier: &UsageAccountingTier) {
    let world = paid_unfinished(tier, "substrate-lost-spend").await;
    let target = crate::engine::RootRef {
        session: world.session_id.clone(),
        root: world.turn_id(),
    };
    let terminal = tier
        .stores
        .session_store_factory()
        .end_lost_root(
            &target,
            crate::engine::RootRunLoss::FailedRun,
            tier.stores.clock().timestamp_ms(),
        )
        .await
        .expect("end lost root")
        .expect("started root ends");
    assert!(matches!(
        terminal.cause,
        crate::store::RootTerminalCause::SubstrateLost { .. }
    ));
    tier.effect_host
        .retire_usage_execution(&world.owner(), world.admitted().scope())
        .await
        .expect("retire lost execution");
    assert_each_returned_attempt_once(&world, 0, "substrate lost").await;
    assert_eq!(world.facts().await.len(), 2);
    assert_eq!(world.invocations(), 2);
}

/// An operator's Cancel intent ends a build-drift park without buying a call.
pub async fn operator_cancelled_parked_keeps_each_paid_call_once(tier: &UsageAccountingTier) {
    let world = paid_unfinished(tier, "operator-cancelled-spend").await;
    let (report, _reports) = tokio::sync::mpsc::unbounded_channel();
    tier.runner
        .run_parking_turn_until_rested(world.admitted(), park::attempt(&world, true, report, 2))
        .await;
    let factory = tier.stores.session_store_factory();
    let store =
        crate::conformance::law_session_store(tier.stores.as_ref(), &world.session_id).await;
    let park = store
        .load_turn_park(&world.session_id)
        .await
        .expect("park read")
        .expect("root parked");
    let intent = factory
        .open_root_intent(
            &crate::store::RootIntentRequest {
                session_id: world.session_id.clone(),
                root: world.turn_id(),
                park: park.park_id,
                verb: crate::store::RootVerb::Cancel,
            },
            tier.stores.clock().timestamp_ms(),
        )
        .await
        .expect("operator cancellation accepted");
    let terminal = factory
        .root_terminal(&world.session_id, &world.turn_id())
        .await
        .expect("terminal")
        .expect("cancelled root");
    assert!(
        matches!(terminal.cause, crate::store::RootTerminalCause::OperatorCancelled { intent: id } if id == intent.id)
    );
    tier.effect_host
        .retire_usage_execution(&world.owner(), world.admitted().scope())
        .await
        .expect("retire cancelled execution");
    assert_each_returned_attempt_once(&world, 0, "operator cancelled parked").await;
    assert_eq!(world.facts().await.len(), 2);
    assert_eq!(world.invocations(), 2);
    assert!(
        store
            .load_turn_park(&world.session_id)
            .await
            .expect("park after cancel")
            .is_none()
    );
}
