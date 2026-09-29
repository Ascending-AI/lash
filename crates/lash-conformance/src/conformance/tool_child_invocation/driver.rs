use pretty_assertions::assert_eq;

use super::*;
use crate::ProcessEventLogTestSupport as _;

/// The driver lanes, including every pending resolver declaration.
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
    cancellation: crate::TurnControlBindingId,
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
    let children = vec![
        leaf(0, LEAF_PLAIN, ToolChildCompletionRouting::Inline),
        leaf(1, LEAF_RETRY, ToolChildCompletionRouting::Inline),
        leaf(2, LEAF_DEFERRED, routing),
        granted,
        leaf(4, LEAF_INTENTS, ToolChildCompletionRouting::Inline),
        leaf(5, LEAF_USAGE, ToolChildCompletionRouting::Inline),
        leaf(6, LEAF_PROCESS_PENDING, ToolChildCompletionRouting::Durable),
        leaf(
            7,
            LEAF_DECLARED_PENDING,
            ToolChildCompletionRouting::Durable,
        ),
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
pub async fn declared_intent_replay_preserves_manifest_order_and_capabilities(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-lane"));
    let turn_id = crate::TurnId::from(format!("{prefix}-lane-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-lane-group");
    let mut scenario = scenario(fixture, &session_id, serde_json::json!({"lane": "intents"})).await;
    // The fixture deliberately lends no definition or trigger registry. Those
    // declarations must retain typed refusals beside the executable vocabulary.
    // Trigger registration also offers an actor without the recorded child frame.
    Arc::get_mut(&mut scenario.provider)
        .expect("the provider has not been lent yet")
        .additional_intents = vec![
        crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
            session_id: session_id.clone(),
            process_id: scenario.intent_target.clone(),
            signal_name: "law_intent_signal".to_string(),
            payload: serde_json::json!({"signal": true}),
        }),
        crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
            session_id: session_id.clone(),
            process_id: scenario.intent_target.clone(),
        }),
        crate::ToolIntent::EmitTrigger(crate::EmitTriggerIntent {
            session_id: session_id.clone(),
            request: crate::TriggerOccurrenceRequest::new(
                "law",
                "key",
                serde_json::Value::Null,
                "caller-key",
            ),
        }),
        crate::ToolIntent::RegisterProcessDefinition(Box::new(
            crate::RegisterProcessDefinitionIntent {
                session_id: session_id.clone(),
                engine_kind: "law".to_string(),
                definition: serde_json::Value::Null,
                env_spec: None,
                label: None,
                name: Some("law".to_string()),
                expected_revision: None,
                module: None,
            },
        )),
        crate::ToolIntent::RegisterTrigger(Box::new(crate::RegisterTriggerIntent {
            session_id: session_id.clone(),
            owner_scope: crate::TriggerOwnerScope::session(session_id.clone()),
            actor: crate::ProcessOriginator::session(crate::SessionScope::new(session_id.clone())),
            env_spec: None,
            draft: crate::TriggerSubscriptionDraft::for_process(
                "law",
                scenario.env_ref.clone(),
                "law",
                "key",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::ProcessIdentity::new("law"),
            ),
        })),
    ];
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
        ToolChildCompletionRouting::Durable,
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
    let plain_runs = scenario.observation.executions_of("law_plain");
    assert_eq!(
        plain_runs.iter().map(|run| run.attempt).collect::<Vec<_>>(),
        vec![1],
        "the plain leaf ran its body exactly once"
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
            crate::ToolIntentKind::EmitProcessEvent,
            crate::ToolIntentKind::SignalProcess,
            crate::ToolIntentKind::CancelProcess,
            crate::ToolIntentKind::EmitTrigger,
            crate::ToolIntentKind::RegisterProcessDefinition,
            crate::ToolIntentKind::RegisterTrigger,
        ],
        "the child records every vocabulary result in declaration order after commit"
    );
    assert_eq!(kinds.len(), crate::ToolIntentKind::ALL.len());
    for (index, receipt) in intents.1.intent_outcomes.iter().enumerate() {
        let identity = match receipt {
            crate::ToolIntentExecutionOutcome::Executed { identity, .. } if index < 4 => identity,
            crate::ToolIntentExecutionOutcome::Refused {
                identity: Some(identity),
                refusal: crate::ToolIntentRefusalReason::CommandFailed { .. },
                ..
            } if (4..6).contains(&index) => identity,
            crate::ToolIntentExecutionOutcome::Refused {
                identity: Some(identity),
                refusal: crate::ToolIntentRefusalReason::ForeignTriggerActor { .. },
                ..
            } if index == 6 => identity,
            other => panic!("unexpected vocabulary result at index {index}: {other:?}"),
        };
        assert_eq!(identity.intent_index as usize, index);
        assert_eq!(identity.session_id, session_id);
        assert_eq!(
            identity.tool_call_id,
            leaf_call_id(&format!("{group_key}-call-4"))
        );
    }
    for (name, expected_id) in [
        ("law_plain", LEAF_PLAIN),
        ("law_retry", LEAF_RETRY),
        ("law_deferred", LEAF_DEFERRED),
        ("law_granted", LEAF_GRANTED),
        ("law_intents", LEAF_INTENTS),
        ("law_usage", LEAF_USAGE),
        ("law_process_pending", LEAF_PROCESS_PENDING),
        ("law_declared_pending", LEAF_DECLARED_PENDING),
    ] {
        for execution in scenario.observation.executions_of(name) {
            assert_eq!(
                execution.tool_id,
                crate::ToolId::from(expected_id),
                "one manifest couples the recorded id and provider name"
            );
        }
    }
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

    // Neither runtime-owned resolver can park on a service that cannot attach
    // a terminal. The declared start still retains the admitted launch receipt.
    for position in [6, 7] {
        let crate::ToolCallOutcome::Failure(failure) = &outcomes[position].0.record.output.outcome
        else {
            panic!(
                "a resolver without terminal attachment must settle a refusal: {:?}",
                outcomes[position].0.record.output
            );
        };
        assert_eq!(failure.code, "pending_tool_resolver_unarmed");
    }
    assert!(outcomes[6].1.intent_outcomes.is_empty());
    let [crate::ToolIntentExecutionOutcome::Executed { identity, kind, .. }] =
        outcomes[7].1.intent_outcomes.as_slice()
    else {
        panic!(
            "the declared pending call retains one launch receipt: {:?}",
            outcomes[7].1.intent_outcomes
        );
    };
    assert_eq!(*kind, crate::ToolIntentKind::StartProcess);
    assert_eq!(identity.session_id, session_id);
    assert_eq!(
        identity.tool_call_id,
        leaf_call_id(&format!("{group_key}-call-7"))
    );
    assert_eq!(identity.intent_index, 0);

    // Replay: a second open of the same group serves the journaled
    // settlements — the receipts and the possession are the recorded ones,
    // not re-executions.
    let mut replay_handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("a recorded group reopens to serve its journaled settlements");
    let mut replayed_intents = None;
    for rank in 0..8 {
        let settlement = next_settlement(&scoped, &mut replay_handle, rank).await;
        let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
            outcome,
            settlement: child,
        }) = &settlement.outcome
        else {
            panic!("the retained rank carries a tool settlement");
        };
        assert_eq!(
            serde_json::to_value(outcome).expect("encode replay outcome"),
            serde_json::to_value(&outcomes[settlement.position].0)
                .expect("encode recorded outcome")
        );
        assert_eq!(
            serde_json::to_value(child).expect("encode replay settlement"),
            serde_json::to_value(&outcomes[settlement.position].1)
                .expect("encode recorded settlement")
        );
        if settlement.position == 4 {
            replayed_intents = Some(settlement.outcome);
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
    }) = replayed_intents.expect("rank 4 re-serves on replay")
    else {
        panic!("rank 4 replayed to something that is not a tool invocation")
    };
    assert_eq!(
        replayed.intent_outcomes, outcomes[4].1.intent_outcomes,
        "replay preserves the complete ordered intent receipts and identities"
    );
    assert_eq!(scenario.observation.executions_of("law_intents").len(), 1);
    assert_eq!(scenario.observation.executions_of("law_granted").len(), 1);
    assert_eq!(
        scenario
            .observation
            .executions_of("law_process_pending")
            .len(),
        1
    );
    assert_eq!(
        scenario
            .observation
            .executions_of("law_declared_pending")
            .len(),
        1
    );
    let replay_events = scenario
        .registry
        .full_event_window(&scenario.intent_target, 0)
        .await
        .expect("the replayed event log reads");
    assert_eq!(
        replay_events
            .iter()
            .filter(|event| event.event_type == "law.intent-event")
            .count(),
        1
    );
    assert_eq!(
        replayed.possession, outcomes[4].1.possession,
        "the settlement's possession is the recorded one after replay"
    );
    assert_eq!(
        replayed.model_return, outcomes[4].1.model_return,
        "the settlement's recorded return is unchanged on replay"
    );
}
