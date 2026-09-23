//! FIG-3528 — a store error on a `ToolAttempt` claim or finalize, or a renew
//! outage past the lease miss budget (FIG-3512), is a typed controller abort,
//! never a model-visible tool result.
//!
//! Each law drives the production attempt coordinator
//! (`coordinate_tool_invocation`) against the real SQLite effect journal with
//! a fault armed on one row-store operation. `ToolCallLaunch::ControllerAborted`
//! is the channel the turn driver treats as a crash: it carries no outcome
//! record, so nothing the store reported can be projected to the model or
//! committed as a `tool_attempt_failed` the tool never produced. The redrive
//! half of each law is ADR 0042's contract — replay a recorded outcome, or
//! re-run the attempt — asserted at the granularity the journal row lives at.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core_execution::runtime::effect::effect_replay_driver::{
    EffectJournalFaultPoint, StoreReplayAdapter,
};
use lash_core_execution::sansio::PendingToolCall;
use lash_core_execution::testing::TestExecutionContextBuilder;
use lash_core_execution::tool_dispatch::ToolCallLaunch;
use lash_core_execution::{
    ExecutionScope, PreparedToolCall, SessionId, ToolCall, ToolCatalog, ToolContext, ToolContract,
    ToolDefinition, ToolId, ToolManifest, ToolOutcome, ToolProvider,
};
use lash_sqlite_store::{SqliteEffectReplayOptions, SqliteRuntimeEffectController};

const SESSION: &str = "sqlite-tool-attempt-store-fault";
// The builder's default admitted scope is `AdmittedScope::turn(session,
// "test-turn")`; the controller's journal scope must equal it or the
// controller refuses the envelope the admitted scope mints.
const TURN: &str = "test-turn";
const TOOL_ID: &str = "tool:fault_probe";
const TOOL_NAME: &str = "fault_probe";
const CALL_ID: &str = "fault-probe-call";

/// A lease window long enough that an unfaulted claim-execute-finalize round
/// trip always fits inside it, yet short enough that a redrive's busy wait on
/// the abandoned claim stays in seconds.
fn short_lease_options() -> SqliteEffectReplayOptions {
    SqliteEffectReplayOptions {
        lease_timings: lash_core_execution::facade_support::LeaseTimings::from_ttl(
            Duration::from_millis(2_000),
        )
        .expect("a 2s ttl satisfies the three-renew-interval minimum"),
        drain_budget: Default::default(),
    }
}

/// What the probe's body returns when it does run.
#[derive(Clone, Copy)]
enum ProbeAnswer {
    /// `ToolOutcome::ok` — the case a finalize fault must not reclassify.
    Success,
    /// A tool-produced failure — the case that must stay model-visible.
    Failure,
    /// `ToolOutcome::ok` after a delay long enough for the claim's renew
    /// interval to fire mid-body, yet inside one lease TTL of a renewal
    /// that lands — the case a single renew fault must not disturb.
    SlowSuccess,
    /// `ToolOutcome::ok` after a delay well past one lease TTL — the case a
    /// renew outage outlasting the miss budget must abort mid-body.
    OutlastsLease,
}

struct ProbeTools {
    answer: ProbeAnswer,
    calls: Arc<AtomicUsize>,
}

fn probe_definition() -> ToolDefinition {
    ToolDefinition::raw(
        TOOL_ID,
        TOOL_NAME,
        "Probe tool for the attempt store-fault laws.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": {} },
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": { "echo": {} },
            "additionalProperties": false
        }),
    )
}

