use pretty_assertions::assert_eq;

use super::*;

// =============================================================================
// Routing: a live opener that is not the recorded one
// =============================================================================

/// The present-but-foreign half of the routing gate, in both directions
/// (ADR 0099 §1, §3).
///
/// `an_unregistered_opener_leaves_the_child_accepted` proves the absent half:
/// no live opener, no run. This proves the mismatched half — a live opener
/// that is *not* the recorded one still cannot drive the child, whichever way
/// the pair is arranged — because a child's context is reconstructed from its
/// retained request, not lent from whichever opener happens to be registered.
/// The leak this closes is the one where "an opener is live" was authority
/// enough: a reopen under a different opener would have run the child under
/// *that* opener's session, frame and controller.
///
/// On a durable tier the group journals and its children stay accepted while
/// the recorded opener is elsewhere; on the in-memory tier the same gate is
/// observable at open. Either way, when the recorded opener registers the
/// child runs under its *recorded* session — which the leaf reports, so a
/// child that ran under the foreign opener's context instead would be caught
/// rather than merely counted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_foreign_opener_cannot_drive_another_openers_child(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_a = crate::SessionId::from(format!("{prefix}-mismatch-session-a"));
    let session_b = crate::SessionId::from(format!("{prefix}-mismatch-session-b"));
    let scope_a = crate::ExecutionScope::turn(
        session_a.clone(),
        crate::TurnId::from(format!("{prefix}-mismatch-turn-a")),
    );
    let scope_b = crate::ExecutionScope::turn(
        session_b.clone(),
        crate::TurnId::from(format!("{prefix}-mismatch-turn-b")),
    );
    let opener_a = crate::EffectOpener::for_scope(&crate::admit(scope_a.clone()))
        .expect("a turn scope derives an opener");
    let opener_b = crate::EffectOpener::for_scope(&crate::admit(scope_b.clone()))
        .expect("a turn scope derives an opener");
    let group_key_a = format!("{prefix}-mismatch-group-a");
    let group_key_b = format!("{prefix}-mismatch-group-b");
    let env_store = (fixture.make_processes)().await.process_env_store;
    let env_ref = crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
    let observation = Arc::new(LawObservation::default());
    let registry = (fixture.make_processes)().await.registry;

    let provider = |session_id: &crate::SessionId| -> Arc<dyn crate::ToolProvider> {
        Arc::new(LawLeafProvider {
            definitions: leaf_definitions(),
            observation: Arc::clone(&observation),
            session_id: session_id.clone(),
            intent_target: crate::ProcessId::from("unused-in-mismatch"),
            start_metadata: serde_json::Value::Null,
        })
    };
    let group = |scope: &crate::ExecutionScope,
                 session_id: &crate::SessionId,
                 group_key: &str,
                 cancellation: Option<crate::TurnControlBindingId>| {
        single_leaf_group(
            scope,
            session_id,
            group_key,
            &env_ref,
            LEAF_PLAIN,
            ToolChildCompletionRouting::Inline,
            cancellation,
        )
    };

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&world.host, &env_store);

    if world.drain.is_some() {
        // The durable tiers. A first open refuses a child with no runner —
        // only a reopen tolerates one — so the groups are journaled while
        // their own openers are live, parked on their deferred keys, and the
        // worker dies leaving claimed unsettled children.
        for (scope, session_id, group_key, opener) in [
            (&scope_a, &session_a, &group_key_a, &opener_a),
            (&scope_b, &session_b, &group_key_b, &opener_b),
        ] {
            crashed_world(fixture, {
                let scope = scope.clone();
                let session_id = session_id.clone();
                let group_key = group_key.clone();
                let env_store = Arc::clone(&env_store);
                let env_ref = env_ref.clone();
                let observation = Arc::clone(&observation);
                let opener = opener.clone();
                let routing_kind = fixture.deferrable_routing;
                move |world| {
                    Box::pin(async move {
                        let _guard = register_opener(
                            &world.host,
                            &scope,
                            Arc::new(LawLeafProvider {
                                definitions: leaf_definitions(),
                                observation: Arc::clone(&observation),
                                session_id: session_id.clone(),
                                intent_target: crate::ProcessId::from("unused-in-mismatch"),
                                start_metadata: serde_json::Value::Null,
                            }),
                            Arc::new(crate::TestLocalProcessRegistry::default()),
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
                            .open_effect_group(single_leaf_group(
                                &scope,
                                &session_id,
                                &group_key,
                                &env_ref,
                                LEAF_DEFERRED,
                                deferrable_routing(routing_kind, &world.host),
                                recorded_cancellation_authority(
                                    &world.host,
                                    &crate::admit(scope.clone()),
                                )
                                .await,
                            ))
                            .await
                            .expect("the group opens under its live opener");
                        // The durable park: the journaled Pending row plus
                        // the armed resolver key, so the dead worker leaves a
                        // claimed unsettled child the successor can inspect.
                        let key = observation.parked_key(&format!("{group_key}-call-0")).await;
                        await_key_registered(&world.host, &session_id, &key).await;
                        scoped
                            .controller()
                            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                            .await
                            .expect("the caller closes and releases its loser");
                    })
                }
            })
            .await;
        }

        let successor = world;
        until_claims_lapse(&successor, &group_key_a).await;
        until_claims_lapse(&successor, &group_key_b).await;
        let drain = successor
            .drain
            .as_ref()
            .expect("a durable tier hands out a drain");

        // Direction one: only the foreign opener B is live. The A child is
        // reported unrunnable — B's live context is not a substitute for the
        // recorded opener.
        let guard_b = register_opener(
            &successor.host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let report = drain
            .drain_group(&group_key_a, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("the drain pass over the A group runs");
        assert!(
            report.children.iter().all(|child| matches!(
                child.outcome,
                crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
            )),
            "a live foreign opener cannot drive the recorded opener's child: {report:?}"
        );
        assert_eq!(
            observation.executions_of("law_deferred").len(),
            2,
            "the foreign opener's drain ran nothing — only the crashed worlds' \
             admissions have run the leaves"
        );

        // Direction two: B steps down, only the foreign opener A is live, and
        // the B child answers the same way.
        drop(guard_b);
        let guard_a = register_opener(
            &successor.host,
            &scope_a,
            provider(&session_a),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_a.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let report = drain
            .drain_group(&group_key_b, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("the drain pass over the B group runs");
        assert!(
            report.children.iter().all(|child| matches!(
                child.outcome,
                crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
            )),
            "the foreign opener fails the other direction the same way: {report:?}"
        );
        assert_eq!(
            observation.executions_of("law_deferred").len(),
            2,
            "no child ran under a foreign opener in either direction"
        );

        // Both recorded openers live: each reclaiming drain replays the
        // journaled Pending attempt — the body never re-runs — and the
        // out-of-band resolutions settle both children.
        let _guard_b = register_opener(
            &successor.host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        for group_key in [group_key_a.as_str(), group_key_b.as_str()] {
            let drained = crate::task::spawn({
                let drain = Arc::clone(drain);
                let group_key = group_key.to_string();
                async move {
                    drain
                        .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
                        .await
                }
            });
            let key = observation.parked_key(&format!("{group_key}-call-0")).await;
            resolve_when_registered(
                &successor.host,
                key,
                crate::Resolution::Ok(serde_json::json!({ "leaf": "mismatch", "via": "resolver" })),
            )
            .await;
            let report = drained
                .await
                .expect("the reclaiming drain task joins")
                .expect("the reclaiming drain pass runs");
            assert!(
                report.children.iter().all(|child| matches!(
                    child.outcome,
                    crate::testing::conformance_support::ChildDrainOutcome::Settled
                )),
                "the recorded opener's drain settles its own child: {report:?}"
            );
        }
        drop(guard_a);
        let runs = observation.executions_of("law_deferred");
        assert_eq!(
            runs.len(),
            2,
            "each child ran its body exactly once — the journaled Pending replayed, never re-executed"
        );
        assert!(
            runs.iter().any(|run| run.session_id == session_a.as_str()),
            "the A child ran under its recorded session: {runs:?}"
        );
        assert!(
            runs.iter().any(|run| run.session_id == session_b.as_str()),
            "the B child ran under its recorded session: {runs:?}"
        );
        return;
    }

    {
        // The in-memory tier: the gate is the open itself, and a live foreign
        // opener does not satisfy it in either direction.
        let host = world.host;
        let scoped_a = host
            .scoped(crate::admit(scope_a.clone()))
            .expect("the A scope binds");
        let scoped_b = host
            .scoped(crate::admit(scope_b.clone()))
            .expect("the B scope binds");
        let guard_b = register_opener(
            &host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        scoped_a
            .controller()
            .open_effect_group(group(
                &scope_a,
                &session_a,
                &group_key_a,
                recorded_cancellation_authority(&host, &crate::admit(scope_a.clone())).await,
            ))
            .await
            .expect_err("a group whose opener is foreign to the live one refuses to open");
        drop(guard_b);
        let guard_a = register_opener(
            &host,
            &scope_a,
            provider(&session_a),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_a.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        scoped_b
            .controller()
            .open_effect_group(group(
                &scope_b,
                &session_b,
                &group_key_b,
                recorded_cancellation_authority(&host, &crate::admit(scope_b.clone())).await,
            ))
            .await
            .expect_err("the foreign direction refuses the same way");

        let _guard_b = register_opener(
            &host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let mut handle_a = scoped_a
            .controller()
            .open_effect_group(group(
                &scope_a,
                &session_a,
                &group_key_a,
                recorded_cancellation_authority(&host, &crate::admit(scope_a.clone())).await,
            ))
            .await
            .expect("the A group opens once its opener is live");
        let mut handle_b = scoped_b
            .controller()
            .open_effect_group(group(
                &scope_b,
                &session_b,
                &group_key_b,
                recorded_cancellation_authority(&host, &crate::admit(scope_b.clone())).await,
            ))
            .await
            .expect("the B group opens once its opener is live");
        next_settlement(&scoped_a, &mut handle_a, 0).await;
        next_settlement(&scoped_b, &mut handle_b, 0).await;
        scoped_a
            .controller()
            .close_effect_group(handle_a, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the A group closes");
        scoped_b
            .controller()
            .close_effect_group(handle_b, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the B group closes");
        drop(guard_a);
    }

    // Each child ran exactly once, under the session its own request
    // recorded — never under the foreign opener's.
    let runs = observation.executions_of("law_plain");
    assert_eq!(runs.len(), 2, "each child ran exactly once");
    assert!(
        runs.iter().any(|run| run.session_id == session_a.as_str()),
        "the A child ran under its recorded session: {runs:?}"
    );
    assert!(
        runs.iter().any(|run| run.session_id == session_b.as_str()),
        "the B child ran under its recorded session: {runs:?}"
    );
}
