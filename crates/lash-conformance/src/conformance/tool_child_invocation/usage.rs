use pretty_assertions::assert_eq;

use super::*;

// =============================================================================
// Usage conservation: one fact per billed provider attempt, on one opener
// =============================================================================

/// The conservation law's group: a billed-retry leaf and a spend-then-cancel
/// leaf that settle before the crash, beside a spend-deferred leaf whose
/// committed `Pending` attempt is what the crash leaves the journal to hold.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn usage_group(
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
            leaf(0, LEAF_BILLED, ToolChildCompletionRouting::Inline),
            leaf(1, LEAF_SPEND_CANCEL, ToolChildCompletionRouting::Inline),
            leaf(2, LEAF_SPEND_DEFERRED, routing),
        ],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the conservation group assembles")
}

/// The conservation law: every provider attempt that reported usage lands
/// exactly once, on the settlement of the child that spent it under its
/// recorded opener — across crash, replay, retry and cancel (ADR 0099 §13,
/// ADR 0032).
///
/// Three children each produce a different conservation edge:
///
/// * the **billed** leaf's fake provider seals one call carrying a *failed*
///   attempt's spend beside the retry's — two facts, not one — and the tool
///   attempt itself retries, so the same call id spends under two attempt
///   ordinals;
/// * the **cancel** leaf spends inside an attempt that then cancels —
///   §13 keeps known usage on the settlement a cancelled outcome still
///   mints;
/// * the **deferred** leaf spends, parks, and loses its worker — the
///   committed `Pending` row's capture is restored exactly once by the
///   reclaiming drain.
///
/// Before the recorded opener returns, a live *foreign* opener's drain pass
/// must find the parked child unrunnable: nothing bills under an opener the
/// request never recorded.
///
/// Durable tiers only: the in-memory host journals nothing past the crash.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_billed_provider_attempt_is_conserved_once_on_its_opener(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_a = crate::SessionId::from(format!("{prefix}-usage-session"));
    let session_b = crate::SessionId::from(format!("{prefix}-usage-foreign"));
    let scope_a = crate::ExecutionScope::turn(
        session_a.clone(),
        crate::TurnId::from(format!("{prefix}-usage-turn")),
    );
    let scope_b = crate::ExecutionScope::turn(
        session_b.clone(),
        crate::TurnId::from(format!("{prefix}-usage-foreign-turn")),
    );
    let opener_a = crate::EffectOpener::for_scope(&crate::admit(scope_a.clone()))
        .expect("a turn scope derives an opener");
    let opener_b = crate::EffectOpener::for_scope(&crate::admit(scope_b.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-usage-group");
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();

    let probe_world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    if probe_world.drain.is_none() {
        // Nothing outlives the process on this tier — the crash edge the
        // conservation is exercised across does not exist here.
        return;
    }

    let observation = Arc::new(LawObservation::default());
    let parked_call = format!("{group_key}-call-2");
    let provider = |session_id: &crate::SessionId| -> Arc<dyn crate::ToolProvider> {
        Arc::new(LawLeafProvider {
            definitions: leaf_definitions(),
            observation: Arc::clone(&observation),
            session_id: session_id.clone(),
            intent_target: crate::ProcessId::from("unused-in-usage"),
            start_metadata: serde_json::Value::Null,
        })
    };

    crashed_world(fixture, {
        let scope = scope_a.clone();
        let session_id = session_a.clone();
        let group_key = group_key.clone();
        let env_store = Arc::clone(&process_env_store);
        let env_ref = env_ref.clone();
        let observation = Arc::clone(&observation);
        let parked_call = parked_call.clone();
        let routing_kind = fixture.deferrable_routing;
        let opener = opener_a.clone();
        let provider = provider(&session_a);
        move |world| {
            Box::pin(async move {
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
                    .open_effect_group(usage_group(
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

                // The billed and cancelled children settle before the crash:
                // the journal holds their minted settlements for the
                // successor to serve, usage included.
                for rank in [0, 1] {
                    let settlement = next_settlement(&scoped, &mut handle, rank).await;
                    assert!(
                        settlement.outcome.is_ok(),
                        "rank {rank} settles before the crash: {:?}",
                        settlement.outcome
                    );
                }

                // The deferred child spends, parks, and is left claimed when
                // the worker dies: its committed Pending row's capture is the
                // fact the reclaiming drain must restore exactly once.
                let key = observation.parked_key(&parked_call).await;
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
        observation
            .executions_of("law_billed")
            .iter()
            .map(|run| run.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the billed leaf's body ran once per tool attempt"
    );
    assert_eq!(
        observation.executions_of("law_spend_cancel").len(),
        1,
        "the cancelled attempt ran its body once"
    );
    assert_eq!(
        observation.executions_of("law_spend_deferred").len(),
        1,
        "the crashed worker ran the spend-deferred body once"
    );

    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&successor.host, &process_env_store);
    until_claims_lapse(&successor, &group_key).await;
    let drain = Arc::clone(
        successor
            .drain
            .as_ref()
            .expect("a durable tier hands out a drain"),
    );

    // A live opener that is not the recorded one cannot host the retained
    // child's ledger: the pass finds it unrunnable and nothing bills under B.
    let guard_b = register_opener(
        &successor.host,
        &scope_b,
        provider(&session_b),
        Arc::new(crate::TestLocalProcessRegistry::default()),
        Arc::clone(&process_env_store),
        opener_b,
        tokio_util::sync::CancellationToken::new(),
    );
    let report = drain
        .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
        .await
        .expect("the foreign opener's drain pass runs");
    assert!(
        report.children.iter().all(|child| matches!(
            child.outcome,
            crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
        )),
        "a live foreign opener cannot drive the parked child: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_spend_deferred").len(),
        1,
        "nothing ran — and nothing billed — under the foreign opener"
    );
    drop(guard_b);

    // The recorded opener's drain replays the journaled Pending attempt — the
    // body never re-runs — then the out-of-band resolution settles the child.
    let _guard_a = register_opener(
        &successor.host,
        &scope_a,
        provider(&session_a),
        (fixture.make_registry)().await,
        Arc::clone(&process_env_store),
        opener_a,
        tokio_util::sync::CancellationToken::new(),
    );
    let drained = crate::task::spawn({
        let drain = Arc::clone(&drain);
        let group_key = group_key.clone();
        async move {
            drain
                .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
                .await
        }
    });
    let key = observation.parked_key(&parked_call).await;
    resolve_when_registered(
        &successor.host,
        key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "usage", "via": "resolver" })),
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
        "the recorded opener's drain settles the parked child: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_spend_deferred").len(),
        1,
        "the journaled Pending attempt replayed; the body never ran twice"
    );

    // A reopen serves all three settlements. Every provider attempt that
    // billed appears exactly once, keyed by the attempt it was spent under —
    // on the settlement of the child that spent it, nowhere else.
    async fn settlements(
        scoped: &crate::ScopedEffectController<'_>,
        handle: &mut crate::EffectGroupHandle,
    ) -> Vec<crate::GroupSettlement> {
        let mut served = Vec::new();
        for rank in 0..3 {
            served.push(next_settlement(scoped, handle, rank).await);
        }
        served.sort_by_key(|settlement| settlement.position);
        served
    }
    let scoped = successor
        .host
        .scoped(crate::admit(scope_a.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(usage_group(
            &scope_a,
            &session_a,
            &group_key,
            &env_ref,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
            recorded_cancellation_authority(&successor.host, &crate::admit(scope_a.clone())).await,
        ))
        .await
        .expect("the successor reopens the drained group");
    let served = settlements(&scoped, &mut handle).await;
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor closes");

    let usage_of =
        |settlement: &crate::GroupSettlement| -> Vec<crate::runtime::effect::ToolUsageDelta> {
            match &settlement.outcome {
                Ok(crate::RuntimeEffectOutcome::ToolInvocation { settlement, .. }) => {
                    settlement.validate().expect("the settlement validates");
                    settlement.usage.clone()
                }
                other => panic!(
                    "rank {} settled to something that is not a tool invocation: {other:?}",
                    settlement.position
                ),
            }
        };

    // The billed leaf: two provider attempts billed per call (the failed one
    // and its retry), two tool attempts billed — four deltas, each keyed by
    // its own (attempt, provider attempt) pair. Dropping the failed provider
    // attempt's spend or summing the pair into one fact both fail here.
    let billed = usage_of(&served[0]);
    let billed_keys: Vec<(u32, &str, u32)> = billed
        .iter()
        .map(|delta| {
            (
                delta.attempt,
                delta.llm_call_id.0.as_str(),
                delta.provider_attempt,
            )
        })
        .collect();
    assert_eq!(
        billed_keys,
        vec![
            (1, "law-billed-call", 1),
            (1, "law-billed-call", 2),
            (2, "law-billed-call", 1),
            (2, "law-billed-call", 2),
        ],
        "the billed leaf conserves the failed attempt's spend and each tool \
         attempt's spend as distinct facts"
    );
    assert_eq!(billed[0].usage.input_tokens, 41);
    assert_eq!(billed[1].usage.input_tokens, 11);
    assert_eq!(billed[2].usage.input_tokens, 41);
    assert_eq!(billed[3].usage.input_tokens, 11);

    // The cancelled leaf: §13's rule — cancellation refuses semantic results,
    // it does not discard known usage.
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &served[1].outcome else {
        panic!(
            "the cancelled child settles a tool invocation: {:?}",
            served[1]
        )
    };
    assert!(
        matches!(
            outcome.record.output.outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ),
        "the cancelled leaf's settled record is the cancellation"
    );
    let cancelled = usage_of(&served[1]);
    assert_eq!(
        cancelled
            .iter()
            .map(|delta| (
                delta.attempt,
                delta.llm_call_id.0.as_str(),
                delta.provider_attempt
            ))
            .collect::<Vec<_>>(),
        vec![(1, "law-direct-call", 1)],
        "the cancelled attempt's spend rides its settlement"
    );

    // The replayed leaf: the committed Pending capture restored once — not
    // zero, not twice.
    let replayed = usage_of(&served[2]);
    assert_eq!(
        replayed
            .iter()
            .map(|delta| (
                delta.attempt,
                delta.llm_call_id.0.as_str(),
                delta.provider_attempt
            ))
            .collect::<Vec<_>>(),
        vec![(1, "law-direct-call", 1)],
        "the crashed child's spend is restored exactly once"
    );

    // Across the whole group every (invocation, attempt, call, provider
    // attempt) key is unique: no fact lands on a sibling's settlement, and no
    // fact lands twice.
    let all_keys: Vec<(usize, u32, String, u32)> = served
        .iter()
        .flat_map(|settlement| {
            usage_of(settlement).into_iter().map(move |delta| {
                (
                    settlement.position,
                    delta.attempt,
                    delta.llm_call_id.0.clone(),
                    delta.provider_attempt,
                )
            })
        })
        .collect();
    let unique: std::collections::BTreeSet<_> = all_keys.iter().collect();
    assert_eq!(
        unique.len(),
        all_keys.len(),
        "every billed provider attempt lands under exactly one key on exactly \
         one child's settlement: {all_keys:?}"
    );

    // A second serve is the recorded facts re-served, not a second billing:
    // "retained facts are incorporated idempotently" (§13).
    let mut replay_handle = scoped
        .controller()
        .open_effect_group(usage_group(
            &scope_a,
            &session_a,
            &group_key,
            &env_ref,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
            recorded_cancellation_authority(&successor.host, &crate::admit(scope_a.clone())).await,
        ))
        .await
        .expect("a recorded group reopens to serve its journaled settlements");
    let re_served = settlements(&scoped, &mut replay_handle).await;
    scoped
        .controller()
        .close_effect_group(replay_handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the replayed group closes");
    for (first, second) in served.iter().zip(re_served.iter()) {
        assert_eq!(
            usage_of(first),
            usage_of(second),
            "rank {} re-serves the same usage facts rather than re-billing",
            first.position
        );
    }
    assert_eq!(
        observation.executions_of("law_billed").len(),
        2,
        "replays re-served the recorded attempts; no body re-ran"
    );
}
