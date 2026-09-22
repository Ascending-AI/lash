use pretty_assertions::assert_eq;

use super::*;
use crate::ProcessEventLogTestSupport as _;

/// The full-lane group: seven children, one per driver lane.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn lane_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    parent: &crate::RuntimeInvocation,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::RuntimeEffectGroup {
    let leaf = |position: usize, tool_id: &str, routing| {
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
                parent,
                cancellation.clone(),
            ),
        )
    };
    let granted = child_envelope(
        scope,
        group_key,
        3,
        leaf_request(
            scope,
            session_id,
            &format!("{group_key}-call-3"),
            LEAF_GRANTED,
            LEAF_GRANTED.trim_start_matches("tool:"),
            crate::runtime::effect::ToolChildAdmission::Granted {
                grant: Box::new(leaf_grant()),
            },
            ToolChildCompletionRouting::Inline,
            env_ref,
            parent,
            cancellation.clone(),
        ),
    );
    // Rank 7 is the same-ID-orchestrator probe: a Granted admission on the
    // id the tool registry holds as orchestrating. The lane gate must see
    // the admission arm, not the registration — the call runs as a leaf
    // under its grant or it runs an orchestrating body the grant never
    // described, and the assertions downstream name which happened.
    let granted_over_orchestrator = child_envelope(
        scope,
        group_key,
        7,
        leaf_request(
            scope,
            session_id,
            &format!("{group_key}-call-7"),
            LEAF_ORCHESTRATING,
            LEAF_ORCHESTRATING.trim_start_matches("tool:"),
            crate::runtime::effect::ToolChildAdmission::Granted {
                grant: Box::new(orchestrator_id_grant()),
            },
            ToolChildCompletionRouting::Inline,
            env_ref,
            parent,
            cancellation.clone(),
        ),
    );
    let children = vec![
        leaf(0, LEAF_PLAIN, ToolChildCompletionRouting::Inline),
        leaf(1, LEAF_RETRY, ToolChildCompletionRouting::Inline),
        leaf(2, LEAF_DEFERRED, routing),
        granted,
        leaf(4, LEAF_INTENTS, ToolChildCompletionRouting::Inline),
        leaf(5, LEAF_USAGE, ToolChildCompletionRouting::Inline),
        leaf(6, LEAF_ORCHESTRATING, ToolChildCompletionRouting::Inline),
        granted_over_orchestrator,
    ];
    crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key,
        children,
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the law's group assembles")
}

