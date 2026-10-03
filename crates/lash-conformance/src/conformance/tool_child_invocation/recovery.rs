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
    cancellation: crate::TurnControlBindingId,
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

/// An absent opener refuses the first open before executing the child. The
/// identical group dispatches once that opener registers (ADR 0099 §1, W1).
/// Deferred dispatch returns its original completion key without awaiting the
/// result. Resolution belongs to the Run and leaves the recorded dispatch
/// unchanged on replay (FIG-4740).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unregistered_opener_leaves_the_child_accepted(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::fixture(format!("{prefix}-recovery"));
    let turn_id = crate::TurnId::fixture(format!("{prefix}-recovery-turn"));
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

    {
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
        let group = recovery_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        );
        let mut handle = scoped
            .controller()
            .open_effect_group(group.clone())
            .await
            .expect("the identical group opens once the opener is live");
        let key = scenario
            .observation
            .parked_key(&format!("{group_key}-call-0"))
            .await;
        await_key_registered(&host, &session_id, &key).await;
        let settlement = next_settlement(&scoped, &mut handle, 0).await;
        let Ok(crate::RuntimeEffectOutcome::ToolInvocationDeferred { completion }) =
            &settlement.outcome
        else {
            panic!("the recovered child settles its deferred dispatch: {settlement:?}")
        };
        assert_eq!(
            completion.request.call.call_id,
            leaf_call_id(&format!("{group_key}-call-0"))
        );
        assert_eq!(completion.pending.call_id, completion.request.call.call_id);
        assert_eq!(completion.pending.key, key);
        assert_eq!(completion.request.execution_env, env_ref);
        let resolved = host
            .resolve_await_event(
                &key,
                crate::Resolution::Ok(serde_json::json!({ "leaf": "recovery", "via": "resolver" })),
            )
            .await
            .expect("the deferred dispatch's key resolves");
        assert_eq!(resolved, crate::ResolveOutcome::Accepted);
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the dispatched group closes");
        let mut replay = scoped
            .controller()
            .open_effect_group(group)
            .await
            .expect("the recovered dispatch reopens");
        let replayed = next_settlement(&scoped, &mut replay, 0).await;
        assert_eq!(
            serde_json::to_value(&replayed.outcome).expect("encode replayed dispatch"),
            serde_json::to_value(&settlement.outcome).expect("encode recorded dispatch"),
            "resolution cannot replace the recorded deferred dispatch"
        );
        scoped
            .controller()
            .close_effect_group(replay, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the replayed group closes");
        assert_eq!(
            scenario.observation.executions_of("law_recovery").len(),
            1,
            "the leaf body ran exactly once"
        );
    }
}
