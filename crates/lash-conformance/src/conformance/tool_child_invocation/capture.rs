use pretty_assertions::assert_eq;

use super::*;

// =============================================================================
// The capture boundary: a crash between attempt commit and invocation settle
// =============================================================================

/// The capture law's group: a spend-deferred leaf whose committed `Pending`
/// attempt holds a usage fact, and a plain sibling settled before the crash.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn capture_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::RuntimeEffectGroup {
    let parent = parent_invocation(scope);
    let leaf = |position: usize, tool_id: &str, routing: ToolChildCompletionRouting| {
        child_envelope(
            scope,
            group_key,
            position,
            leaf_request(
                scope,
                session_id,
                &format!("{group_key}-call-{position}"),
                tool_id,
                tool_id.trim_start_matches("tool:"),
                catalog_admission(tool_id),
                routing,
                env_ref,
                &parent,
                cancellation.clone(),
            ),
        )
    };
    crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key.to_string(),
        vec![
            leaf(0, LEAF_SPEND_DEFERRED, routing),
            leaf(1, LEAF_PLAIN, ToolChildCompletionRouting::Inline),
        ],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the capture group assembles")
}

/// The capture-boundary law: a crash after a child's attempt committed but
/// before its invocation settled neither loses nor repeats the attempt's
/// facts (ADR 0099 §13).
///
/// The spend-deferred leaf makes a managed-LLM call inside its attempt, then
/// parks: the committed `Pending` row's capture is where that spend survives,
/// because the invocation owning it never settled — the worker died parked.
/// The reclaiming drain replays the journaled attempt: the body does not
/// re-execute, the captured spend restores into the child's usage ledger
/// through the same arm a live outcome takes, and the settlement the reopened
/// group is served carries it exactly once — not zero, not two. The sibling
/// settled before the crash is served its journaled outcome unchanged: the
/// replayed child touched nothing beside its own row.
///
/// Durable tiers only: on the in-memory host the process is the substrate, so
/// nothing outlives the crash to be drained — the edge does not exist there.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_crashed_child_replays_its_committed_attempts_facts(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-capture"));
    let turn_id = crate::TurnId::from(format!("{prefix}-capture-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-capture-group");
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();

    let probe_world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    if probe_world.drain.is_none() {
        // Nothing outlives the process on this tier, so there is no journal
        // boundary for the capture to survive — the edge does not exist.
        return;
    }

    let observation = Arc::new(LawObservation::default());
    let call_id = format!("{group_key}-call-0");
    crashed_world(fixture, {
        let scope = scope.clone();
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let env_store = Arc::clone(&process_env_store);
        let env_ref = env_ref.clone();
        let observation = Arc::clone(&observation);
        let call_id = call_id.clone();
        let routing_kind = fixture.deferrable_routing;
        let opener = opener.clone();
        move |world| {
            Box::pin(async move {
                let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
                    definitions: leaf_definitions(),
                    observation: Arc::clone(&observation),
                    session_id: session_id.clone(),
                    intent_target: crate::ProcessId::from("unused-in-capture"),
                    start_metadata: serde_json::Value::Null,
                });
                let _guard = register_opener(
                    &world.host,
                    &scope,
                    provider,
                    Arc::new(crate::TestLocalProcessRegistry::default()),
                    env_store,
                    opener,
                    tokio_util::sync::CancellationToken::new(),
                );
                let scoped = world
                    .host
                    .scoped(crate::admit(scope.clone()))
                    .expect("the group scope binds");
                let mut handle = scoped
                    .controller()
                    .open_effect_group(capture_group(
                        &scope,
                        &session_id,
                        &group_key,
                        &env_ref,
                        deferrable_routing(routing_kind, &world.host),
                        recorded_cancellation_authority(&world.host, &crate::admit(scope.clone()))
                            .await,
                    ))
                    .await
                    .expect("the group opens under the live opener");

                // The sibling settles while the parked child still holds its
                // key. Serving the settlement is the journal-visible proof the
                // sibling's outcome is durable before the worker dies.
                let sibling = next_settlement(&scoped, &mut handle, 1).await;
                assert_eq!(
                    sibling.position, 1,
                    "the plain sibling is the child that settles first: {:?}",
                    sibling.outcome
                );
                assert!(
                    sibling.outcome.is_ok(),
                    "the sibling settles before the crash: {:?}",
                    sibling.outcome
                );

                // `park` fires inside the attempt body — one commit before the
                // journaled Pending row lands — so the durable half of the
                // wait is the armed resolver key becoming visible: by then the
                // attempt row, and the capture riding it, are durable.
                let key = observation.parked_key(&call_id).await;
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
    assert_eq!(
        observation.executions_of("law_spend_deferred").len(),
        1,
        "the crashed worker ran the spend-deferred body once"
    );
    assert_eq!(
        observation.executions_of("law_plain").len(),
        1,
        "the crashed worker ran the sibling once"
    );

    // The successor: claims lapsed, the recorded opener registers, and the
    // drain replays the journaled Pending attempt — the body is not
    // re-executed — then the out-of-band resolution settles the invocation.
    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&successor.host, &process_env_store);
    until_claims_lapse(&successor, &group_key).await;

    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
        definitions: leaf_definitions(),
        observation: Arc::clone(&observation),
        session_id: session_id.clone(),
        intent_target: crate::ProcessId::from("unused-in-capture"),
        start_metadata: serde_json::Value::Null,
    });
    let registry = (fixture.make_registry)().await;
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
    // The redrive re-arms the resolver without re-running the body, so the
    // recorded key is already in the observation — `parked_key` returns it.
    let key = observation.parked_key(&call_id).await;
    resolve_when_registered(
        &successor.host,
        key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "capture", "via": "resolver" })),
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
        "the only unsettled child — the replayed one — settles: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_spend_deferred").len(),
        1,
        "the journaled Pending attempt replays; the body never runs twice"
    );

    // Journal-visible: a reopen serves both ranks — the drained one carrying
    // the restored capture, the pre-crash sibling unchanged.
    let scoped = successor
        .host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(capture_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
            recorded_cancellation_authority(&successor.host, &crate::admit(scope.clone())).await,
        ))
        .await
        .expect("the successor reopens the drained group");
    let mut settled = [
        next_settlement(&scoped, &mut handle, 0).await,
        next_settlement(&scoped, &mut handle, 1).await,
    ];
    settled.sort_by_key(|settlement| settlement.position);

    let replayed = &settled[0];
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
        outcome,
        settlement,
    }) = &replayed.outcome
    else {
        panic!("the replayed child settles a tool invocation: {replayed:?}")
    };
    settlement.validate().expect("the settlement validates");
    assert!(
        format!("{:?}", outcome.record.output).contains("resolver"),
        "the settled output carries the out-of-band resolution"
    );
    // §13's accounting: the committed attempt's spend is restored exactly
    // once — dropped would mean zero deltas, re-executed or double-restored
    // would mean two.
    assert_eq!(
        settlement.usage.len(),
        1,
        "the committed attempt's spend is restored exactly once"
    );
    assert_eq!(
        settlement.usage[0].usage.input_tokens, 41,
        "the restored delta is the attempt's own spend"
    );
    assert_eq!(
        settlement.usage[0].attempt, 1,
        "the delta keeps its attempt stamp"
    );
    assert_eq!(
        settlement.model_return.call_id, call_id,
        "the recorded presentation answers the child's own call"
    );

    let sibling = &settled[1];
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
        outcome,
        settlement,
    }) = &sibling.outcome
    else {
        panic!("the sibling settles a tool invocation: {sibling:?}")
    };
    assert!(
        matches!(
            outcome.record.output.outcome,
            crate::ToolCallOutcome::Success(_)
        ),
        "the pre-crash sibling is served its journaled outcome"
    );
    assert!(
        settlement.usage.is_empty(),
        "the replayed child's restored spend stayed in its own settlement"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor closes");
}
