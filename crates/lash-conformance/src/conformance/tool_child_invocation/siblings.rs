//! One cross-tier law: a timer child and a durable-wait child are admitted
//! beside a tool child in one effect group (FIG-3397, ADR 0099 §4).
//!
//! The tool-child host is the group-child resolver, and its `executor_for`
//! must answer a local executor for every child kind a group can carry — a
//! `Sleep` and an `AwaitEvent` among them. Before FIG-3397 armed those slots
//! the open was refused: a group whose child had no executor could not run.
//! The law proves the arms exist by opening a three-child group — one tool
//! leaf, one one-millisecond timer, one durable wait — and observing all
//! three settle, the wait only after an out-of-band resolution lands against
//! the key it named.

use super::*;

/// Admits a `Sleep` and an `AwaitEvent` child beside a `ToolInvocation` and
/// serves all three settlements (ADR 0099 §4).
///
/// The wait child must not settle before its key resolves out of band — the
/// timer and the tool leaf settling first is exactly the settlement-rank
/// behavior §5 defines, and a wait that settled early would be an armed
/// executor that never waited.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn timer_and_durable_wait_children_are_admitted_beside_a_tool_child(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-siblings"));
    let turn_id = crate::TurnId::from(format!("{prefix}-siblings-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-siblings-group");
    let scenario = scenario(
        fixture,
        &session_id,
        serde_json::json!({"lane": "siblings"}),
    )
    .await;
    let host = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await
    .host;
    let _guard = register_opener(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );

    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let wait_key = scoped
        .controller()
        .await_event_key(
            &scope,
            crate::AwaitEventWaitIdentity::tool_completion(format!("{prefix}-wait")),
        )
        .await
        .expect("the wait child's key derives on the group's controller");

    let parent = parent_invocation(&scope);
    let tool_child = child_envelope(
        &scope,
        &group_key,
        0,
        leaf_request(
            &scope,
            &session_id,
            &format!("{group_key}-call-0"),
            LEAF_PLAIN,
            LEAF_PLAIN.trim_start_matches("tool:"),
            catalog_admission(LEAF_PLAIN),
            ToolChildCompletionRouting::Inline,
            &scenario.env_ref,
            &parent,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        ),
    );
    let envelope = |position: usize, command: crate::RuntimeEffectCommand| {
        crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(scope.clone(), format!("{group_key}:child:{position}"))
                    .expect("valid group-child address"),
                crate::RuntimeAttribution::none(),
                "effect",
            ),
            command,
        )
    };
    let group = crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key.clone(),
        vec![
            tool_child,
            envelope(
                1,
                crate::RuntimeEffectCommand::Sleep {
                    spec: crate::SleepSpec::For { duration_ms: 1 },
                },
            ),
            envelope(
                2,
                crate::RuntimeEffectCommand::AwaitEvent {
                    key: wait_key.clone(),
                },
            ),
        ],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the siblings group assembles");

    let mut handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("a group carrying timer and wait children opens when their opener is live");

    // Two settlements arrive promptly: the millisecond timer and the tool
    // leaf. The wait child must not: it settles only after its key resolves.
    let mut settlements = Vec::new();
    for rank in 0..2 {
        settlements.push(next_settlement(&scoped, &mut handle, rank).await);
    }
    assert!(
        tokio::time::timeout(
            ABSENCE_BUDGET,
            scoped
                .controller()
                .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new()),
        )
        .await
        .is_err(),
        "the wait child settled before its key resolved"
    );

    resolve_when_registered(
        &host,
        wait_key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "wait", "via": "resolver" })),
    )
    .await;
    settlements.push(next_settlement(&scoped, &mut handle, 2).await);
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");

    settlements.sort_by_key(|settlement| settlement.position);
    assert_eq!(
        settlements
            .iter()
            .map(|settlement| settlement.position)
            .collect::<Vec<_>>(),
        vec![0, 1, 2],
        "every child settles exactly once"
    );
    match &settlements[0].outcome {
        Ok(crate::RuntimeEffectOutcome::ToolInvocation { settlement, .. }) => {
            settlement.validate().expect("the settlement validates");
        }
        other => panic!("position 0 is the tool leaf, not {other:?}"),
    }
    assert!(
        matches!(
            &settlements[1].outcome,
            Ok(crate::RuntimeEffectOutcome::Sleep)
        ),
        "position 1 is the timer child"
    );
    assert!(
        matches!(
            &settlements[2].outcome,
            Ok(crate::RuntimeEffectOutcome::AwaitEvent { .. })
        ),
        "position 2 is the wait child"
    );
}
