//! A process start whose invocation the engine cancels mid-schedule
//! (FIG-4127, FIG-4128).
//!
//! A declared start runs in a tool call's group child. When the parent turn
//! is cancelled, the group decides the call's cancel and the engine cancels
//! the child's invocation; the SDK surfaces that cancellation once, as a
//! `409`, at whichever await the start is parked on. Once the row is
//! registered the call's abandonment cancels the child, and only a submitted
//! run can honour that cancel, so the start must reach the engine whatever
//! await the cancellation lands on, and must never mistake it for a refusal.

use super::*;

/// A start over a fresh memory store set whose committed `Start` delivers
/// through the `ProcessStart` ledger, as production binds it.
struct CancelledStart {
    context: Arc<RecordingContext>,
    stores: MemoryProcessStores,
}

impl CancelledStart {
    async fn new() -> Self {
        Self {
            context: Arc::new(RecordingContext::default()),
            stores: memory_process_stores().await,
        }
    }

    async fn run(
        &self,
        start_key: &str,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let spec = lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::empty(),
            recovery_session_policy(),
        );
        RestateRuntimeEffectController::new_for_test(Arc::clone(&self.context))
            .execute_effect(
                start_recovery_effect(start_key, &spec),
                registry_local_executor(self.stores.registry.clone())
                    .with_process_env_store(Arc::clone(&self.stores.env_store))
                    .with_process_starts(
                        Arc::clone(&self.stores.start_ledger),
                        Arc::clone(&self.stores.clock),
                    ),
            )
            .await
    }

    fn submissions(&self) -> usize {
        self.context.started.lock_recover().len()
    }

    async fn start_obligation(&self, process_id: &ProcessId) -> lash_core::store::ObligationState {
        self.stores
            .start_ledger
            .state(&lash_core::store::process_start_obligation_id(process_id))
            .await
            .expect("read the start obligation")
            .expect("registration armed the start obligation")
    }
}

fn started_record(
    outcome: RuntimeEffectOutcome,
) -> (
    lash_core::ProcessRecord,
    lash_core::ProcessRegistrationOutcome,
) {
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start {
            record,
            disposition,
        },
    } = outcome
    else {
        panic!("wrong start outcome")
    };
    (*record, disposition)
}

/// FIG-4127: the cancellation lands on the registration step's await after
/// the row committed. The start goes on with the row its key holds and
/// submits it, as retained, so nothing may compensate it.
#[tokio::test]
pub(super) async fn a_cancel_at_the_registration_await_still_submits_the_registered_row() {
    let start = CancelledStart::new().await;
    start
        .context
        .cancel_after_next_run("process-start-register");

    let outcome = start
        .run("engine-cancel-at-registration")
        .await
        .expect("a registered start outlives its invocation's cancellation");
    let (record, disposition) = started_record(outcome);

    let stored = the_only_process(start.stores.registry.as_ref()).await;
    assert_eq!(record.id, stored.id);
    assert_eq!(
        disposition,
        lash_core::ProcessRegistrationOutcome::Existing,
        "a row whose registration answer was lost is taken as retained"
    );
    assert_eq!(start.submissions(), 1, "the registered row is submitted");
    assert!(stored.external_ref.is_some() && stored.cancel_request.is_none());
    assert_eq!(
        start.start_obligation(&stored.id).await,
        lash_core::store::ObligationState::Delivered,
        "the start claims and settles its own delivery"
    );
}

/// FIG-4127, FIG-4131: the cancellation lands on the claim's await after the
/// claim was taken, and withholds the token it answered. The start still
/// submits the run, and the claim step run again re-derives the token and
/// takes its own claim back, so the start settles the row `Delivered` before
/// it returns — one claim, no lapse, no relay pass.
#[tokio::test]
pub(super) async fn a_cancel_at_the_claim_await_still_submits_the_start() {
    let start = CancelledStart::new().await;
    start.context.cancel_after_next_run("process-start-claim");
    let began_ms = start.stores.clock.timestamp_ms();

    let outcome = start
        .run("engine-cancel-at-claim")
        .await
        .expect("a claimed start outlives its invocation's cancellation");
    let (record, _) = started_record(outcome);

    assert_eq!(
        start.submissions(),
        1,
        "the claimed row is submitted; the call's cancel reaches it only through a run"
    );
    let stored = the_only_process(start.stores.registry.as_ref()).await;
    assert_eq!(record.id, stored.id);
    assert!(stored.external_ref.is_some() && stored.cancel_request.is_none());
    assert!(
        start.stores.clock.timestamp_ms().saturating_sub(began_ms)
            < lash_core::runtime::drive::relay::RelayPolicy::default().claim_ttl_ms,
        "the start ran inside one claim lapse"
    );
    assert_eq!(
        start
            .stores
            .start_ledger
            .standing(&lash_core::store::process_start_obligation_id(&stored.id))
            .await
            .expect("read the start obligation"),
        Some(lash_core::store::ObligationStanding {
            state: lash_core::store::ObligationState::Delivered,
            attempts: 1,
        }),
        "the rerun claim step takes back its own claim and the start delivers the row"
    );
}

/// FIG-4128: the cancellation lands on the send's await. The send was
/// journaled before that await, so the run may be executing: nothing is
/// compensated, and the claim goes back to the relay.
#[tokio::test]
pub(super) async fn a_cancel_at_the_send_await_is_no_refusal() {
    let start = CancelledStart::new().await;
    start.context.cancel_next_process_workflow_start();

    let outcome = start
        .run("engine-cancel-at-send")
        .await
        .expect("a start whose send was journaled returns its record");
    let (record, _) = started_record(outcome);

    let stored = the_only_process(start.stores.registry.as_ref()).await;
    assert_eq!(record.id, stored.id);
    assert!(
        !stored.is_terminal() && stored.cancel_request.is_none(),
        "a journaled send the cancellation interrupted is not compensated: {stored:?}"
    );
    assert_eq!(
        start.start_obligation(&stored.id).await,
        lash_core::store::ObligationState::Due,
        "the claim goes back due for the relay, whose repeat coalesces"
    );
}

/// FIG-4128: a refused submission compensates with `StartFailed`, which is
/// terminal on the spot only for a row nothing runs. A row another delivery
/// already started keeps the request standing, and the request reaches that
/// run through its `cancel` handler.
#[tokio::test]
pub(super) async fn a_start_failed_cancel_reaches_a_run_already_holding_the_row() {
    let start = CancelledStart::new().await;
    start
        .context
        .start_elsewhere_then_refuse_next(start.stores.registry.clone());

    start
        .run("start-failed-with-a-live-run")
        .await
        .expect_err("the refused submission reaches the caller as an error");

    let stored = the_only_process(start.stores.registry.as_ref()).await;
    assert!(
        !stored.is_terminal(),
        "a row a run holds is not terminal on the spot: {stored:?}"
    );
    assert_eq!(
        stored
            .cancel_request
            .as_deref()
            .map(|request| request.origin),
        Some(lash_core::CancelOrigin::StartFailed)
    );
    let delivered = start.context.cancelled.lock_recover().clone();
    assert_eq!(
        delivered
            .iter()
            .map(|request| (request.process_id.clone(), request.request.origin))
            .collect::<Vec<_>>(),
        [(stored.id.clone(), lash_core::CancelOrigin::StartFailed)],
        "the standing StartFailed request is delivered to the live run"
    );
}
