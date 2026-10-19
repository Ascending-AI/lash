//! A `Repeatable` call a cell makes (FIG-6289): a retried call answers the
//! cell's wait with its last attempt, and a failure no repeat can repair
//! runs once.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use lash_vm_runtime::ToolDefinitionBindingExt as _;

use super::{SESSION, TURN, finish_of, open_host, typescript_services, typescript_state};
use crate::testing::{cell_context, run_cell};

const FLAKY: &str = "flaky";

/// `tools.flaky`: a `Repeatable` tool allowed two attempts with no backoff.
/// It counts its attempts, and fails with `class` on its first, or on every
/// one when `class` is `InvalidRequest`; another attempt answers its number.
struct Flaky {
    class: lash_core::ToolFailureClass,
    attempts: AtomicU32,
}

impl Flaky {
    fn failing_first_with(class: lash_core::ToolFailureClass) -> Arc<Self> {
        Arc::new(Self {
            class,
            attempts: AtomicU32::new(0),
        })
    }
}

fn flaky_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{FLAKY}"),
        FLAKY,
        "Fails its first attempt.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(2).expect("a nonzero attempt bound"),
        0,
        0,
    ))
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(["tools"], FLAKY))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Flaky {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![flaky_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == FLAKY).then(|| Arc::new(flaky_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 || self.class == lash_core::ToolFailureClass::InvalidRequest {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::with_suggested_delay(
                self.class.clone(),
                "flaky_failed",
                format!("attempt {attempt} failed"),
                Some(0),
            ))
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!(format!("attempt {attempt}"))).into()
    }
}

/// A cell's call whose first attempt failed and whose retry answered is
/// answered with the retry's value: the call's outcome is found by the
/// identity its admission minted, whichever attempt settled it.
#[tokio::test(flavor = "multi_thread")]
async fn a_retried_repeatable_call_answers_the_cell_with_its_last_attempt() {
    let host = open_host().await;
    let tools = Flaky::failing_first_with(lash_core::ToolFailureClass::Io);
    let ctx = cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone());
    let response = run_cell(
        &mut typescript_state(),
        ctx.clone(),
        &typescript_services(),
        "await control.finish(await tools.flaky({}));",
    )
    .await;
    assert_eq!(finish_of(&response), serde_json::json!("attempt 2"));
    assert_eq!(tools.attempts.load(Ordering::SeqCst), 2);
    assert!(
        ctx.take_nested_effect_error().is_none(),
        "the cell answered without a fault"
    );
}

/// An `invalid_request` failure is the request's own: a repeat would fail
/// the same way, so a `Repeatable` call runs it once and the cell is
/// answered with it.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_request_failure_of_a_repeatable_call_runs_once() {
    let host = open_host().await;
    let tools = Flaky::failing_first_with(lash_core::ToolFailureClass::InvalidRequest);
    let ctx = cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone());
    let response = run_cell(
        &mut typescript_state(),
        ctx.clone(),
        &typescript_services(),
        r#"let failed = "no";
try { await tools.flaky({}); }
catch (error) { failed = "yes"; }
await control.finish(failed);"#,
    )
    .await;
    assert_eq!(finish_of(&response), serde_json::json!("yes"));
    assert_eq!(tools.attempts.load(Ordering::SeqCst), 1);
    assert!(
        ctx.take_nested_effect_error().is_none(),
        "the cell answered without a fault"
    );
}

/// A wait nothing can answer is the broker's own invariant failing over
/// sound records: the cell's turn ends with the typed, terminal
/// `broker_invariant` failure naming the call, never a retryable fault that
/// every later pass meets again.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswerable_wait_ends_the_turn_with_a_typed_terminal_failure() {
    let host = open_host().await;
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "exec-code:0",
        Flaky::failing_first_with(lash_core::ToolFailureClass::Io),
    );
    let call = lash_core::ToolCallId::fixture("unanswered");
    let exec = lash_vm_broker::ExecKey::Cell(
        lash_core::SessionId::from(SESSION),
        lash_core::TurnId::try_from(TURN.to_owned()).expect("a turn id"),
        lash_vm_broker::CellId::new("exec-code:0"),
    );
    let response = super::super::broker_failure(
        &mut typescript_state(),
        &ctx,
        lash_vm_broker::kernel::KernelFailure::Invariant(
            lash_vm_broker::BrokerInvariant::Unanswerable {
                calls: vec![call.clone()],
            },
        ),
        false,
        &super::super::host::CellHostLedgers::default(),
        &super::super::cell_run::DiedRunning {
            workers: &typescript_services().workers,
            exec: &exec,
        },
    );
    assert!(response.error().is_some(), "the cell answers no value");
    let error = ctx
        .take_nested_effect_error()
        .expect("the invariant is the turn's failure");
    assert_eq!(error.code, lash_core::RuntimeErrorCode::BrokerInvariant);
    assert!(error.message.contains(call.as_str()), "{}", error.message);
    assert!(!error.is_attempt_fault(), "no pass derives the cell again");
    assert_eq!(
        error.turn_failure_cause(),
        lash_core::TurnFailureCause::Outcome,
        "the turn fails with it rather than aborting for a retry"
    );
}
