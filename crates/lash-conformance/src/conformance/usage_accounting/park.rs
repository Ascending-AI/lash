#![expect(clippy::expect_used, reason = "conformance fixture assertions")]
use super::*;
use lash_core::runtime::effect::{EffectLayer, LayeredEffectHost};

pub(super) struct FourthEnvelope {
    calls: AtomicUsize,
    drift: bool,
    crash: crate::ConformanceCrash,
    after: usize,
}

#[async_trait::async_trait]
impl EffectLayer for FourthEnvelope {
    async fn execute_effect(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        envelope: crate::RuntimeEffectEnvelope,
        executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let model_call = matches!(
            envelope.command,
            crate::RuntimeEffectCommand::LlmCall { .. }
        );
        let scope = envelope.invocation.address().execution_scope.clone();
        let outcome = inner.execute_effect(envelope, executor).await?;
        if model_call && self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.after {
            let fourth = crate::RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(scope, "usage-fourth-envelope")
                        .expect("the fourth effect has a stable address"),
                    crate::RuntimeAttribution::none(),
                    "fourth-envelope",
                ),
                crate::RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: if self.drift { "fourth-v2" } else { "fourth-v1" }.into(),
                },
            );
            inner
                .execute_effect(
                    fourth,
                    crate::RuntimeEffectLocalExecutor::testing(|_| async {
                        Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: serde_json::json!("fourth operation"),
                        })
                    }),
                )
                .await?;
            assert!(
                !self.drift,
                "the changed fourth envelope must refuse replay"
            );
            self.crash.fire();
            std::future::pending::<()>().await;
        }
        Ok(outcome)
    }
}

pub(super) fn attempt(
    world: &World,
    drift: bool,
    report: tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>,
    after: usize,
) -> crate::ConformanceTurnAttempt {
    let world = world.clone();
    Arc::new(move |scope| {
        let world = world.clone();
        let report = report.clone();
        Box::pin(async move {
            let scope = LayeredEffectHost::layer_scoped(
                scope,
                Arc::new(FourthEnvelope {
                    calls: AtomicUsize::new(0),
                    drift,
                    crash: world.kill.clone(),
                    after,
                }),
            )
            .expect("layer the tier's real controller");
            let result = world.drive(scope).await;
            let ending = crate::ConformanceTurnEnd::of(&result);
            let _ = report.send(result);
            ending
        })
    })
}

/// E1: three paid model calls precede a fourth, non-spending journaled
/// operation. A changed build reconstructs that fourth envelope differently;
/// the root parks and is never restored or driven to finalization.
#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn usage_of_a_root_parked_forever_before_finalization_is_read_without_driving(
    tier: &UsageAccountingTier,
) {
    let world = World::new(tier, "fourth-envelope-drift", Script::completed(4));
    let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
    tier.runner
        .run_turn_until_crash(
            world.admitted(),
            attempt(&world, false, report.clone(), 3),
            world.kill.clone(),
        )
        .await;
    assert_eq!(world.invocations(), 3);
    let store =
        crate::conformance::law_session_store(tier.stores.as_ref(), &world.session_id).await;
    let head = store
        .load_session_head_meta(&world.session_id)
        .await
        .expect("head before park");
    let fence = store
        .drive_epoch(&world.session_id)
        .await
        .expect("fence before park");
    tier.runner
        .run_parking_turn_until_rested(world.admitted(), attempt(&world, true, report, 3))
        .await;
    let mut refused = None;
    while let Ok(result) = reports.try_recv() {
        refused = Some(result.expect_err("the changed fourth envelope parks the root"));
    }
    assert!(
        refused
            .expect("the parked execution reports")
            .code
            .parks_turn()
    );
    let park = store
        .load_turn_park(&world.session_id)
        .await
        .expect("read park")
        .expect("root parked");
    assert_eq!(park.turn_id, world.turn_id());
    assert!(matches!(
        park.reason,
        crate::store::ParkReason::EffectReplayDivergence { ref effect_kind, .. }
            if effect_kind == "language_runtime_value"
    ));
    let parked_head = store
        .load_session_head_meta(&world.session_id)
        .await
        .expect("head after park");
    assert_eq!(
        parked_head.as_ref().map(|head| (
            head.head_revision,
            &head.current_frame_node_id,
            &head.checkpoint_ref,
            &head.leaf_node_id
        )),
        head.as_ref().map(|head| (
            head.head_revision,
            &head.current_frame_node_id,
            &head.checkpoint_ref,
            &head.leaf_node_id
        ))
    );
    assert_eq!(
        store
            .drive_epoch(&world.session_id)
            .await
            .expect("fence after park"),
        fence
    );

    // This store handle is independent of every runtime constructed above.
    // The read opens no runtime and lends no controller to the parked root.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let usage = loop {
        let usage = tier
            .stores
            .usage_accounting()
            .load_owner_usage(&world.owner())
            .await
            .expect("second host reads usage");
        if usage.completeness.is_settled() {
            break usage;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "three settlements arrive within five seconds"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(usage.completeness, crate::UsageCompleteness::default());
    assert_eq!(world.facts().await.len(), 3);
    assert_each_returned_attempt_once(&world, 0, "parked fourth-envelope root").await;
    assert_eq!(
        world.invocations(),
        3,
        "parking and reading buy no further call"
    );
    assert_eq!(
        store
            .load_turn_park(&world.session_id)
            .await
            .expect("park after read"),
        Some(park)
    );
    assert_eq!(
        store
            .drive_epoch(&world.session_id)
            .await
            .expect("fence after read"),
        fence
    );
}
