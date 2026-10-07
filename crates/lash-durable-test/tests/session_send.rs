//! Queued withdrawal and running cancellation through the facade (ADR 0039,
//! ADR 0101 A2), ported from the L9c and L9e laws retired with their host.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use served::{Tier, WATCHDOG, World};

const RUNNING: &str = "keep me running";
const QUEUED: &str = "withdraw me";

/// The provider holds the first run until its cancellation unwinds it.
/// Every entry is counted, including an erroneous admission of the queue.
#[derive(Default)]
struct HeldModel {
    calls: AtomicUsize,
    started: tokio::sync::Notify,
}

impl HeldModel {
    fn provider(self: &Arc<Self>) -> lash_core::facade_support::ProviderHandle {
        let model = Arc::clone(self);
        lash_core::testing::TestProvider::builder()
            .kind("cancel-held-model")
            .complete(move |request: lash_core::llm::types::LlmRequest| {
                let model = Arc::clone(&model);
                async move {
                    model.calls.fetch_add(1, Ordering::SeqCst);
                    model.started.notify_one();
                    if format!("{:?}", request.messages).contains(QUEUED) {
                        return Ok(served::text(&request, "the withdrawn input ran"));
                    }
                    std::future::pending().await
                }
            })
            .build()
            .into_handle()
    }
}

async fn world(tier: Tier) -> Option<(World, lash::DurableSession, Arc<HeldModel>)> {
    let model = Arc::new(HeldModel::default());
    let world = World::with_model(tier, Vec::new(), model.provider(), |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await?;
    let session = world.session("withdraw-cancel", served::spec(8)).await;
    Some((world, session, model))
}

async fn assert_stopped(
    session: &lash::DurableSession,
    running: lash::SendHandle,
    queued: lash::SendHandle,
    model: &HeldModel,
) {
    let input = queued.input_id().clone();
    let (running, queued) = tokio::join!(running.outcome(), queued.outcome());
    let running = running.expect("the running send settles");
    let queued = queued.expect("the queued send settles");
    assert_eq!(running.status(), lash::TurnStatus::Cancelled);
    assert!(
        running.output().is_some(),
        "the running run commits its stop"
    );
    assert!(matches!(queued, lash::SendOutcome::Withdrawn { .. }));
    assert!(queued.output().is_none(), "withdrawal applies no turn");
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "the queue never calls the model"
    );
    let applied = session
        .turn_input_applications()
        .await
        .expect("read applications");
    assert!(
        applied
            .iter()
            .all(|application| application.input_id != input),
        "the withdrawn input is never applied: {applied:?}"
    );
    let transcript = session
        .transcript()
        .await
        .expect("read the committed transcript");
    assert!(
        !format!("{transcript:?}").contains(QUEUED),
        "the queue never reaches history"
    );
}

/// Draft reconciliation reads the retained terminal, without addressing a run.
async fn withdrawal_tombstone(
    session: &lash::DurableSession,
    input: &lash::InputId,
) -> lash_core::PendingTurnInput {
    let mut receipts = session
        .cancel_pending_turn_inputs([lash_core::PendingTurnInputCancelTarget::input_id(
            input.to_string(),
        )])
        .await
        .expect("read the cancelled tombstone through draft reconciliation");
    let receipt = receipts.pop().expect("the input has one receipt");
    let lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input) = receipt.outcome else {
        panic!("the withdrawal tombstone is retained");
    };
    input
}

/// Withdraw a queued run by its host id, before it has any admitted run row.
/// The facade retains the typed withdrawal and repeated input cancellation
/// reads the same durable tombstone; cancelling the admitted input stops its run.
async fn withdraw_while_queued_vs_cancel_while_running(tier: Tier) {
    let Some((world, session, model)) = world(tier).await else {
        return;
    };
    tokio::time::timeout(WATCHDOG, async {
        let running = session
            .send(lash::TurnInput::text(RUNNING))
            .await
            .expect("send first");
        model.started.notified().await;
        let id = lash::TurnId::try_from("queued-run".to_owned()).expect("a host id");
        let queued = session
            .send(lash::TurnInput::text(QUEUED))
            .id(id.clone())
            .await
            .expect("send second");
        assert_eq!(
            queued.run().await.expect("read binding"),
            None,
            "the queue is unadmitted"
        );
        let receipt = session
            .run(lash::RunId::from(id))
            .cancel()
            .await
            .expect("cancel the queued run");
        assert!(
            matches!(&receipt, lash::CancelReceipt::Withdrawn { run, input: Some(input) }
                if run == queued.id().expect("the host named the queued run") && input == queued.input_id()),
            "a queued run withdraws: {receipt:?}"
        );
        let before = withdrawal_tombstone(&session, queued.input_id()).await;
        let terminal = before.terminal().expect("the withdrawal has a terminal");
        assert_eq!(terminal.cause, lash_core::store::IngressTerminalCause::Cancelled);
        let again = queued.cancel().await.expect("repeat the queued cancellation");
        assert!(matches!(again, lash::CancelReceipt::UnknownOrRevoked),
            "a withdrawn run accepts no new cancel: {again:?}");
        let after = withdrawal_tombstone(&session, queued.input_id()).await;
        assert_eq!(after.terminal(), Some(terminal), "the tombstone is unchanged");
        let receipt = running.cancel().await.expect("cancel the admitted input");
        assert!(
            matches!(&receipt, lash::CancelReceipt::Cancelled { receipt, .. }
            if matches!(receipt.outcome, lash::TurnCancelOutcome::Requested(_))),
            "the admitted input addresses its running run: {receipt:?}"
        );
        assert_stopped(&session, running, queued, &model).await;
    })
    .await
    .expect("deadlock watchdog: cancel/withdraw never settled");
    world.shutdown().await;
}

/// Stop both accepted sends: the first commits a cancelled run and the
/// second retains a withdrawal with no output and no new provider call.
async fn cancelling_both_sends_stops_the_running_run_and_withdraws_the_queued_one(tier: Tier) {
    let Some((world, session, model)) = world(tier).await else {
        return;
    };
    tokio::time::timeout(WATCHDOG, async {
        let running = session
            .send(lash::TurnInput::text(RUNNING))
            .await
            .expect("send first");
        model.started.notified().await;
        let queued = session
            .send(lash::TurnInput::text(QUEUED))
            .await
            .expect("send second");
        let running_id = running.input_id().clone();
        assert!(matches!(
            session
                .attach(running_id.clone())
                .cancel()
                .await
                .expect("stop first"),
            lash::CancelReceipt::Cancelled { .. }
        ));
        assert!(matches!(
            queued.cancel().await.expect("withdraw second"),
            lash::CancelReceipt::Withdrawn { .. }
        ));
        assert_stopped(&session, running, queued, &model).await;
        let again = session
            .attach(running_id)
            .cancel()
            .await
            .expect("stop again");
        assert!(
            matches!(again, lash::CancelReceipt::UnknownOrRevoked),
            "the stopped run is already settled: {again:?}"
        );
    })
    .await
    .expect("deadlock watchdog: both sends never settled");
    world.shutdown().await;
}

tiered_laws!(
    withdraw_while_queued_vs_cancel_while_running,
    cancelling_both_sends_stops_the_running_run_and_withdraws_the_queued_one,
);
