//! A logical Run closes its park from any admitted physical turn.

use super::*;

/// A commit of `run`'s physical turn `turn` over `state` that ends the run,
/// as the runtime writes it: the run's terminal evidence in the head
/// transaction.
pub(in crate::conformance) fn run_final_commit(
    state: &crate::RuntimeSessionState,
    run: &TurnId,
    turn: &TurnId,
    ordinal: u32,
) -> crate::RuntimeCommit {
    let operation = crate::OperationId::turn(state.session_id.clone(), turn.clone(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("nodes");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state, graph, operation,
    )
    .expect("commit");
    commit.run_terminal = Some(Box::new(RunTerminalWrite {
        run: run.clone(),
        commit: TurnCommitId::new(run.clone(), ordinal),
        turn: turn.clone(),
        outcome: crate::store::RunCommittedOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }));
    commit
}

/// B1: a run that parks on a later physical turn — a frame switch's
/// follow-on turn — is still one run. The commit that ends it clears its
/// park whichever physical turn committed, so the run is never parked and
/// terminal at once: the verbs answer `NotParked` and nothing is left for a
/// drain to count.
pub async fn a_run_parked_on_a_later_physical_turn_is_cleared_by_its_commit(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    // Keep this call site on production admission; the shared real-root fixture
    // can absorb it when its independent construction change lands.
    async fn admit_root(
        parts: &ShiftParts,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
        name: &str,
    ) -> Admitted {
        let request = parts.request(name);
        on_tier(runner, parts, move |mut runtime, scope| {
            let request = request.clone();
            Box::pin(async move {
                super::super::shift_admission::admitted(
                    lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                        .await
                        .expect("the real root admits the input"),
                )
            })
        })
        .await
    }
    let parts = ShiftParts::new(prefix, "later-physical-park", &host, &stores, 8).await;
    let input = parts
        .enqueue("first", Some("later-physical-park-run"))
        .await;
    let admitted = admit_root(&parts, &runner, "park-root").await;
    let root = admitted.root().clone();
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
    // The admitted execution retains its live fence across its physical turns.
    let f = Fixture {
        parts: admitted.parts,
        factory: stores.session_store_factory(),
        stores: Arc::clone(&stores),
        intents: stores.obligation_ledger(ObligationKind::ControlIntent),
        run: admitted.run,
        input: admitted.input,
        park,
    };
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &redrive).await;
    assert!(
        f.park().await.is_some(),
        "the final commit still owes the park's closure"
    );

    let ShiftEpochSeal::Sealed(fence) = &root.seal else {
        panic!("the resumed root owns its fence: {:?}", root.seal);
    };
    let turn = PhysicalTurn::derive_turn_id(&f.run, 1);
    let address = lash_core::facade_support::TurnAddress::new(&f.parts.session_id, &turn);
    let effect_host = f.parts.host.control.effect_host.as_ref();
    let control = lash_core::runtime::turn_control::ActiveTurnControl::new(
        effect_host.await_event_resolver(),
        address.clone(),
    )
    .await
    .expect("the physical turn's cancellation authority");
    let observed = f
        .parts
        .store
        .turn_cancel_request_intent(&address)
        .await
        .expect("the physical turn's durable intent");
    let authorization = control
        .closure_authorization(
            effect_host.turn_control_binding_id(),
            address.execution_scope(),
            fence,
            observed.clone(),
            None,
            None,
        )
        .expect("the admitted final closure");
    let settlement = control.settle_admitted_intent(authorization, None);
    let mut commit = run_final_commit(&f.parts.initial_state(), &f.run, &turn, 1);
    commit.shift_fence = Some(Box::new(fence.clone()));
    commit.interrupted_turn = Some(InterruptedTurnClosure {
        settlement,
        observed_intent: observed,
        admitted_intent: Some(root.cancel_intent.clone()),
    });
    f.parts
        .store
        .commit_runtime_state(commit)
        .await
        .expect("the run's final commit on its second physical turn");
    assert!(
        f.factory
            .run_terminal(&f.parts.session_id, &f.run)
            .await
            .expect("terminal")
            .is_some()
    );
    assert_eq!(
        f.park().await,
        None,
        "the run's commit clears its park whichever physical turn committed"
    );
    assert!(matches!(
        f.verb(RunVerb::Cancel).await,
        Err(RunIntentRefused::NotParked)
    ));
}
