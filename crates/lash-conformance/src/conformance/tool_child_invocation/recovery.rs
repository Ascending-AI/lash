use pretty_assertions::assert_eq;

use super::*;

/// A single-child group: the recovery leaf alone, parked on its deferred
/// completion key.
fn recovery_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::RuntimeEffectGroup {
    single_leaf_group(
        scope,
        session_id,
        group_key,
        env_ref,
        LEAF_RECOVERY,
        routing,
        cancellation,
    )
}

/// The recovery law: an opener that is not live on this host leaves its child
/// accepted — not failed, not run — and the child completes once the same
/// opener registers on the recovering host (ADR 0099 §1, W1).
///
/// Two shapes of the same statement:
///
/// * On a tier with a durable journal the group outlives the worker that
///   opened it. The crash phase opens it and parks its deferred child, then
///   dies. A successor host whose resolver is wired but whose opener is absent
///   drains the group and reports the child as `NoExecutor` — the typed "not
///   mine", not a failure and not an execution. Registering the same
///   `EffectOpener` on the successor and draining again runs the child to a
///   settlement a reopen serves, and the leaf's body never runs twice: the
///   journaled `Pending` attempt is replayed, the deferred resolver is
///   re-armed, and the out-of-band resolution is what settles it.
/// * On a drain-less tier the observable
///   edge is the first open: with the opener unregistered the open is refused
///   before anything is journaled, and the identical group opens and settles
///   once the opener registers.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unregistered_opener_leaves_the_child_accepted(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-recovery"));
    let turn_id = crate::TurnId::from(format!("{prefix}-recovery-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-recovery-group");
    let process_env_store = (fixture.make_processes)().await.process_env_store();
    let env_ref = crate::testing::process_execution_env_fixture(process_env_store.as_ref()).await;

    let probe_world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    if probe_world.drain.is_none() {
        // A drain-less tier: the routing gate is observable only at first
        // open — the group is refused before anything is recorded, and the
        // same key opens once the opener is live.
        let host = probe_world.host;
        install_child_host(&host, &process_env_store);
        let scoped = host
            .scoped(crate::admit(scope.clone()))
            .expect("the group scope binds");
        let group = recovery_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        );
        let refusal = scoped
            .controller()
            .open_effect_group(group)
            .await
            .expect_err("a child whose opener is not live refuses the first open");
        assert!(
            refusal.to_string().contains("child 0"),
            "the refusal names the child it cannot route: {refusal}"
        );

        let registry = (fixture.make_processes)().await.process_registry();
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let _guard = register_opener(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            registry,
            Arc::clone(&process_env_store),
            opener,
            tokio_util::sync::CancellationToken::new(),
        );
        let mut handle = scoped
            .controller()
            .open_effect_group(recovery_group(
                &scope,
                &session_id,
                &group_key,
                &env_ref,
                ToolChildCompletionRouting::Durable,
                recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
            ))
            .await
            .expect("the identical group opens once the opener is live");
        let key = scenario
            .observation
            .parked_key(&format!("{group_key}-call-0"))
            .await;
        host.resolve_await_event(
            &key,
            crate::Resolution::Ok(serde_json::json!({ "leaf": "recovery", "via": "resolver" })),
        )
        .await
        .expect("the parked child's key resolves");
        let settlement = next_settlement(&scoped, &mut handle, 0).await;
        let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &settlement.outcome
        else {
            panic!("the recovered child settles a tool invocation: {settlement:?}")
        };
        assert!(
            format!("{:?}", outcome.record.output).contains("resolver"),
            "the settled output carries the out-of-band resolution"
        );
        assert_eq!(
            scenario.observation.executions_of("law_recovery").len(),
            1,
            "the leaf body ran exactly once"
        );
        return;
    }

    // The durable tiers: the group outlives the worker that opened it.
    let observation = Arc::new(LawObservation::default());
    let call_id = format!("{group_key}-call-0");
    crashed_world(fixture, {
        let fixture_processes = Arc::clone(&fixture.make_processes);
        let scope = scope.clone();
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let make_processes = Arc::clone(&fixture.make_processes);
        let env_ref = env_ref.clone();
        let observation = Arc::clone(&observation);
        let call_id = call_id.clone();
        let opener = opener.clone();
        move |world| {
            Box::pin(async move {
                let env_store = phase_env_store(&make_processes, &env_ref).await;
                let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
                    definitions: leaf_definitions(),
                    observation: Arc::clone(&observation),
                    session_id: session_id.clone(),
                    intent_target: crate::ProcessId::from("unused-in-recovery"),
                    start_metadata: serde_json::Value::Null,
                });
                let _guard = register_opener(
                    &world.host,
                    &scope,
                    provider,
                    fixture_processes().await.process_registry(),
                    env_store,
                    opener,
                    tokio_util::sync::CancellationToken::new(),
                );
                let scoped = world
                    .host
                    .scoped(crate::admit(scope.clone()))
                    .expect("the group scope binds");
                let handle = scoped
                    .controller()
                    .open_effect_group(recovery_group(
                        &scope,
                        &session_id,
                        &group_key,
                        &env_ref,
                        ToolChildCompletionRouting::Durable,
                        recorded_cancellation_authority(&world.host, &crate::admit(scope.clone()))
                            .await,
                    ))
                    .await
                    .expect("the group opens under the live opener");
                // Wait until the child has parked on its completion key, so
                // what the dead process leaves is a *claimed* unsettled child,
                // not a row that never ran. `park` fires inside the attempt
                // body — one commit before the journaled Pending row lands —
                // so the durable half of the wait is the armed resolver key
                // becoming visible to this host's journal: by then the
                // attempt row the resolver settles is already durable.
                let key = observation.parked_key(&call_id).await;
                await_key_registered(&world.host, &session_id, &key).await;
                // Close the caller's handle the way a finishing turn would:
                // the parked child is a loser under RunToCompletion and stays
                // owned by the host's task until the runtime dies under it.
                scoped
                    .controller()
                    .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                    .await
                    .expect("the caller closes and releases its loser");
            })
        }
    })
    .await;
    assert_eq!(
        observation.executions_of("law_recovery").len(),
        1,
        "the crashed worker ran the leaf body exactly once"
    );

    // The successor: resolver wired, opener absent. The drain reports the
    // child as unrunnable on *this* host — the typed "not mine" — once the
    // dead worker's claim has lapsed.
    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&successor.host, &process_env_store);
    until_claims_lapse(&successor, &group_key).await;
    let report = successor
        .drain
        .as_ref()
        .expect("a durable tier hands out a drain")
        .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
        .await
        .expect("a drain pass over the journaled group runs");
    assert!(
        report.children.iter().all(|child| matches!(
            child.outcome,
            crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
        )),
        "with no live opener the child is reported unrunnable, not run and not failed: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_recovery").len(),
        1,
        "the successor did not re-execute the leaf body"
    );

    // A reopen tolerates the miss the same way: the group opens, the child is
    // not dispatched, and no settlement is served for its rank while the
    // opener stays absent. The reopen runs on a peer host of its own — resolver
    // wired, opener never registered — torn down with its runtime once the
    // observation is made. Its close spawns that host's finalizer, whose
    // obligation pass drives every child the host can run; on the successor
    // that finalizer would still be in flight when the opener registers below,
    // claim the child itself, and leave the reclaiming drain a `LeaseLive` row.
    world_on_own_runtime(fixture, LIVE_LEASE_MS, {
        let scope = scope.clone();
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let make_processes = Arc::clone(&fixture.make_processes);
        let env_ref = env_ref.clone();
        move |peer| {
            Box::pin(async move {
                let env_store = phase_env_store(&make_processes, &env_ref).await;
                install_child_host(&peer.host, &env_store);
                let scoped = peer
                    .host
                    .scoped(crate::admit(scope.clone()))
                    .expect("the group scope binds");
                let mut handle = scoped
                    .controller()
                    .open_effect_group(recovery_group(
                        &scope,
                        &session_id,
                        &group_key,
                        &env_ref,
                        ToolChildCompletionRouting::Durable,
                        recorded_cancellation_authority(&peer.host, &crate::admit(scope.clone()))
                            .await,
                    ))
                    .await
                    .expect("a reopen tolerates a child this host cannot run");
                assert!(
                    tokio::time::timeout(
                        ABSENCE_BUDGET,
                        scoped.controller().await_next_settlement(
                            &mut handle,
                            tokio_util::sync::CancellationToken::new()
                        ),
                    )
                    .await
                    .is_err(),
                    "no settlement is served while the opener is absent: the child is accepted, not run and not failed"
                );
                scoped
                    .controller()
                    .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                    .await
                    .expect("the peer's observing handle closes");
            })
        }
    })
    .await;

    // The opener registers on the successor — the same `EffectOpener`, derived
    // from the same scope — and the next drain runs the child. The journaled
    // `Pending` attempt replays (the body is not re-executed), the deferred
    // resolver is re-armed, and the out-of-band resolution settles it.
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
        definitions: leaf_definitions(),
        observation: Arc::clone(&observation),
        session_id: session_id.clone(),
        intent_target: crate::ProcessId::from("unused-in-recovery"),
        start_metadata: serde_json::Value::Null,
    });
    let registry = (fixture.make_processes)().await.process_registry();
    let _guard = register_opener(
        &successor.host,
        &scope,
        provider,
        registry,
        Arc::clone(&process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let drain = Arc::clone(
        successor
            .drain
            .as_ref()
            .expect("a durable tier hands out a drain"),
    );
    let drained = crate::task::spawn({
        let group_key = group_key.clone();
        async move {
            drain
                .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
                .await
        }
    });
    let key = observation.parked_key(&call_id).await;
    resolve_when_registered(
        &successor.host,
        key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "recovery", "via": "resolver" })),
    )
    .await;
    let report = drained
        .await
        .expect("the drain task joins")
        .expect("the reclaiming drain pass runs");
    assert!(
        report.children.iter().all(|child| matches!(
            child.outcome,
            crate::testing::conformance_support::ChildDrainOutcome::Settled
        )),
        "the reclaimed child settles through the successor's drain: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_recovery").len(),
        1,
        "the journaled Pending attempt replays; the leaf body never runs twice"
    );

    // Journal-visible: a caller reopening the group on the successor is served
    // the rank the drain settled, carrying the out-of-band resolution.
    let scoped = successor
        .host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(recovery_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&successor.host, &crate::admit(scope.clone())).await,
        ))
        .await
        .expect("the successor reopens the drained group");
    let settlement = next_settlement(&scoped, &mut handle, 0).await;
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
        outcome,
        settlement,
    }) = &settlement.outcome
    else {
        panic!("the recovered child settles a tool invocation: {settlement:?}")
    };
    settlement.validate().expect("the settlement validates");
    assert!(
        format!("{:?}", outcome.record.output).contains("resolver"),
        "the settled output carries the out-of-band resolution"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor closes");
}
