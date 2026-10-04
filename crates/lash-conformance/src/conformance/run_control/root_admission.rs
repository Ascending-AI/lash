//! Real root admissions for control laws that resume execution.

use super::*;

impl Fixture {
    pub(super) async fn new_for_execution(
        prefix: &str,
        name: &str,
        host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
    ) -> Self {
        let parts = ShiftParts::new(prefix, name, host, stores, 8).await;
        let input = parts.enqueue("first", Some(&format!("{name}-run"))).await;
        let admitted =
            super::super::run_admission_fixture::admit(&parts, runner, "control-root").await;
        let root = admitted
            .root()
            .expect("atomic admission retains its receipt");
        let ShiftEpochSeal::Sealed(lease) = &root.seal else {
            panic!("the root owns its fence: {:?}", root.seal);
        };
        let admitted = AdmittedRun {
            run: admitted.run().clone(),
            lease: lease.clone(),
            parts,
            input,
        };
        let park = admitted
            .parts
            .store
            .record_turn_park(&TurnParkWrite::refusal(
                admitted.parts.session_id.clone(),
                admitted.run.clone(),
                ParkReason::ReplayDivergence {
                    message: "old build".into(),
                },
                1,
            ))
            .await
            .map(StoreTransition::into_record)
            .expect("park the admitted root");
        Self::parked(admitted, stores, park).await
    }
}

/// A lost resume acknowledgement keeps admission refused until reconciliation
/// settles the intent; the held input and queued send then each run once.
pub async fn a_lost_resume_ack_is_reconciled_before_queued_work_is_admitted(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new_for_execution(prefix, "lost-resume-ack", &host, &stores, &runner).await;
    let send = f
        .parts
        .enqueue("queued send", Some("queued-send-run"))
        .await;
    let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    work.0.lose_resume_reply.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Pending
    ));
    assert!(f.owed(intent.id).await);
    let refused = shift_result(&f, &runner, "before-reconcile").await;
    assert!(
        matches!(refused, Err(ShiftAbort::Retry(ref error))
        if error.code == crate::RuntimeErrorCode::SessionRedriveUnsettled),
        "{refused:?}"
    );
    assert_eq!(f.parts.calls(), 0);
    assert!(f.parts.applications().await.is_empty());

    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(Fixture::intent_pass(&report).delivered, 1);
    assert!(matches!(
        f.intent_state(intent.id).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert!(!f.owed(intent.id).await);
    let resumed = shift(&f, &runner, "after-reconcile").await;
    assert_eq!(resumed.stop, ShiftStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), f.run.clone()),
            (send, TurnId::from("queued-send-run")),
        ]
    );
    assert_eq!(f.parts.calls(), 2);
}
