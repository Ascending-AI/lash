//! A cancelled turn never completes input it withheld from its terminal
//! checkpoint (FIG-3531).
//!
//! Input claimed at `BeforeCompletion` is withheld from that checkpoint's
//! delivery for a follow-on turn (FIG-3157). An Immediate cancel means that
//! follow-on never runs and the model never saw the input, so the cancelled
//! turn settles it through the cancellation's undelivered disposition exactly
//! as it settles an unclaimed active-turn row: deferred by default, with the
//! same `TurnCancelAffectedInput` record, and delivered once by the next turn.
//!
//! The law drives a real runtime turn over the supplied durable store and
//! reads the outcome back only through surfaces every backend already owes.

use super::direct_turn_acceptance::{acceptance_runtime, direct_input, text_response};
use crate::admit;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

const SESSION_ID: &str = "root";
const WITHHELD_TEXT: &str = "also check the tests";

/// Cancels the running turn the moment its terminal checkpoint has claimed
/// active-turn input: the claim is taken and withheld, and the Stop lands
/// before the effect loop returns.
struct StopAfterTerminalClaim {
    inner: Arc<dyn crate::RuntimePersistence>,
    stop: CancellationToken,
    withheld_claims: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for StopAfterTerminalClaim {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        turn_id: &TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::TurnInputClaim>,
            Option<crate::QueuedWorkClaim>,
        ),
        crate::StoreError,
    > {
        let claims = self
            .inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await?;
        if matches!(checkpoint, crate::CheckpointKind::BeforeCompletion)
            && claims
                .0
                .as_ref()
                .is_some_and(|claim| !claim.inputs.is_empty())
        {
            self.withheld_claims.fetch_add(1, Ordering::SeqCst);
            self.stop.cancel();
        }
        Ok(claims)
    }
}

/// An Immediate cancel that lands after the terminal checkpoint claimed and
/// withheld inject-now input defers that input — never completes it — records
/// it on the cancellation exactly as it records an unclaimed row, and the next
/// turn delivers it exactly once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn immediate_cancel_defers_withheld_inject_now_input(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let session_id = SessionId::from(SESSION_ID);
    let cancelled_turn_id = TurnId::from(format!("{prefix}-withheld-cancelled"));
    let next_turn_id = TurnId::from(format!("{prefix}-withheld-next"));
    let stop = CancellationToken::new();
    let decorated = Arc::new(StopAfterTerminalClaim {
        inner: Arc::clone(&store),
        stop: stop.clone(),
        withheld_claims: AtomicUsize::new(0),
    });
    let runtime_store: Arc<dyn crate::RuntimePersistence> = decorated.clone();

    let requests = Arc::new(std::sync::Mutex::new(Vec::<crate::LlmRequest>::new()));
    let provider = {
        let requests = Arc::clone(&requests);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                let requests = Arc::clone(&requests);
                async move {
                    requests.lock().expect("request log").push(request);
                    Ok(text_response("summary of the repo"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut runtime = acceptance_runtime(
        &runtime_store,
        &effect_host,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;

    // "also check the tests", sent while the turn runs: inject-now input the
    // terminal checkpoint claims and withholds for a follow-on turn.
    let withheld = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &session_id,
            crate::TurnInputIngress::active_turn(
                &cancelled_turn_id,
                crate::TurnInputCheckpointBoundary::BeforeCompletion,
            ),
            crate::TurnInput::text(WITHHELD_TEXT),
        ))
        .await
        .expect("enqueue inject-now input for the running turn");
    let input_id = withheld.input_id.clone();

    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(
            SESSION_ID,
            &cancelled_turn_id,
        )))
        .expect("scope the cancelled turn");
    let run = runtime
        .stream_turn_with_agent_frames(
            direct_input(&cancelled_turn_id, "summarise the repo"),
            crate::TurnOptions::new(stop, scope),
        )
        .await
        .expect("the cancelled turn commits");
    let outcomes = run
        .turns
        .iter()
        .map(|turn| format!("{:?}", turn.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        run.turns.len(),
        1,
        "a cancelled turn starts no follow-on for its withheld input: {outcomes:?}"
    );
    let cancelled = run.turns.into_iter().next().expect("the cancelled turn");

    assert_eq!(
        decorated.withheld_claims.load(Ordering::SeqCst),
        1,
        "the terminal checkpoint must claim and withhold the inject-now input"
    );
    let evidence = match &cancelled.outcome {
        crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("expected an Immediate cancel, got {other:?}"),
    };
    assert_eq!(evidence.mode, crate::TurnCancelMode::Immediate);
    assert_eq!(evidence.undelivered, crate::TurnCancelDisposition::Defer);
    assert!(
        requests
            .lock()
            .expect("request log")
            .iter()
            .all(|request| !format!("{request:?}").contains(WITHHELD_TEXT)),
        "the cancelled turn's model never saw the withheld input"
    );

    // Never completed: no application settles it, and the row is still
    // pending, deferred to the next turn.
    let applications = store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read applications");
    assert!(
        applications
            .iter()
            .all(|application| application.input_id != input_id),
        "the cancelled turn must not settle the withheld input as completed: {applications:?}"
    );
    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("read pending inputs");
    let row = pending
        .iter()
        .find(|pending| pending.input.input_id == input_id)
        .expect("the withheld input stays pending");
    assert_eq!(row.input.state, crate::TurnInputState::DeferredNextTurn);

    // The same record an unclaimed row gets, on the turn result and on the
    // durable cancellation.
    let expected = vec![crate::TurnCancelAffectedInput {
        input_id: input_id.clone(),
        payload: crate::TurnInput::text(WITHHELD_TEXT),
        disposition: crate::TurnCancelDisposition::Defer,
    }];
    assert_eq!(
        cancelled.turn_cancel_input_outcome.affected_inputs, expected,
        "the turn result records the deferred input"
    );
    let record = store
        .turn_cancel_request(&crate::TurnAddress::new(&session_id, &cancelled_turn_id))
        .await
        .expect("read the cancellation record")
        .expect("the Immediate cancel is durable");
    assert_eq!(
        record.outcome.map(|outcome| outcome.affected_inputs),
        Some(expected),
        "the durable cancellation records the deferred input"
    );

    // Deferred work drives the next turn, exactly once.
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(
            SESSION_ID,
            &next_turn_id,
        )))
        .expect("scope the next turn");
    runtime
        .stream_turn(
            direct_input(&next_turn_id, "carry on"),
            crate::TurnOptions::new(CancellationToken::new(), scope),
        )
        .await
        .expect("the next turn commits");
    let applications = store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read applications after the next turn")
        .into_iter()
        .filter(|application| application.input_id == input_id)
        .collect::<Vec<_>>();
    assert_eq!(
        applications.len(),
        1,
        "the next turn delivers the deferred input exactly once"
    );
    assert_eq!(applications[0].turn_id, next_turn_id);
    assert!(
        store
            .list_pending_turn_inputs(&session_id)
            .await
            .expect("read pending inputs after the next turn")
            .iter()
            .all(|pending| pending.input.input_id != input_id),
        "nothing withheld is left pending once the next turn commits"
    );
}
