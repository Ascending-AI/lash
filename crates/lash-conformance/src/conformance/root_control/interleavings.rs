//! FIG-4626's actors, and a send against a redrive's settle, in every order
//! of their store calls (FIG-4716).

use super::*;
use crate::interleave::Explorer;
use lash_core::testing::{Outcome, Phase};

/// What one actor of the reconcile exploration did.
enum Reconciled {
    Recorded(EngineParkRecorded),
    /// The operator's redrive, when the root showed parked.
    Redrove(Option<ControlIntent>),
}

/// FIG-4626 in every order: two reconcile passes over one root's stopped
/// child, and an operator who redrives the root once it shows parked. The
/// actors meet at the park and at the redrive intent, so they are held before
/// each read and write of either. Whatever the order, the root holds one park
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
        StoreOp::open_root_intent.into(),
    ]);
    let mut redriven = 0;
    while let Some(mut schedule) = explorer.next_schedule() {
        let name = format!("explore-reconciles-{}", schedule.index());
        let admitted = AdmittedRoot::new(prefix, &name, &host, &stores).await;
        let factory = stores.session_store_factory();
        let clock = Arc::clone(&admitted.parts.host.clock);
        let session = admitted.parts.session_id.clone();
        let root = admitted.root.clone();
        let child = ParkTarget::RootChild {
            session: session.clone(),
            root: root.clone(),
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
            lash_core::drive::StoreParkRecovery::new(passes[0].as_ref(), clock.as_ref()),
            lash_core::drive::StoreParkRecovery::new(passes[1].as_ref(), clock.as_ref()),
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
            let request = RootIntentRequest {
                session_id: session.clone(),
                root: root.clone(),
                park: park.park_id,
                verb: RootVerb::Redrive,
            };
            Reconciled::Redrove(Some(
                operator
                    .open_root_intent(&request, 2)
                    .await
                    .expect("redrive the parked root"),
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
            .expect("two passes over a stopped child park its root");
        assert_eq!(park.turn_id, root);
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
            "the pass whose write opened the park parked the root"
        );
        let other = recorded[1 - opened];
        let Some(redrive) = redrive else {
            assert_eq!(park.resume_intent, None);
            assert_eq!(
                *other,
                EngineParkRecorded::AttachedToExisting(park.park_id),
                "the other pass found the root parked"
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
        assert_eq!(
            resumes, 1,
            "the engine resumed the root's stopped work once"
        );
    }
    assert!(redriven > 0, "no schedule admitted the redrive");
}

/// What one actor of the send exploration did.
enum Raced {
    Drove(Result<DriveOutcome, DriveAbort>),
    Settled(ControlIntentState),
}

/// D15 in every order: a send's drive against the settle of the redrive its
/// session's parked root names. The drive is held before each park read and
/// each root admission it makes through its session store, and the settle
/// before it claims and before it acknowledges the intent. An engine retries
/// a refused drive, so the drive is bounded to two steps ahead of the settle.
/// Whatever the order, the drive admits no root before the redrive is
/// acknowledged, and afterwards the parked root runs once and the send lands
/// behind it.
pub async fn every_order_of_a_send_and_a_redrives_settle_admits_nothing_ahead_of_the_redrive(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut explorer = Explorer::new("send-versus-redrive").holding(&[
        StoreOp::load_turn_park.into(),
        StoreOp::admit_root.into(),
        StoreOp::claim_intent_application.into(),
        StoreOp::acknowledge_intent.into(),
    ]);
    let mut met_unsettled = 0;
    while let Some(mut schedule) = explorer.next_schedule() {
        let name = format!("explore-send-{}", schedule.index());
        let mut f = Fixture::new(prefix, &name, &host, &stores).await;
        let send_root = format!("{name}-send");
        let send = f.parts.enqueue("racing send", Some(&send_root)).await;
        let intent = f.verb(RootVerb::Redrive).await.expect("redrive");
        assert!(f.owed(intent.id).await);
        f.parts.store = schedule.actor("send", Arc::clone(&f.parts.store));
        f.factory = schedule.actor("redrive", Arc::clone(&f.factory));
        schedule.yields_after("send", 2);
        let (work, close) = f.control(false, false);
        let did = schedule
            .run(vec![
                (
                    "send",
                    Box::pin(async { Raced::Drove(drive_result(&f, &runner, "racing").await) }),
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
            .position(by("send", StoreOp::admit_root, Phase::Before))
            .expect("the settled drive admits the parked root");
        assert!(
            admitted > acknowledged,
            "the send's drive admitted a root while the redrive was unsettled"
        );
        if trace[..acknowledged]
            .iter()
            .any(by("send", StoreOp::load_turn_park, Phase::After))
        {
            met_unsettled += 1;
        }

        match drove {
            Ok(landed) => assert_eq!(landed.stop, DriveStop::Idle),
            // A tier that answers the refusal instead of retrying it.
            Err(DriveAbort::Retry(refusal)) => {
                assert_eq!(
                    refusal.code,
                    crate::RuntimeErrorCode::SessionRedriveUnsettled,
                    "{refusal:?}"
                );
                assert_eq!(
                    drive(&f, &runner, "after-settle").await.stop,
                    DriveStop::Idle
                );
            }
            Err(abort) => panic!("the refusal is retryable, never a failed turn: {abort:?}"),
        }
        assert_eq!(
            f.parts.applications().await,
            vec![
                (f.input.clone(), f.root.clone()),
                (send, TurnId::from(send_root))
            ],
            "the held input runs the root once, then the send lands"
        );
        assert_eq!(
            f.parts.calls(),
            2,
            "the parked root runs once and the send's root once"
        );
        assert!(
            f.park().await.is_none(),
            "the root's commit cleared its park"
        );
        runner.scenario_finished().await;
    }
    assert!(
        met_unsettled > 0,
        "no schedule put the send's admission ahead of the settle"
    );
}
