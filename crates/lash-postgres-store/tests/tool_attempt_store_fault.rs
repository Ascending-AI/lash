//! FIG-3528 — a store error on a `ToolAttempt` claim or finalize is a typed
//! controller abort, never a model-visible tool result.
//!
//! The PostgreSQL counterpart of the SQLite store-fault laws: each test drives
//! the production attempt coordinator (`coordinate_tool_invocation`) against
//! the real PostgreSQL effect journal with a fault armed on one row-store
//! operation. `ToolCallLaunch::ControllerAborted` is the channel the turn
//! driver treats as a crash: it carries no outcome record, so nothing the
//! store reported can be projected to the model or committed as a
//! `tool_attempt_failed` the tool never produced. The redrive half is ADR
//! 0042's contract — replay a recorded outcome, or re-run the attempt —
//! asserted at the granularity the journal row lives at.

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
use lash_postgres_store::{
    PostgresEffectReplayOptions, PostgresRuntimeEffectController, PostgresStorage,
};

use crate::support::{SharedDatabaseLock, database_url, reset};

const SESSION: &str = "pg-tool-attempt-store-fault";
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
fn short_lease_options() -> PostgresEffectReplayOptions {
    PostgresEffectReplayOptions {
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
    /// interval to fire mid-body — the case a renew fault must abort.
    SlowSuccess,
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

/// A dispatch context whose effect controller is a real PostgreSQL journal
/// with a short lease, plus the probe provider's body-run counter. Returns
/// `None` when no test database is configured.
async fn faulted_world(
    answer: ProbeAnswer,
) -> Option<(
    lash_core_execution::tool_dispatch::ToolDispatchContext<'static>,
    PostgresRuntimeEffectController,
    Arc<AtomicUsize>,
    SharedDatabaseLock,
)> {
    let database_url = database_url()?;
    let lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect the PostgreSQL store-fault host");
    reset(storage.pool()).await;

    let scope = ExecutionScope::turn(SessionId::from(SESSION.to_string()), TURN);
    let controller =
        PostgresRuntimeEffectController::with_options(&storage, scope, short_lease_options());
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
    Some(((*built.dispatch).clone(), controller, calls, lock))
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
        lash_core_execution::RuntimeErrorCode::PostgresEffectReplayStore,
        "the abort must carry the typed store error the journal returned: {error:?}"
    );
}

/// A store error on the ToolAttempt claim aborts the call as a typed
/// controller error, and the tool body never ran — nothing was admitted, so
/// nothing can reach the model or the transcript.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_claim_store_error_aborts_turn() {
    let Some((context, controller, calls, _lock)) = faulted_world(ProbeAnswer::Success).await
    else {
        eprintln!("skipping the PostgreSQL store-fault law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
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
    let Some((context, controller, calls, _lock)) = faulted_world(ProbeAnswer::Success).await
    else {
        eprintln!("skipping the PostgreSQL store-fault law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
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
    // 2s of server-side time), the attempt re-runs, and its real outcome
    // seals this time.
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

/// A store error on the lease renew — mid-body — aborts the call the same
/// way. The abandoned execution still seals a `Failed` terminal recording the
/// fault, so the redrive's claim replays that journaled failure as the
/// attempt's recorded outcome (`tool_attempt_failed`, model-visible as
/// today) instead of aborting again or re-running the body.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_renew_store_error_aborts_turn() {
    let Some((context, controller, calls, _lock)) = faulted_world(ProbeAnswer::SlowSuccess).await
    else {
        eprintln!("skipping the PostgreSQL store-fault law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let faults = controller.replay_driver().journal_faults();
    faults.fail_next(EffectJournalFaultPoint::Renew, &attempt_replay_key(1));

    assert_store_abort(&drive(&context).await);
    assert!(faults.fired(), "the armed renew fault fired");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the renew fault fired while the tool body was running"
    );

    // The `Failed` terminal the aborted execution sealed is the durable
    // record: replaying it surfaces `tool_attempt_failed` rather than
    // re-running the body or aborting the turn a second time (FIG-3528's
    // "a recorded `Failed` terminal replayed for that tool stays
    // model-visible").
    let launch = drive(&context).await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("a recorded `Failed` terminal replays as a tool result: {launch:?}");
    };
    let lash_core_execution::ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome
    else {
        panic!("the journaled failure replays as a failure record: {outcome:?}");
    };
    assert_eq!(failure.code, "tool_attempt_failed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a recorded `Failed` terminal replays without re-executing the body"
    );
    assert_eq!(
        faults.calls_after_fire(),
        0,
        "the faulted renew was never retried: its `Failed` terminal stands"
    );
}

/// The other half of the classification: a failure the tool *produced* is a
/// `Done` outcome — model-visible exactly as before — and a redrive replays
/// the recorded terminal without re-entering the body.
#[tokio::test(flavor = "multi_thread")]
async fn tool_attempt_tool_failure_stays_model_visible() {
    let Some((context, _controller, calls, _lock)) = faulted_world(ProbeAnswer::Failure).await
    else {
        eprintln!("skipping the PostgreSQL store-fault law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };

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
