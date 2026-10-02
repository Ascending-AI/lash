//! FIG-4626's actors, and a send against a redrive's settle, in every order
//! of their store calls (FIG-4716).

use super::*;
use crate::interleave::Explorer;
use lash_core::testing::{Outcome, Phase};

/// What one actor of the reconcile exploration did.
enum Reconciled {
    Recorded(EngineParkRecorded),
    /// The operator's redrive, when the run showed parked.
    Redrove(Option<ControlIntent>),
}

/// FIG-4626 in every order: two reconcile passes over one run's stopped
/// child, and an operator who redrives the run once it shows parked. The
/// actors meet at the park and at the redrive intent, so they are held before
/// each read and write of either. Whatever the order, the run holds one park
/// that counts one refusal, and no reconcile write settles or clears a
/// redrive: its engine half still runs and resumes the child once.
pub async fn every_order_of_two_child_reconciles_and_a_redrive_counts_one_refusal_and_keeps_the_redrive_open(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut explorer = Explorer::new("child-reconciles-and-redrive").holding(&[
        StoreOp::load_turn_park.into(),
        StoreOp::load_intent.into(),
        StoreOp::record_turn_park.into(),
        StoreOp::open_run_intent.into(),
    ]);
    let mut redriven = 0;
    while let Some(mut schedule) = explorer.next_schedule() {
        let name = format!("explore-reconciles-{}", schedule.index());
        let admitted = AdmittedRun::new(prefix, &name, &host, &stores).await;
        let factory = stores.session_store_factory();
        let clock = Arc::clone(&admitted.parts.host.clock);
        let session = admitted.parts.session_id.clone();
        let run = admitted.run.clone();
        let child = ParkTarget::RunChild {
            session: session.clone(),
            run: run.clone(),
        };
        let stopped = Execution { stopped: true };
        let reasons = [
            ParkReason::engine_retry_exhausted(8, None, "one pass".into()),
            ParkReason::engine_retry_exhausted(9, None, "another pass".into()),
        ];
        let passes: [Arc<dyn crate::DeploymentStore>; 2] = [
            schedule.actor("a", Arc::clone(&factory)),
            schedule.actor("b", Arc::clone(&factory)),
        ];
        let operator: Arc<dyn crate::DeploymentStore> = schedule.actor("r", Arc::clone(&factory));
        let writers = [
            lash_core::shift::StoreParkRecovery::new(passes[0].as_ref(), clock.as_ref()),
            lash_core::shift::StoreParkRecovery::new(passes[1].as_ref(), clock.as_ref()),
        ];
        let pass = |index: usize| {
            let (writer, child, reason, stopped) =
                (&writers[index], &child, &reasons[index], &stopped);
            async move { Reconciled::Recorded(held_pass(writer, child, reason, stopped).await) }
        };
        let redrive = async {
            let Some(park) = operator.load_turn_park(&session).await.expect("park read") else {
                return Reconciled::Redrove(None);
            };
            let request = RunIntentRequest {
                session_id: session.clone(),
                run: run.clone(),
                park: park.park_id,
                verb: RunVerb::Redrive,
            };
            Reconciled::Redrove(Some(
                operator
                    .open_run_intent(&request, 2)
                    .await
                    .expect("redrive the parked run"),
            ))
        };
        let did = schedule
            .run(vec![
                ("a", Box::pin(pass(0))),
                ("b", Box::pin(pass(1))),
                ("r", Box::pin(redrive)),
            ])
            .await;
        let [
            Reconciled::Recorded(first),
            Reconciled::Recorded(second),
            Reconciled::Redrove(redrive),
        ] = &did[..]
        else {
            panic!("each actor answers its own outcome");
        };

        let park = factory
            .load_turn_park(&session)
            .await
            .expect("park")
            .expect("two passes over a stopped child park its run");
        assert_eq!(park.turn_id, run);
        assert_eq!(
            (park.attempts, park.since_ms),
            (1, park.last_refused_ms),
            "the passes count one refusal"
        );
        let opened = reasons
            .iter()
            .position(|reason| *reason == park.reason)
            .expect("the park carries one pass's reason");
        let recorded = [first, second];
        assert_eq!(
            *recorded[opened],
            EngineParkRecorded::Parked(park.park_id),
            "the pass whose write opened the park parked the run"
        );
        let other = recorded[1 - opened];
        let Some(redrive) = redrive else {
            assert_eq!(park.resume_intent, None);
            assert_eq!(
                *other,
                EngineParkRecorded::AttachedToExisting(park.park_id),
                "the other pass found the run parked"
            );
            continue;
        };
        redriven += 1;
        assert_eq!(
            park.resume_intent,
            Some(redrive.id),
            "no reconcile write clears the redrive from the park"
        );
        assert!(
            *other == EngineParkRecorded::AttachedToExisting(park.park_id)
                || *other == EngineParkRecorded::Redriven,
            "the other pass leaves the park to its first writer or to the redrive: {other:?}"
        );
        let f = Fixture::parked(admitted, &stores, park).await;
        assert!(
            f.owed(redrive.id).await,
            "a reconcile never acknowledges a redrive whose engine half has not run"
        );
        let (work, close) = f.control(false, false);
        assert!(matches!(
            f.apply(&work, &close, redrive).await,
            ControlIntentState::Acknowledged { .. }
        ));
        let resumes = work
            .0
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| **event == "resume")
            .count();
        assert_eq!(resumes, 1, "the engine resumed the run's stopped work once");
    }
    assert!(redriven > 0, "no schedule admitted the redrive");
}

/// What one actor of the send exploration did.
enum Raced {
    Drove(Result<ShiftOutcome, ShiftAbort>),
    Settled(ControlIntentState),
}

/// D15 in every order: a send's shift against the settle of the redrive its
/// session's parked run names. The shift is held before each park read and
/// each run admission it makes through its session store, and the settle
/// before it claims and before it acknowledges the intent. An engine retries
/// a refused shift, so the shift is bounded to two steps ahead of the settle.
/// Whatever the order, the shift admits no run before the redrive is
/// acknowledged, and afterwards the parked run executes once and the send lands
/// behind it.
pub async fn every_order_of_a_send_and_a_redrives_settle_admits_nothing_ahead_of_the_redrive(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut explorer = Explorer::new("send-versus-redrive").holding(&[
        StoreOp::load_turn_park.into(),
        StoreOp::admit_run.into(),
        StoreOp::claim_intent_application.into(),
        StoreOp::acknowledge_intent.into(),
    ]);
    let mut met_unsettled = 0;
    while let Some(mut schedule) = explorer.next_schedule() {
        let name = format!("explore-send-{}", schedule.index());
        let mut f = Fixture::new(prefix, &name, &host, &stores).await;
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
            .position(by("send", StoreOp::admit_run, Phase::Before))
            .expect("the settled shift admits the parked run");
        assert!(
            admitted > acknowledged,
            "the send's shift admitted a run while the redrive was unsettled"
        );
        if trace[..acknowledged]
            .iter()
            .any(by("send", StoreOp::load_turn_park, Phase::After))
        {
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
