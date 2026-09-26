//! ADR 0099 §10 L3 on the product path: an aggregate's infrastructure failure
//! travels on the host-control channel, never as a leaf rejection (FIG-3397).
//!
//! The case injects the failure where a real one lands: the controller's
//! settlement read answers a store I/O error. A guest `catch` must not see it.
//! Caught, it would let the cell commit a fallback value that a redrive —
//! which reads the same settlement successfully — answers differently.

use super::*;

use std::sync::atomic::AtomicBool;

/// Fails every settlement read while `fail_settlements` is raised; every
/// other operation is the SQLite memory backend's own.
struct SettlementFaultLayer {
    fail_settlements: AtomicBool,
}

#[async_trait]
impl lash_core::testing::EffectLayer for SettlementFaultLayer {
    async fn await_next_settlement(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> std::result::Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError>
    {
        if self.fail_settlements.load(Ordering::SeqCst) {
            return Err(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "settlement read failed: disk I/O error",
            ));
        }
        inner.await_next_settlement(handle, cancel).await
    }
}

/// A `try`/`catch` around each aggregate whose settlement read fails on store
/// I/O. The catch never runs: the turn ends on the host failure, and no cell
/// commits the fallback.
async fn a_settlement_store_failure_is_not_caught_by_the_cell(tier: &JournaledTier) -> Result<()> {
    for aggregate in [
        "Promise.any",
        "Promise.race",
        "Promise.all",
        "Promise.allSettled",
    ] {
        let session_id = format!(
            "aggregate-oracle-host-control-{}",
            aggregate.trim_start_matches("Promise.").to_lowercase()
        );
        let theatre = Arc::new(OracleTheatre::default());
        let backend = DecoratedBackend::over(tier.backend().await.into()).effect_host(|inner| {
            Arc::new(lash_core::testing::LayeredEffectHost::new(
                inner,
                Arc::new(SettlementFaultLayer {
                    fail_settlements: AtomicBool::new(true),
                }),
            ))
        });
        let registry = lash_core::Backend::from(backend.clone()).process_registry();
        register_intent_target(registry.as_ref(), &session_id, &theatre).await;
        let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
        let core = oracle_builder(
            backend.into(),
            &session_id,
            vec![typescript_block(&format!(
                r#"try {{
  await {aggregate}([oracle.step({{ id: "a" }}), oracle.step({{ id: "b" }})]);
  finish("resolved");
}} catch (error) {{
  finish("caught");
}}"#
            ))],
            Arc::clone(&theatre),
            Arc::clone(&requests),
        )
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session(&session_id).open().await?;
        let report = tokio::time::timeout(
            RENDEZVOUS_BUDGET,
            session
                .turn(TurnInput::text("settle the aggregate"))
                .stream_to(theatre.as_ref()),
        )
        .await
        .expect("the turn reports");
        match report {
            Ok(report) => {
                assert_ne!(
                    report.final_value(),
                    Some(&serde_json::json!("caught")),
                    "{}/{aggregate}: the cell's catch saw a host-control failure",
                    tier.name
                );
                assert_ne!(
                    report.final_value(),
                    Some(&serde_json::json!("resolved")),
                    "{}/{aggregate}: the aggregate cannot resolve without a settlement",
                    tier.name
                );
            }
            Err(error) => assert!(
                error.to_string().contains("disk I/O error"),
                "{}/{aggregate}: the turn fails on the host failure itself: {error}",
                tier.name
            ),
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_settlement_store_failure_is_not_caught_by_the_cell() -> Result<()> {
    a_settlement_store_failure_is_not_caught_by_the_cell(&sqlite()).await
}
