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
    }
}