/// The lane law: every child runs through the handler-level driver and its
/// settlement carries the semantic record ADR 0099 §6 specifies.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn tool_children_run_through_the_invocation_driver(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-lane"));
    let turn_id = crate::TurnId::from(format!("{prefix}-lane-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-lane-group");
    let scenario = scenario(fixture, &session_id, serde_json::json!({"lane": "intents"})).await;
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

    let parent = parent_invocation(&scope);
    let group = lane_group(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        &parent,
        deferrable_routing(fixture.deferrable_routing, &host),
        recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(group.clone())
        .await
        .expect("a group of tool children opens when their opener is live");

    // The deferred leaf parks on its key; the law resolves it out of band,
    // through the same host surface an external resolver would use.
    let observation = Arc::clone(&scenario.observation);
    let resolver = Arc::clone(&host);
    let deferred_call = format!("{group_key}-call-2");
    let resolve = crate::task::spawn(async move {
        let key = observation.parked_key(&deferred_call).await;
        resolve_when_registered(
            &resolver,
            key,
            crate::Resolution::Ok(serde_json::json!({ "leaf": "deferred", "via": "resolver" })),
        )
        .await;
    });

    let mut settlements: Vec<crate::GroupSettlement> = Vec::new();
    for rank in 0..8 {
        settlements.push(next_settlement(&scoped, &mut handle, rank).await);
    }
    resolve.await.expect("the resolver task joins");
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
        (0..8).collect::<Vec<_>>(),
        "every child settles exactly once, at every rank"
    );

    // Every settlement is a ToolInvocation carrying a valid settlement, and the
    // recorded return is the presentation the opener incorporates verbatim.
    let outcomes: Vec<(
        crate::tool_dispatch::ToolDispatchOutcome,
        crate::runtime::effect::ToolSettlement,
    )> = settlements
        .iter()
        .map(|group_settlement| match &group_settlement.outcome {
            Ok(crate::RuntimeEffectOutcome::ToolInvocation {
                outcome,
                settlement,
            }) => {
                settlement.validate().expect("the settlement validates");
                // The resolved return was recorded at the child's
                // presentation boundary: its call id is the leaf's own.
                assert_eq!(
                    settlement.model_return.call_id,
                    format!("{group_key}-call-{}", group_settlement.position),
                    "the recorded return answers the child's own call id"
                );
                ((**outcome).clone(), (**settlement).clone())
            }
            other => panic!(
                "rank {} settled to something that is not a tool invocation: {other:?}",
                group_settlement.position
            ),
        })
        .collect();

    // The plain leaf: a first execution settles with its resolved return.
    let plain = &outcomes[0];
    assert!(
        matches!(
            plain.0.record.output.outcome,
            crate::ToolCallOutcome::Success(_)
        ),
        "the plain leaf succeeds"
    );
    // The orchestrating child at rank 6 also calls this leaf nested; every
    // invocation ran its body exactly once (each run is attempt 1). The total
    // count is asserted after the orchestrating assertions below.
    assert!(
        scenario
            .observation
            .executions_of("law_plain")
            .iter()
            .all(|run| run.attempt == 1),
        "no invocation of the plain leaf re-executed its body"
    );

    // The retry leaf: the first attempt's journaled failure is visible as a
    // recorded attempt, and the retry settles the child — one driver owns the
    // whole loop.
    let retry = &outcomes[1];
    let retry_runs = scenario.observation.executions_of("law_retry");
    assert_eq!(
        retry_runs.iter().map(|run| run.attempt).collect::<Vec<_>>(),
        vec![1, 2],
        "the retry leaf's body ran once per attempt, and the attempts numbered themselves"
    );
    assert!(
        !retry.0.attempts.is_empty(),
        "the journaled retry attempts ride the outcome"
    );

    // The deferred leaf: parked at handler level, settled by the out-of-band
    // resolution, which becomes the child's output.
    let deferred = &outcomes[2];
    let deferred_text = format!("{:?}", deferred.0.record.output);
    assert!(
        deferred_text.contains("resolver"),
        "the deferred leaf's settled output carries the resolution: {deferred_text}"
    );

    // The granted leaf: the recorded grant's execution binding reached the
    // executing body — authority the live catalog never supplied.
    let granted_runs = scenario.observation.executions_of("law_granted");
    assert_eq!(granted_runs.len(), 1);
    assert_eq!(
        granted_runs[0].execution_binding,
        serde_json::json!({ "route": "granted-by-request" }),
        "the granted leaf executed under its recorded grant"
    );

    // The intents leaf: both declarations were realized by the child after the
    // attempt committed, and the settlement carries the realized outcomes plus
    // the started process's possession.
    let intents = &outcomes[4];
    let kinds: Vec<crate::ToolIntentKind> = intents
        .1
        .intent_outcomes
        .iter()
        .filter_map(|outcome| outcome.kind())
        .collect();
    assert_eq!(
        kinds,
        vec![
            crate::ToolIntentKind::StartProcess,
            crate::ToolIntentKind::EmitProcessEvent
        ],
        "the child realized both declared intents after commit"
    );
    assert_eq!(
        intents.1.possession.len(),
        1,
        "the realized start names the derived process id in the settlement's possession"
    );
    let events = scenario
        .registry
        .full_event_window(&scenario.intent_target, 0)
        .await
        .expect("the intent target's event log reads");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "law.intent-event")
            .count(),
        1,
        "the child's declared event landed in the process registry exactly once"
    );

    // The usage leaf: the managed-LLM spend inside its attempt was captured
    // into the journaled attempt outcome and aggregated onto the settlement.
    let usage = &outcomes[5];
    assert_eq!(
        usage.1.usage.len(),
        1,
        "the child's direct-completion spend rides its settlement"
    );
    assert_eq!(
        usage.1.usage[0].usage.input_tokens, 41,
        "the captured delta is the attempt's own spend"
    );

    // The orchestrating leaf: the orchestration lane ran the body directly —
    // the leaf provider never saw *it* — while the body's nested call ran as
    // a journaled attempt under the child's own rebound dispatch, and the
    // durable start it realized is the settlement's possession.
    //
    // The one provider execution under this name is the rank-7 child's: a
    // Granted admission, so the lane gate left it to the leaf provider, which
    // recorded the grant's execution binding. Had the gate consulted the
    // registration instead of the admission arm, that call would have run an
    // orchestrating body and this execution would not exist.
    let orchestrating_runs = scenario.observation.executions_of("law_orchestrating");
    assert_eq!(
        orchestrating_runs.len(),
        1,
        "only the same-ID grant reached the leaf provider: {orchestrating_runs:?}"
    );
    assert_eq!(
        orchestrating_runs[0].execution_binding,
        serde_json::json!({ "route": "granted-over-orchestrator" }),
        "the same-ID call executed under its recorded grant, not the orchestrating registration"
    );
    let orchestrating = &outcomes[6];
    if !orchestrating.0.record.output.is_success() {
        panic!("the orchestrating leaf settles: {:?}", orchestrating.0)
    }
    let value = orchestrating.0.record.output.value_for_projection();
    assert_eq!(
        value["nested_ok"],
        serde_json::json!(true),
        "the body's nested call executed through the child's rebound dispatch"
    );
    let started_id = format!("{group_key}-call-6-started");
    assert_eq!(
        value["started"],
        serde_json::json!(started_id.clone()),
        "the body's durable start is part of its settled output"
    );
    assert_eq!(
        orchestrating.1.possession,
        vec![crate::ProcessId::from(started_id)],
        "an orchestrating body's realized start rides the settlement's possession"
    );
    assert_eq!(
        scenario.observation.executions_of("law_plain").len(),
        2,
        "the plain leaf ran once as its own child and once nested under the \
         orchestrating body"
    );

    // The same-ID-orchestrator child: the recorded grant decided the lane, so
    // the call settled as a leaf — the leaf-shaped output, no started process
    // in its possession, and none of the orchestrating body's side effects.
    let granted_over_orchestrator = &outcomes[7];
    let granted_value = granted_over_orchestrator
        .0
        .record
        .output
        .value_for_projection();
    assert_eq!(
        granted_value["leaf"],
        serde_json::json!("orchestrating-as-leaf"),
        "the granted call ran the leaf body, not the orchestrating one: {granted_value}"
    );
    assert!(
        granted_over_orchestrator.1.possession.is_empty(),
        "a granted leaf starts nothing: possession stays empty where an \
         orchestrating body would have recorded its start"
    );

    // Replay: a second open of the same group serves the journaled
    // settlements — the receipts and the possession are the recorded ones,
    // not re-executions.
    let mut replay_handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("a recorded group reopens to serve its journaled settlements");
    let mut replayed_orchestrating = None;
    for rank in 0..8 {
        let settlement = next_settlement(&scoped, &mut replay_handle, rank).await;
        if settlement.position == 6 {
            replayed_orchestrating = Some(settlement.outcome);
        }
    }
    scoped
        .controller()
        .close_effect_group(replay_handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the replayed group closes");
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
        settlement: replayed,
        ..
    }) = replayed_orchestrating.expect("rank 6 re-serves on replay")
    else {
        panic!("rank 6 replayed to something that is not a tool invocation")
    };
    assert_eq!(
        replayed.possession, outcomes[6].1.possession,
        "the settlement's possession is the recorded one after replay"
    );
    assert_eq!(
        replayed.model_return, outcomes[6].1.model_return,
        "the settlement's recorded return is unchanged on replay"
    );
}
