//! Compiled sources for the Rust snippets on `docs/operations.html`.

use std::sync::Arc;
use std::time::Duration;

use lash::durability::{InlineEffectHost, LeaseTimings};
use lash::persistence::{LeaseOwnerIdentity, SessionStoreFactory};
use lash::provider::ProviderHandle;
use lash::{LashCore, LashSession, TurnInput, TurnOutput};

fn configure_lease_timings(
    factory: lash::rlm::RlmProtocolPluginFactory,
    provider: ProviderHandle,
    store_factory: Arc<dyn SessionStoreFactory>,
) -> lash::Result<LashCore> {
    // docs:start:lease-timings
    // One timing decision governs the three durable lease lanes:
    // session execution, effect replay, and process execution. `new` enforces
    // `ttl >= 3 * renew_interval`, so a live owner can miss two renewals before
    // a peer may treat the lease as expired. Queued-work and turn-input claims
    // are generation-fenced under the session lease and carry no timing.
    let lease_timings = LeaseTimings::new(
        Duration::from_secs(15), // ttl
        Duration::from_secs(5),  // renew_interval
    )
    .expect("ttl >= 3 * renew_interval");

    let core = LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits(
                "anthropic/claude-sonnet-4.6",
                Default::default(),
                200_000,
                None,
            )
            .expect("valid model metadata"),
        )
        .store_factory(store_factory)
        .effect_host(Arc::new(InlineEffectHost::default()))
        .lease_timings(lease_timings) // omit to keep the 30s ttl / 10s renew default
        .build()?;
    // docs:end:lease-timings
    Ok(core)
}

async fn open_with_stable_owner(core: &LashCore, chat_id: &str) -> lash::Result<LashSession> {
    // docs:start:worker-identity
    // A stable owner id per replica plus a per-boot incarnation. A crashed
    // holder remains busy until the lease TTL expires.
    let owner = LeaseOwnerIdentity::opaque(
        std::env::var("WORKER_ID").unwrap_or_else(|_| "worker-1".to_string()),
        std::env::var("AGENT_SERVICE_INCARNATION").unwrap_or_else(|_| boot_incarnation()),
    );

    let session = core
        .session(chat_id)
        .session_execution_owner(owner)
        .open()
        .await?;
    // docs:end:worker-identity
    Ok(session)
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
    //    `session.abandon_turn_input_claim(&claim)`. Losing the session lease
    //    also supersedes those generation-fenced claims. Resolve outstanding
    //    durable waits as `Cancelled` with `session.revoke_durable_waits()`.

    // 5. Release provider transports. The default `close()` is a no-op; the
    //    Codex provider sends WebSocket Close frames on its cached sessions.
    let _ = provider.close().await;

    // 6. Flush the trace sink (fsync for JSONL). OTel span-export durability is
    //    the host's duty: `force_flush()`/`shutdown()` your own TracerProvider.
    core.flush_trace_sink()?;

    // 7. Exit. Any lease this process still holds now expires on its TTL.
    Ok(())
    // docs:end:graceful-drain
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
            // SessionExecutionBusy / SessionExecutionLeaseLost: another owner
            // holds or fenced the lease, so the attempt committed nothing.
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

fn record_turn_metrics(output: &TurnOutput, session: &LashSession) {
    // docs:start:monitoring
    // Per-turn timing, straight off the runtime clock.
    let started_at = output.result.started_at(); // SystemTime the turn was claimed
    let elapsed = output.result.duration(); // claim -> commit + post-persist hooks
    let _ = (started_at, elapsed);

    // Cumulative token usage for the session, split by source and by model.
    let usage = session.usage_report();
    let _ = (usage.entry_count, usage.usage);
    // docs:end:monitoring
}
