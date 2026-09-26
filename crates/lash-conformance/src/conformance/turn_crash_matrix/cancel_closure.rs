//! The turn-cancel closure across a crash, on the tier's turn runner, with the
//! turn run as a root of the session drive (FIG-3600).
//!
//! The reference turn carries a durable after-step cancellation that drops its
//! undelivered active-turn input. The turn honours it and closes: it settles
//! the base and escalation gates, authorizes the closure, and applies the input
//! effects and consumes the authorization in one store write. The law crashes
//! the turn at each of those eight cuts, before and inside each step, and the
//! tier recovers the turn its own way: a fresh runtime over the same store in
//! process, a redelivered invocation replaying its journal on Restate. The
//! drive's recorded admission, seal and claim name the same root on every
//! execution, so a recovery never re-decides from the store the crashed
//! closure write already moved (FIG-3736). Every recovery must finish the
//! same closure once: the cancellation records its
//! input effects with the requested `Drop` disposition, no closure pin
//! survives, and the turn's terminal commit lands.

use super::*;

/// See the module docs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn turn_cancel_closure_recovers_from_a_crash_at_every_cut<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    // One layered host for the whole law: a runtime installs its tool-child
    // host get-or-init, so a host layered afresh per execution would strand
    // every later execution's group children (see `LawSeamHost`).
    let host = LawSeamHost::over(host);
    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    for action in cold_process::ColdProcessTurnAction::CANCEL_CRASH_ACTIONS {
        let point = action
            .point()
            .expect("every cancellation cut names its crash point");
        let scenario = format!("cancel-closure-{}", point_key(&point));
        let identity = ReferenceIdentity::for_scenario(&scenario);
        let store = make(&scenario);
        seed_reference_ingress_for_drive(&store, &identity, &scenario).await;
        let address = crate::TurnAddress::new(&identity.session_id, &identity.turn_id);
        let receipt = crate::TurnWorkDriver::for_session(
            host.host(),
            identity.session_id.to_string(),
            Arc::clone(&store),
        )
        .request_cancel(
            crate::TurnCancelRequest::new(
                address.clone(),
                format!("turn-cancel-closure-crash:{scenario}"),
                Some("turn-cancel-closure-conformance".to_string()),
            )
            .mode(crate::TurnCancelMode::AfterStep)
            .undelivered(crate::TurnCancelDisposition::Drop),
        )
        .await
        .expect("seed a durable after-step cancellation before the turn runs");
        assert!(
            matches!(
                receipt.outcome,
                crate::TurnCancelOutcome::Requested(_)
                    | crate::TurnCancelOutcome::AlreadyRequested(_)
            ),
            "{point:?}: the cancellation is requested: {receipt:?}"
        );

        let control = SeamControl::default();
        control.arm(point.clone());
        let crash = crash_at_armed_point(&control);
        let attempt = |control: SeamControl,
                       lease_timings: crate::LeaseTimings,
                       ends: Option<tokio::sync::mpsc::UnboundedSender<String>>|
         -> crate::ConformanceTurnAttempt {
            let stores = Arc::clone(&stores);
            let store = Arc::clone(&store);
            let host = host.clone();
            let identity = identity.clone();
            let seam = SeamLayer {
                control,
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                journal_faults: None,
            };
            let point = point.clone();
            Arc::new(move |scoped| {
                let stores = Arc::clone(&stores);
                let store = SeamStore::wrap(Arc::clone(&store), seam.control.clone());
                let host = host.clone();
                let identity = identity.clone();
                let seam = seam.clone();
                let ends = ends.clone();
                let point = point.clone();
                Box::pin(async move {
                    let runtime = Box::pin(try_build_runtime_on_host(
                        Arc::clone(&stores),
                        store,
                        &seam,
                        host,
                        &identity,
                        TraceTool::default(),
                        lease_timings,
                    ))
                    .await
                    .expect("build the cancelled reference runtime");
                    let turn = Box::pin(drive_root_on(runtime, seam.over_scoped(scoped))).await;
                    let Some(ends) = ends else {
                        panic!("{point:?}: the armed cancellation cut was never reached: {turn:?}");
                    };
                    let end = crate::ConformanceTurnEnd::of(&turn);
                    let _ = ends.send(format!("{turn:?}"));
                    end
                })
            })
        };
        runner
            .run_turn_until_crash(
                reference_admitted_scope(&identity),
                attempt(control.clone(), crashed_turn_timings(), None),
                crash,
            )
            .await;
        // On the drive the closure write is the root's final commit, which
        // releases the lane with the head: once it landed, no lane is left.
        let lane_held = !(point.operation
            == TurnSeamOperation::Store(StoreOperation::ApplyTurnCancelEffectsAndConsume)
            && point.placement == CrashPlacement::InsideCall);
        wait_for_recovery_lease(&make, &scenario, &point, lane_held).await;
        let recovery = make(&scenario);
        super::super::bind_conformance_session(&recovery, &identity.session_id).await;

        let (ends, mut ended) = tokio::sync::mpsc::unbounded_channel();
        runner
            .run_turn(
                reference_admitted_scope(&identity),
                attempt(
                    SeamControl::default(),
                    nominal_recovery_timings(),
                    Some(ends),
                ),
            )
            .await;
        let ended = ended.recv().await.expect("the recovered turn reported");

        let record = recovery
            .turn_cancel_request(&address)
            .await
            .expect("read the recovered cancellation record")
            .unwrap_or_else(|| panic!("{point:?}: the cancellation survives the crash"));
        let outcome = record.outcome.unwrap_or_else(|| {
            panic!("{point:?}: the recovery records the cancellation's input effects: {ended}")
        });
        assert!(
            !outcome.affected_inputs.is_empty()
                && outcome
                    .affected_inputs
                    .iter()
                    .all(|input| input.disposition == crate::TurnCancelDisposition::Drop),
            "{point:?}: the recovery drops the undelivered active-turn input: {outcome:?}"
        );
        assert!(
            recovery
                .pending_turn_cancel_closure_pins()
                .await
                .expect("read closure pins after recovery")
                .is_empty(),
            "{point:?}: the input effects and the closure consumption land together"
        );
        assert!(
            recovery
                .turn_is_committed(&address)
                .await
                .expect("read the cancelled turn's commit receipt"),
            "{point:?}: the recovered turn commits its cancelled terminal: {ended}"
        );
    }
}
