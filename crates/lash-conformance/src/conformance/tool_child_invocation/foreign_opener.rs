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
/// the recorded opener is elsewhere; on a drain-less tier the same gate is
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
    let env_store = (fixture.make_processes)().await.process_env_store();
    let env_ref = crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
    let observation = Arc::new(LawObservation::default());
    let registry = (fixture.make_processes)().await.process_registry();

    let provider = |session_id: &crate::SessionId| -> Arc<dyn crate::ToolProvider> {
        Arc::new(LawLeafProvider {
            definitions: leaf_definitions(),
            observation: Arc::clone(&observation),
            session_id: session_id.clone(),
            intent_target: crate::ProcessId::fixture("unused-in-mismatch"),
            start_metadata: serde_json::Value::Null,
            additional_intents: Vec::new(),
        })
    };
    let group = |scope: &crate::ExecutionScope,
                 session_id: &crate::SessionId,
                 group_key: &str,
                 cancellation: crate::TurnControlBindingId| {
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

    {
        // A drain-less tier: the gate is the open itself, and a live foreign
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