#[async_trait::async_trait]
impl ToolProvider for ProbeTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == TOOL_NAME).then(|| Arc::new(probe_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core_execution::ToolAttemptOutcome {
        assert_eq!(call.name(), TOOL_NAME);
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.answer {
            ProbeAnswer::SlowSuccess => {
                // Outlast several renew intervals (~667ms each under the 2s
                // lease) so an armed renew fault lands while the body runs.
                tokio::time::sleep(Duration::from_millis(2_200)).await;
                ToolOutcome::ok(serde_json::json!({
                    "echo": call.args.get("value").cloned().unwrap_or_default(),
                }))
                .into()
            }
            ProbeAnswer::OutlastsLease => {
                tokio::time::sleep(Duration::from_millis(6_000)).await;
                ToolOutcome::ok(serde_json::json!({
                    "echo": call.args.get("value").cloned().unwrap_or_default(),
                }))
                .into()
            }
            ProbeAnswer::Success => ToolOutcome::ok(serde_json::json!({
                "echo": call.args.get("value").cloned().unwrap_or_default(),
            }))
            .into(),
            ProbeAnswer::Failure => ToolOutcome::err(serde_json::json!({
                "detail": "the tool declined the call",
            }))
            .into(),
        }
    }
}

fn prepared_call() -> PreparedToolCall {
    PreparedToolCall::identity(
        ToolId::from(TOOL_ID.to_string()),
        PendingToolCall {
            call_id: CALL_ID.to_string(),
            tool_name: TOOL_NAME.to_string(),
            args: serde_json::json!({ "value": "invoice-42" }),
            replay: None,
        },
    )
}

/// The ToolAttempt row's replay key for attempt `n` of the probe call:
/// `tool:{call_id}:attempt:{n}` under a scalar identity with no parent.
fn attempt_replay_key(attempt: u32) -> String {
    format!("tool:{CALL_ID}:attempt:{attempt}")
}

/// A dispatch context whose effect controller is a real SQLite journal with a
/// short lease, plus the probe provider's body-run counter.
async fn faulted_world(
    answer: ProbeAnswer,
) -> (
    lash_core_execution::tool_dispatch::ToolDispatchContext<'static>,
    SqliteRuntimeEffectController,
    Arc<AtomicUsize>,
) {
    let scope = ExecutionScope::turn(SessionId::from(SESSION.to_string()), TURN);
    let controller = lash_sqlite_store::SqliteDeployment::memory_with_options_and_clock(
        lash_sqlite_store::SqliteDeploymentOptions {
            effect_replay: short_lease_options(),
            ..lash_sqlite_store::SqliteDeploymentOptions::memory()
        },
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .expect("memory deployment")
    .open_effect_controller(scope)
    .await
    .expect("in-memory SQLite effect controller");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(ProbeTools {
        answer,
        calls: Arc::clone(&calls),
    });
    let built = TestExecutionContextBuilder::new()
        .session_id(SESSION)
        .provider(provider)
        .tool_catalog(ToolCatalog::from_tool_definitions(vec![probe_definition()]))
        .shared_effect_controller(Arc::new(controller.clone()))
        .build();
    ((*built.dispatch).clone(), controller, calls)
}

async fn drive(
    context: &lash_core_execution::tool_dispatch::ToolDispatchContext<'_>,
) -> ToolCallLaunch {
    let prepared = prepared_call();
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .build();
    lash_core_execution::tool_dispatch::coordinate_prepared_tool_call_launch_with_execution_context(
        context,
        prepared,
        None,
        tool_context,
    )
    .await
}

/// The abort a store fault must produce: `ControllerAborted` carrying this
/// backend's store-error code.
fn assert_store_abort(launch: &ToolCallLaunch) {
    let ToolCallLaunch::ControllerAborted(error) = launch else {
        panic!("a ToolAttempt store fault must abort, not produce a tool result: {launch:?}");
    };
    assert_eq!(
        error.code,
        lash_core_execution::RuntimeErrorCode::SqliteEffectReplayStore,
        "the abort must carry the typed store error the journal returned: {error:?}"
    );
}

/// A store error on the ToolAttempt claim aborts the call as a typed
/// controller error, and the tool body never ran — nothing was admitted, so
/// nothing can reach the model or the transcript.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_claim_store_error_aborts_turn() {
    let (context, controller, calls) = faulted_world(ProbeAnswer::Success).await;
    let faults = controller.replay_driver().journal_faults();
    faults.fail_next(EffectJournalFaultPoint::Claim, &attempt_replay_key(1));

    assert_store_abort(&drive(&context).await);
    assert!(faults.fired(), "the armed claim fault fired");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a failed claim never reached the tool body"
    );

    // ADR 0042 redrive: nothing was journaled, so the whole attempt runs.
    let launch = drive(&context).await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("an unfaulted redrive completes the attempt: {launch:?}");
    };
    assert!(outcome.record.output.is_success());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        faults.calls_after_fire(),
        1,
        "the faulted claim was retried once, for real, on redrive"
    );
}

