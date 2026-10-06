//! A send against a redrive's settle in every order of their store calls
//! (FIG-4716).

use super::*;
use crate::ActorContext;
use crate::interleave::Explorer;
use lash_core::testing::{Outcome, Phase};

enum Raced {
    Drove(Result<ShiftOutcome, ShiftAbort>),
    Settled(ControlIntentState),
}

/// D15 in every order: a send's shift against the settle of the redrive its
/// session's parked run names. The shift is held before each admission
/// preparation, which reads the park, and each atomic root admission it makes
/// through its session store (FIG-4848), and the settle
/// before it claims and before it acknowledges the intent. An engine retries
/// a refused shift, so the shift is bounded to two steps ahead of the settle.
/// Whatever the order, the shift admits no run before the redrive is
/// acknowledged, and afterwards the parked run executes once and the send lands
/// behind it.
pub async fn every_order_of_a_send_and_a_redrives_settle_admits_nothing_ahead_of_the_redrive(
    prefix: &str,
    host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut explorer = Explorer::new("send-versus-redrive").holding(&[
        StoreOp::prepare_shift_admission.into(),
        StoreOp::commit_shift_admission.into(),
        StoreOp::claim_intent_application.into(),
        StoreOp::acknowledge_intent.into(),
    ]);
    let mut met_unsettled = 0;
    while let Some(mut schedule) = explorer.next_schedule() {
        let name = format!("explore-send-{}", schedule.index());
        let mut f = Fixture::new_for_execution(prefix, &name, &host, &stores, &runner).await;
        let send_run = format!("{name}-send");
        let send = f.parts.enqueue("racing send", Some(&send_run)).await;
        let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
        assert!(f.owed(intent.id).await);
        f.parts.store = schedule.actor("send", Arc::clone(&f.parts.store));
        f.factory = schedule.actor("redrive", Arc::clone(&f.factory));
        schedule.yields_after("send", 2);
        let (work, close) = f.control(false, false);
        let did = schedule
            .run(vec![
                (
                    "send",
                    Box::pin(async { Raced::Drove(shift_result(&f, &runner, "racing").await) }),
                ),
                (
                    "redrive",
                    Box::pin(async { Raced::Settled(f.apply(&work, &close, &intent).await) }),
                ),
            ])
            .await;
        let [Raced::Drove(drove), Raced::Settled(settled)] = &did[..] else {
            panic!("each actor answers its own outcome");
        };
        assert!(
            matches!(settled, ControlIntentState::Acknowledged { .. }),
            "the redrive settles: {settled:?}"
        );

        let trace = schedule.trace();
        let by = |actor: &'static str, op: StoreOp, phase: Phase| {
            move |call: &lash_core::testing::Call| {
                *call.actor == *actor && call.op == op.into() && call.phase == phase
            }
        };
        let acknowledged = trace
            .iter()
            .position(|call| {
                by("redrive", StoreOp::acknowledge_intent, Phase::After)(call)
                    && call.outcome == Outcome::Returned
            })
            .expect("the settle acknowledged the redrive through its store");
        let admitted = trace
            .iter()
            .position(by("send", StoreOp::commit_shift_admission, Phase::Before))
            .expect("the settled shift admits the parked run");
        assert!(
            admitted > acknowledged,
            "the send's shift admitted a run while the redrive was unsettled"
        );
        if trace[..acknowledged].iter().any(by(
            "send",
            StoreOp::prepare_shift_admission,
            Phase::After,
        )) {
            met_unsettled += 1;
        }

        match drove {
            Ok(landed) => assert_eq!(landed.stop, ShiftStop::Idle),
            // A tier that answers the refusal instead of retrying it.
            Err(ShiftAbort::Retry(refusal)) => {
                assert_eq!(
                    refusal.code,
                    crate::RuntimeErrorCode::SessionRedriveUnsettled,
                    "{refusal:?}"
                );
                assert_eq!(
                    shift(&f, &runner, "after-settle").await.stop,
                    ShiftStop::Idle
                );
            }
            Err(abort) => panic!("the refusal is retryable, never a failed turn: {abort:?}"),
        }
        assert_eq!(
            f.parts.applications().await,
            vec![
                (f.input.clone(), f.run.clone()),
                (send, TurnId::fixture(send_run))
            ],
            "the held input executes the run once, then the send lands"
        );
        assert_eq!(
            f.parts.calls(),
            2,
            "the parked run executes once and the send's run once"
        );
        assert!(
            f.park().await.is_none(),
            "the run's commit cleared its park"
        );
        runner.scenario_finished().await;
    }
    assert!(
        met_unsettled > 0,
        "no schedule put the send's admission ahead of the settle"
    );
}
