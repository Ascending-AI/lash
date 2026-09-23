//! ADR 0099 §10 L3 on the product path: an aggregate's infrastructure failure
//! travels on the host-control channel, never as a leaf rejection (FIG-3397).
//!
//! The case injects the failure where a real one lands: the controller's
//! settlement read answers a store I/O error. A guest `catch` must not see it.
//! Caught, it would let the cell commit a fallback value that a redrive —
//! which reads the same settlement successfully — answers differently.

use super::*;

use std::sync::atomic::AtomicBool;

/// A native controller whose settlement reads fail while `fail_settlements`
/// is raised; every other operation is the native controller's own.
struct SettlementFaultController {
    native: Arc<lash_core::facade_support::NativeRuntimeEffectController>,
    fail_settlements: AtomicBool,
}

#[async_trait]
impl lash_core::AwaitEventResolver for SettlementFaultController {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> std::result::Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> std::result::Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> std::result::Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> std::result::Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait]
impl lash_core::RuntimeEffectController for SettlementFaultController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        self.native.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> std::result::Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError>
    {
        self.native.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> std::result::Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.native.native_effect_groups_substrate()
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> std::result::Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError>
    {
        if self.fail_settlements.load(Ordering::SeqCst) {
            return Err(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "settlement read failed: disk I/O error",
            ));
        }
        self.native.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> std::result::Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> std::result::Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> std::result::Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.read_group_settlement(group_key, rank).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> std::result::Result<bool, lash_core::RuntimeEffectControllerError> {
        self.native
            .group_child_drain_blocked(group_key, commit_seq)
            .await
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
        let registry = Arc::new(TestLocalProcessRegistry::default());
        register_intent_target(registry.as_ref(), &session_id).await;
        let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
        let native = Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default());
        let controller = Arc::new(SettlementFaultController {
            native: Arc::clone(&native),
            fail_settlements: AtomicBool::new(true),
        });
        let host =
            lash_core::facade_support::NativeEffectHost::with_controller_sharing_native_groups(
                controller as Arc<dyn lash_core::RuntimeEffectController>,
                &native,
            )
            .allow_process_lifetime_completion_keys();
        let core = oracle_builder(
            tier,
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
            Arc::clone(&registry),
            Arc::clone(&requests),
        )
        .effect_host(Arc::new(host))
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