/// A store error on the ToolAttempt finalize — after the tool body already
/// ran — aborts the call as a typed controller error rather than committing a
/// `tool_attempt_failed` for work that succeeded. The claim stays open, so a
/// redrive re-runs the attempt to the tool's real outcome.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_finalize_store_error_aborts_turn() {
    let (context, controller, calls) = faulted_world(ProbeAnswer::Success).await;
    let faults = controller.replay_driver().journal_faults();
    faults.fail_next(EffectJournalFaultPoint::Finalize, &attempt_replay_key(1));

    assert_store_abort(&drive(&context).await);
    assert!(faults.fired(), "the armed finalize fault fired");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the finalize fault fired after the tool body ran"
    );

    // ADR 0042 redrive: the abandoned claim lapses (the lease window above is
    // 2s), the attempt re-runs, and its real outcome seals this time.
    let launch = drive(&context).await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("the redriven attempt must reach its own outcome: {launch:?}");
    };
    assert!(
        outcome.record.output.is_success(),
        "the redrive replays the tool's real outcome, not a recorded failure"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        faults.calls_after_fire(),
        1,
        "the faulted finalize was retried once, for real, on redrive"
    );
}

/// A store error on the lease renew — mid-body — is a missed renewal, not a
/// lost lease (FIG-3512): the lease TTL covers three renew intervals, so the
/// renewal loop retries while the tool keeps running. The attempt completes
/// with the tool's own outcome and seals it; nothing aborts and no
/// `tool_attempt_failed` is recorded, and a redrive replays the success.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_renew_store_error_within_budget_completes() {
    let (context, controller, calls) = faulted_world(ProbeAnswer::SlowSuccess).await;
    let faults = controller.replay_driver().journal_faults();
    faults.fail_next(EffectJournalFaultPoint::Renew, &attempt_replay_key(1));

    let launch = drive(&context).await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("a renew error inside the miss budget must not abort the attempt: {launch:?}");
    };
    assert!(
        outcome.record.output.is_success(),
        "the tool's own success is the attempt's outcome, not a recorded failure"
    );
    assert!(faults.fired(), "the armed renew fault fired");
    assert!(
        faults.calls_after_fire() >= 1,
        "the renewal loop retried the failed renew while the body ran"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The sealed `Completed` terminal replays without re-running the body.
    let launch = drive(&context).await;
    let ToolCallLaunch::Done(replayed) = launch else {
        panic!("a completed attempt replays as its recorded result: {launch:?}");
    };
    assert!(replayed.record.output.is_success());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a recorded success replays without re-executing the body"
    );
}

/// Renew errors that outlast the miss budget — no confirmed renewal for a
/// full lease TTL — abort the call as a typed lease-lost controller error
/// mid-body (FIG-3528's intent: nothing model-visible). No terminal is
/// sealed: the row stays reclaimable, so once the store recovers a redrive
/// re-runs the attempt to the tool's real outcome (FIG-3512, ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_renew_store_errors_past_budget_abort_turn() {
    let (context, controller, calls) = faulted_world(ProbeAnswer::OutlastsLease).await;
    let faults = controller.replay_driver().journal_faults();
    faults.fail_until_healed(EffectJournalFaultPoint::Renew, &attempt_replay_key(1));

    let launch = drive(&context).await;
    let ToolCallLaunch::ControllerAborted(error) = &launch else {
        panic!("renew errors past the budget must abort, not produce a tool result: {launch:?}");
    };
    assert_eq!(
        error.code,
        lash_core_execution::RuntimeErrorCode::SqliteEffectReplayLeaseLost,
        "the abort carries the typed lease-lost controller error: {error:?}"
    );
    assert!(
        faults.fires() >= 2,
        "the budget tolerated missed renewals before giving the lease up"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the budget ran out while the tool body was running"
    );

    // Nothing was sealed: the redrive reclaims the row once its lease lapses
    // and re-runs the attempt to the tool's own outcome.
    faults.heal();
    let launch = drive(&context).await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("the redriven attempt must reach its own outcome: {launch:?}");
    };
    assert!(
        outcome.record.output.is_success(),
        "the redrive re-runs the attempt instead of replaying a sealed failure"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// The other half of the classification: a failure the tool *produced* is a
/// `Done` outcome — model-visible exactly as before — and a redrive replays
/// the recorded terminal without re-entering the body.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_tool_failure_stays_model_visible() {
    let (context, _controller, calls) = faulted_world(ProbeAnswer::Failure).await;

    let launch = drive(&context).await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("a tool-produced failure is a tool result, not an abort: {launch:?}");
    };
    assert!(!outcome.record.output.is_success());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The recorded `Completed` terminal replays: same failure, no re-run.
    let launch = drive(&context).await;
    let ToolCallLaunch::Done(replayed) = launch else {
        panic!("a recorded tool failure replays as the same result: {launch:?}");
    };
    assert!(!replayed.record.output.is_success());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a recorded tool failure replays without re-executing the body"
    );
}
