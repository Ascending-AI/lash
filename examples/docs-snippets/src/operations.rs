//! Compiled sources for the Rust snippets on `docs/operations.html`.

use std::sync::Arc;
use std::time::Duration;

use lash::durability::{InlineEffectHost, LeaseTimings};
use lash::persistence::{
    AttachmentStore, LeaseOwnerIdentity, ProcessExecutionEnvStore, SessionLeaseRenewal,
    SessionStoreFactory,
};
use lash::provider::ProviderHandle;
use lash::{DeploymentDrainStatus, LashCore, LashSession, TurnInput, TurnOutput};

fn configure_lease_timings(
    factory: lash::rlm::RlmProtocolPluginFactory,
    provider: ProviderHandle,
    store_factory: Arc<dyn SessionStoreFactory>,
    attachment_store: Arc<dyn AttachmentStore>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    session_execution_owner: LeaseOwnerIdentity,
) -> lash::Result<LashCore> {
    // docs:start:lease-timings
    // One timing decision governs the three durable lease lanes:
    // session execution, effect replay, and process execution. `new` enforces
    // `ttl >= 3 * renew_interval`, so a live owner can miss two renewals before
    // a peer may treat the lease as expired. Queued-work and turn-input claims
    // pin the session-lease generation and carry no timing of their own.
    let lease_timings = LeaseTimings::new(
        Duration::from_secs(15), // ttl
        Duration::from_secs(5),  // renew_interval
    )
    .expect("ttl >= 3 * renew_interval");

    let core = LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("anthropic/claude-sonnet-4.6")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .store_factory(store_factory)
        .effect_host(Arc::new(InlineEffectHost::default()))
        .attachment_store(attachment_store)
        .process_env_store(process_env_store)
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .lease_timings(lease_timings) // omit to keep the 30s ttl / 10s renew default
        .build(session_execution_owner)?;
    // docs:end:lease-timings
    Ok(core)
}

fn build_with_stable_owner(builder: lash::LashCoreBuilder) -> lash::Result<LashCore> {
    // docs:start:worker-identity
    // Stable per worker or process, never per turn. Change only the
    // incarnation when this process boots.
    let owner = LeaseOwnerIdentity::opaque(
        std::env::var("WORKER_ID").unwrap_or_else(|_| "worker-1".to_string()),
        std::env::var("AGENT_SERVICE_INCARNATION").unwrap_or_else(|_| boot_incarnation()),
    );

    let core = builder.build(owner)?;
    // docs:end:worker-identity
    Ok(core)
}

fn boot_incarnation() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

async fn graceful_drain(
    core: &LashCore,
    provider: &ProviderHandle,
    idle_sessions: Vec<LashSession>,
) -> lash::Result<()> {
    // docs:start:graceful-drain
    // lash ships no drain orchestrator (ADR-0014): each step is an explicit,
    // host-owned lever. The order below and every deadline are host policy.

    // 1. Stop admitting new turns. A host-layer decision — flip a readiness
    //    flag, drain the load balancer. lash cannot see your ingress.

    // 2. Finish or cancel in-flight turns. Exact retained turn addresses should
    //    normally go through `core.turn_work_driver().request_cancel(...)`.
    //    This process-local cancel-all remains a shutdown compatibility lever;
    //    "shutdown" is opaque host vocabulary.
    for session in &idle_sessions {
        session.cancel_running_turns_with_origin(Some("shutdown".to_string()));
    }

    // 3. Park resumable sessions (flush dirty state through a fresh-lease commit,
    //    release the lease, keep a cheap handle) or `close()` ephemeral ones.
    //    Both consume the session and need exclusive ownership.
    for session in idle_sessions {
        let parked = session.park().await?;
        // Cache `parked` keyed by `parked.session_id()` and rebuild it later
        // with `LashCore::resume(parked)`; drop it instead to fully close.
        let _ = parked.session_id();
    }

    // 4. If you stopped an external queued-work or turn-input driver mid-claim,
    //    hand its claims back for immediate reuse with
    //    `session.abandon_queued_work_claim(&claim)` and
    //    `session.abandon_turn_input_claim(&claim)`. Lease loss makes the claims
    //    eligible for successor re-claim; only re-claim or explicit abandon
    //    invalidates the old completion. Resolve outstanding durable waits as
    //    `Cancelled` with `session.revoke_durable_waits()`.

    // 5. Read the deployment's authoritative process status before retiring
    //    an immutable deployment. This is a read, not a drain orchestrator:
    //    keep the old deployment registered while `drained` is false.
    let drain_status = core.drain_status(false).await?;
    if drain_status.drained {
        // The host may now retire this deployment.
    } else {
        // Keep it available for the remaining pinned invocations.
        let _ = drain_status.remaining_invocations;
    }

    // 6. Release provider transports. The default `close()` is a no-op; the
    //    Codex provider sends WebSocket Close frames on its cached sessions.
    let _ = provider.close().await;

    // 7. Release resources owned by plugin factories. This is not a drain
    //    orchestrator: intake, ordering, and deadlines remain host policy.
    core.shutdown().await?;

    // 8. Flush the trace sink (fsync for JSONL). OTel span-export durability is
    //    the host's duty: `force_flush()`/`shutdown()` your own TracerProvider.
    core.flush_trace_sink()?;

    // 9. Exit. Any lease this process still holds now expires on its TTL.
    Ok(())
    // docs:end:graceful-drain
}

async fn read_deployment_drain_status(
    core: &LashCore,
    accepting_new_work: bool,
) -> lash::Result<DeploymentDrainStatus> {
    core.drain_status(accepting_new_work).await
}

async fn run_turn_with_retry(session: &LashSession, text: &str) -> lash::Result<TurnOutput> {
    // docs:start:failure-classification
    loop {
        match session.turn(TurnInput::text(text)).run().await {
            Ok(output) => {
                // A failed LLM call finishes the turn instead of erroring; read
                // the typed provider signal off the turn's issues.
                for issue in &output.result.errors {
                    if issue.retryable == Some(true) {
                        // Transient provider/transport failure — safe to re-run.
                    }
                    if let Some(kind) = issue.provider_failure_kind {
                        let _ = kind; // Timeout, Http, Quota, Auth, Stream, ...
                    }
                }
                return Ok(output);
            }
            // Busy rejects before the turn starts; LeaseLost means an operation
            // observed handoff. Reload durable state before retrying: lease loss
            // alone neither proves no commit landed nor releases claims.
            Err(err) if err.is_retryable() => continue, // back off in real code
            // Wiring/config a retry can never repair (missing facet, provider
            // unconfigured). Surface it to an operator.
            Err(err) if err.is_terminal() => return Err(err),
            // Neither typed signal: unknown. Apply your own bounded policy.
            Err(err) => return Err(err),
        }
    }
    // docs:end:failure-classification
}

async fn triage_stuck_turn(core: &LashCore, session_id: &str) -> lash::Result<()> {
    // docs:start:stuck-turn
    // Step 1: read the lane. Diagnostics only: this never claims, renews, or
    // releases anything, so it is free to run against a live session. `None`
    // means no durable session under this id at all.
    let Some(lease) = core.session_lease_diagnostics(session_id).await? else {
        // The host's own record of an in-flight turn is what is wrong here.
        return Ok(());
    };
    let holder = lease.holder.as_ref().map(|holder| &holder.owner);
    let generation = lease.holder.as_ref().map(|holder| holder.generation);
    let _ = (holder, generation, lease.observed_at_epoch_ms);

    match lease.renewal() {
        // Nobody holds the lane. A turn the host still shows as running either
        // already committed and released, or never claimed. Reconcile against
        // the session's committed head.
        SessionLeaseRenewal::Unheld => {}

        // Renewals were current: the lane is healthy, so the turn is blocked
        // inside itself. Look at the provider call, not the lease, and cancel
        // the exact turn if it has to stop.
        SessionLeaseRenewal::Current { expires_in_ms } => {
            let _ = expires_in_ms;
        }

        // Renewals stopped. The worker that swept the lane logs
        // `session_execution_lease.taken_over` naming this holder as
        // `displaced_owner_id`, so the handoff is in the log even if this holder
        // died without noticing. Do NOT kill it: it may still win the commit CAS.
        // Only `session_execution_lease.commit_cas_rejected` proves it lost.
        SessionLeaseRenewal::Lapsed { expired_for_ms } => {
            let _ = expired_for_ms;
        }
    }
    Ok(())
    // docs:end:stuck-turn
}

fn record_turn_metrics(output: &TurnOutput, session: &LashSession) {
    // docs:start:monitoring
    // Per-turn timing, straight off the runtime clock.
    let started_at = output.result.started_at(); // SystemTime the turn was claimed
    let elapsed = output.result.duration(); // claim -> Committed Turn + Post-Commit Delivery
    let _ = (started_at, elapsed);

    // Cumulative token usage for the session, split by source and by model.
    let usage = session.usage_report();
    let _ = (usage.entry_count, usage.usage);
    // docs:end:monitoring
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_lease_timing_builder_resolves() {
        let factory = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::new(
                lash::rlm::ExecutionBound::instructions(1_000_000),
                lash::rlm::ExecutionBound::secs(30),
                lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
            ),
            Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
        );

        configure_lease_timings(
            factory,
            crate::test_support::provider(),
            Arc::new(lash::persistence::InMemorySessionStoreFactory::new()),
            Arc::new(lash::persistence::InMemoryAttachmentStore::new()),
            Arc::new(lash::persistence::InMemoryProcessExecutionEnvStore::new()),
            crate::example_process_owner(),
        )
        .expect("lease-timing snippet must build");
    }

    #[tokio::test]
    async fn deployment_drain_status_counts_work_and_reaches_drained() -> anyhow::Result<()> {
        use lash::process::{
            ProcessAwaitOutput, ProcessCompletionAuthority, ProcessInput, ProcessProvenance,
            ProcessRegistration, ProcessRegistry, RecoveryDisposition,
        };

        let registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::memory()
                .await
                .expect("open in-memory process registry"),
        );
        let core = LashCore::standard_builder(lash::TurnBudget::Unbounded)
            .model(crate::test_support::model())
            .store_factory(Arc::new(
                lash::persistence::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(registry.clone())
            .effect_host(Arc::new(InlineEffectHost::default()))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(crate::example_process_owner())?;
        registry
            .register_process(ProcessRegistration::new(
                "deployment-drain-status-example",
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryDisposition::ExternallyOwned,
                ProcessProvenance::host(),
            ))
            .await?;

        let accepting = read_deployment_drain_status(&core, true).await?;
        assert!(accepting.accepting_new_work);
        assert_eq!(accepting.remaining_invocations, 1);
        assert!(!accepting.drained);

        let closed = read_deployment_drain_status(&core, false).await?;
        assert!(!closed.accepting_new_work);
        assert_eq!(closed.remaining_invocations, 1);
        assert!(!closed.drained);
        assert!(accepting.checked_at <= closed.checked_at);

        registry
            .complete_process(
                "deployment-drain-status-example",
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                ProcessCompletionAuthority::external_owner(),
            )
            .await?;
        let drained = read_deployment_drain_status(&core, false).await?;
        assert!(!drained.accepting_new_work);
        assert_eq!(drained.remaining_invocations, 0);
        assert!(drained.drained);
        Ok(())
    }
}
