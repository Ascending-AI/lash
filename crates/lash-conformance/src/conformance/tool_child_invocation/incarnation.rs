use pretty_assertions::assert_eq;

use super::*;

/// The process-scope sibling of [`leaf_request`]. `opener_ref` pins the
/// incarnation the recorded opener names — inside `admitted_scope`, the one
/// checked pair — and `enclosing` is the incarnation the request admits the
/// call inside. `None` only for the malformed probe: a process opener with
/// no enclosing incarnation, which `ToolChildRequest::validate` refuses
/// because the opener and its enclosing process are one fact (ADR 0099 §1).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the request's fields are the leaf's parameters; a struct would only rename the list"
)]
fn process_leaf_request(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    call_id: &str,
    tool_id: &str,
    tool_name: &str,
    admission: crate::runtime::effect::ToolChildAdmission,
    routing: ToolChildCompletionRouting,
    env_ref: &crate::ProcessExecutionEnvRef,
    parent: &crate::RuntimeInvocation,
    opener_ref: &crate::ProcessRef,
    enclosing: Option<crate::ProcessRef>,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::runtime::effect::ToolChildRequest {
    let mut request = crate::runtime::effect::ToolChildRequest::new(
        crate::PreparedToolCall::from_parts(
            call_id,
            crate::ToolId::from(tool_id),
            tool_name,
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        ),
        admission,
        crate::tool_dispatch::ToolAttemptEffectIdentity::Scalar {
            parent: Some(parent.clone()),
        },
        crate::runtime::effect::ToolChildScope {
            opener: crate::EffectOpener::for_scope(
                &crate::AdmittedScope::new(scope.clone(), Some(opener_ref.clone()))
                    .expect("the recorded pin names the claim's own process"),
            )
            .expect("a pinned process scope derives an opener"),
            admitted_scope: crate::AdmittedScope::new(scope.clone(), Some(opener_ref.clone()))
                .expect("the recorded pin names the claim's own process"),
            session_id: session_id.clone(),
            agent_frame_id: crate::FrameNodeId::new("law-frame").expect("a valid frame id"),
        },
        env_ref.clone(),
        routing,
    );
    if let Some(process_ref) = enclosing {
        request = request.with_enclosing_process(process_ref);
    }
    if let Some(binding) = cancellation {
        request = request.with_cancellation_authority(binding);
    }
    request
}

/// The incarnation law's group: two deferred leaves — one resolved inside the
/// crashed world so its settlement orders the crash boundary, one left parked
/// as the survivor the foreign incarnation must refuse — an orchestrating
/// child under the recorded pin, and a plain leaf. The malformed probe cannot
/// ride inside the group: a process opener with no enclosing incarnation is
/// refused at envelope construction, which the law asserts directly instead.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn incarnation_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    recorded_ref: &crate::ProcessRef,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::RuntimeEffectGroup {
    let parent = parent_invocation(scope);
    let child = |position: usize,
                 tool_id: &str,
                 routing: ToolChildCompletionRouting,
                 enclosing: Option<crate::ProcessRef>| {
        child_envelope(
            scope,
            group_key,
            position,
            process_leaf_request(
                scope,
                session_id,
                &format!("{group_key}-call-{position}"),
                tool_id,
                tool_id.trim_start_matches("tool:"),
                catalog_admission(tool_id),
                routing,
                env_ref,
                &parent,
                recorded_ref,
                enclosing,
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
            child(
                0,
                LEAF_DEFERRED,
                routing.clone(),
                Some(recorded_ref.clone()),
            ),
            child(
                1,
                LEAF_ORCHESTRATING,
                ToolChildCompletionRouting::Inline,
                Some(recorded_ref.clone()),
            ),
            child(
                2,
                LEAF_PLAIN,
                ToolChildCompletionRouting::Inline,
                Some(recorded_ref.clone()),
            ),
            child(3, LEAF_DEFERRED, routing, Some(recorded_ref.clone())),
        ],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the incarnation group assembles")
}

/// ADR 0099 §1 and the C1 review's pinned-incarnation finding: a process
/// opener is its name bound to **one** store-minted incarnation, so the
/// same-name successor is a foreign opener, not a continuation.
///
/// One group of four process-scoped children records `process(P)#7` as its
/// opener: two deferred leaves (one resolved to order the crash boundary, one
/// the durable survivor), an orchestrating child (whose durable-parent
/// derivation must name the recorded incarnation), and a plain leaf. The
/// malformed request — a process opener that records no enclosing incarnation
/// — is asserted directly: the boundary refuses it at envelope construction,
/// so no journal can hold it, because the opener and its enclosing process
/// are one fact.
///
/// On the durable tiers the group journals under `process(P)#7`, the worker
/// dies, and a live `process(P)#9` proves it cannot drain the survivor. On
/// every tier the orchestrating settlement names `P#7` and the malformed
/// request is refused with `ToolChildRequestOpener`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_same_name_process_incarnation_is_not_the_recorded_opener(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-incarnation-session"));
    let process_id = crate::ProcessId::from(format!("{prefix}-incarnation-process"));
    let scope_p = crate::ExecutionScope::process(process_id.clone());
    let recorded_ref = crate::ProcessRef::new(
        process_id.clone(),
        crate::ProcessIncarnation::from_registration_sequence(7),
    );
    let successor_ref = crate::ProcessRef::new(
        process_id.clone(),
        crate::ProcessIncarnation::from_registration_sequence(9),
    );
    let opener_7 =
        crate::EffectOpener::for_scope(&crate::AdmittedScope::process(recorded_ref.clone()))
            .expect("a pinned process scope derives an opener");
    let opener_9 =
        crate::EffectOpener::for_scope(&crate::AdmittedScope::process(successor_ref.clone()))
            .expect("a pinned process scope derives an opener");
    let group_key = format!("{prefix}-incarnation-group");
    let env_store = (fixture.make_processes)().await.process_env_store;
    let env_ref = crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
    let observation = Arc::new(LawObservation::default());
    let registry = (fixture.make_processes)().await.registry;
    let provider = || -> Arc<dyn crate::ToolProvider> {
        Arc::new(LawLeafProvider {
            definitions: leaf_definitions(),
            observation: Arc::clone(&observation),
            session_id: session_id.clone(),
            intent_target: crate::ProcessId::from("unused-in-incarnation"),
            start_metadata: serde_json::Value::Null,
        })
    };
    // Derived through the same projection the store writes, never a
    // hand-formatted rendering: `storage_id` is the canonical
    // `identity_encoding` of the recorded `ProcessRef`, so the law still
    // proves the body's parent is the recorded incarnation and not whatever
    // string a retired delimiter codec would have produced.
    let expected_parent = crate::ParentScope::process(recorded_ref.clone())
        .storage_id()
        .expect("an owned process parent projects a storage id");

    // The malformed probe: a process opener that records no enclosing
    // incarnation. `ToolChildRequest::validate` makes the opener and its
    // enclosing process one fact, so the envelope constructor refuses the
    // pair — the malformed request cannot be journaled at all, which is a
    // stronger boundary than the settle-time refusal a group child could
    // have shown.
    let malformed = process_leaf_request(
        &scope_p,
        &session_id,
        &format!("{group_key}-call-malformed"),
        LEAF_PLAIN,
        LEAF_PLAIN.trim_start_matches("tool:"),
        catalog_admission(LEAF_PLAIN),
        ToolChildCompletionRouting::Inline,
        &env_ref,
        &parent_invocation(&scope_p),
        &recorded_ref,
        None,
        None,
    );
    let error = malformed
        .validate()
        .expect_err("a process opener without its enclosing incarnation is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
        "the opener and its enclosing process are one fact: {error}"
    );
    let error = crate::RuntimeEffectEnvelope::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope_p.clone(), format!("{group_key}:malformed"))
                .expect("valid group-child address"),
            crate::RuntimeAttribution::none(),
            "effect",
        ),
        crate::RuntimeEffectCommand::ToolInvocation {
            request: Box::new(malformed),
        },
    )
    .expect_err("the boundary refuses the malformed pair at construction");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
        "no journal can hold a request whose opener and enclosing process disagree: {error}"
    );

    /// Asserts the orchestrating settlement's recorded incarnation — identical
    /// on every tier.
    fn assert_process_settlements(
        settlements: &mut [crate::GroupSettlement],
        expected_parent: &str,
    ) {
        settlements.sort_by_key(|settlement| settlement.position);
        let resolved_leaf = settlements
            .iter()
            .find(|settlement| settlement.position == 0)
            .expect("the resolved deferred leaf settled");
        assert!(
            resolved_leaf.outcome.is_ok(),
            "the deferred leaf the law resolved settles: {:?}",
            resolved_leaf.outcome
        );
        let orchestrating = settlements
            .iter()
            .find(|settlement| settlement.position == 1)
            .expect("the orchestrating child settled");
        let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) =
            &orchestrating.outcome
        else {
            panic!("the orchestrating child is a tool invocation: {orchestrating:?}");
        };
        let output = outcome.record.output.value_for_projection();
        assert_eq!(
            output["parent"],
            serde_json::json!(expected_parent),
            "the body's durable parent names the recorded incarnation, never \
             the name's current owner: {output}"
        );
        assert_eq!(
            output["nested_ok"],
            serde_json::json!(true),
            "the orchestrating body ran its nested call: {output}"
        );
        let plain = settlements
            .iter()
            .find(|settlement| settlement.position == 2)
            .expect("the plain leaf settled");
        assert!(
            plain.outcome.is_ok(),
            "the plain leaf settles under the recorded incarnation: {:?}",
            plain.outcome
        );
    }

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&world.host, &env_store);

    if world.drain.is_some() {
        // The durable tiers: journal the group under `process(P)#7` while its
        // opener is live, then kill the worker with the deferred leaf parked.
        crashed_world(fixture, {
            let scope_p = scope_p.clone();
            let session_id = session_id.clone();
            let group_key = group_key.clone();
            let recorded_ref = recorded_ref.clone();
            let env_store = Arc::clone(&env_store);
            let env_ref = env_ref.clone();
            let observation = Arc::clone(&observation);
            let opener_7 = opener_7.clone();
            let routing_kind = fixture.deferrable_routing;
            let expected_parent = expected_parent.clone();
            move |world| {
                Box::pin(async move {
                    let _guard = register_opener(
                        &world.host,
                        &scope_p,
                        Arc::new(LawLeafProvider {
                            definitions: leaf_definitions(),
                            observation: Arc::clone(&observation),
                            session_id: session_id.clone(),
                            intent_target: crate::ProcessId::from("unused-in-incarnation"),
                            start_metadata: serde_json::Value::Null,
                        }),
                        Arc::new(crate::TestLocalProcessRegistry::default()),
                        env_store,
                        opener_7,
                        tokio_util::sync::CancellationToken::new(),
                    );
                    let scoped = world
                        .host
                        .scoped(crate::AdmittedScope::process(recorded_ref.clone()))
                        .expect("the process scope binds");
                    let mut handle = scoped
                        .controller()
                        .open_effect_group(incarnation_group(
                            &scope_p,
                            &session_id,
                            &group_key,
                            &env_ref,
                            &recorded_ref,
                            deferrable_routing(routing_kind, &world.host),
                            recorded_cancellation_authority(
                                &world.host,
                                &crate::AdmittedScope::process(recorded_ref.clone()),
                            )
                            .await,
                        ))
                        .await
                        .expect("the group opens under the recorded incarnation's opener");
                    // Both deferred leaves park. The survivor's await lands
                    // under the process scope's journal — no session listing
                    // can see it — so the crash boundary is ordered instead
                    // through call-0: resolving its key and consuming its
                    // settlement rank is a durable commit strictly after its
                    // own attempt row, and by the time all three ranks are
                    // consumed the survivor's identical commits have landed.
                    let key0 = observation.parked_key(&format!("{group_key}-call-0")).await;
                    let _key3 = observation.parked_key(&format!("{group_key}-call-3")).await;
                    resolve_when_registered(
                        &world.host,
                        key0,
                        crate::Resolution::Ok(
                            serde_json::json!({ "leaf": "incarnation", "via": "resolver" }),
                        ),
                    )
                    .await;
                    let mut settled = vec![
                        next_settlement(&scoped, &mut handle, 0).await,
                        next_settlement(&scoped, &mut handle, 1).await,
                        next_settlement(&scoped, &mut handle, 2).await,
                    ];
                    assert_process_settlements(&mut settled, &expected_parent);
                    scoped
                        .controller()
                        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                        .await
                        .expect("the caller closes and releases its loser");
                })
            }
        })
        .await;

        let successor = world;
        until_claims_lapse(&successor, &group_key).await;
        let drain = successor
            .drain
            .as_ref()
            .expect("a durable tier hands out a drain");

        // The same-name successor is live. It is a foreign opener: the drain
        // reports the surviving child unrunnable and runs nothing.
        let guard_9 = register_opener(
            &successor.host,
            &scope_p,
            provider(),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_9,
            tokio_util::sync::CancellationToken::new(),
        );
        let report = drain
            .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("the drain pass runs under the successor incarnation");
        assert!(
            report.children.iter().all(|child| matches!(
                child.outcome,
                crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
            )),
            "incarnation 9 of the same name cannot drive the child incarnation 7 opened: {report:?}"
        );
        assert_eq!(
            observation.executions_of("law_deferred").len(),
            2,
            "the successor's drain ran nothing — only the crashed world's admission ran the leaves"
        );
        drop(guard_9);

        // The recorded incarnation registers again — how a durable process
        // opener returns — and its drain replays the journaled Pending
        // attempt; the out-of-band resolution settles the child.
        let _guard_7 = register_opener(
            &successor.host,
            &scope_p,
            provider(),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_7,
            tokio_util::sync::CancellationToken::new(),
        );
        let drained = crate::task::spawn({
            let drain = Arc::clone(drain);
            let group_key = group_key.clone();
            async move {
                drain
                    .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
                    .await
            }
        });
        let key3 = observation.parked_key(&format!("{group_key}-call-3")).await;
        resolve_when_registered(
            &successor.host,
            key3,
            crate::Resolution::Ok(serde_json::json!({ "leaf": "incarnation", "via": "resolver" })),
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
            "the recorded incarnation's drain settles its own child: {report:?}"
        );
    } else {
        // The in-memory tier: the gate is the open itself. A live
        // `process(P)#9` does not satisfy it; `process(P)#7` does.
        let host = world.host;
        let scoped = host
            .scoped(crate::AdmittedScope::process(recorded_ref.clone()))
            .expect("the process scope binds");
        let guard_9 = register_opener(
            &host,
            &scope_p,
            provider(),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_9,
            tokio_util::sync::CancellationToken::new(),
        );
        scoped
            .controller()
            .open_effect_group(incarnation_group(
                &scope_p,
                &session_id,
                &group_key,
                &env_ref,
                &recorded_ref,
                deferrable_routing(fixture.deferrable_routing, &host),
                recorded_cancellation_authority(
                    &host,
                    &crate::AdmittedScope::process(recorded_ref.clone()),
                )
                .await,
            ))
            .await
            .expect_err("a group whose recorded incarnation is not the live one refuses to open");
        drop(guard_9);

        let _guard_7 = register_opener(
            &host,
            &scope_p,
            provider(),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_7,
            tokio_util::sync::CancellationToken::new(),
        );
        let mut handle = scoped
            .controller()
            .open_effect_group(incarnation_group(
                &scope_p,
                &session_id,
                &group_key,
                &env_ref,
                &recorded_ref,
                deferrable_routing(fixture.deferrable_routing, &host),
                recorded_cancellation_authority(
                    &host,
                    &crate::AdmittedScope::process(recorded_ref.clone()),
                )
                .await,
            ))
            .await
            .expect("the group opens once its recorded incarnation is live");
        let key0 = observation.parked_key(&format!("{group_key}-call-0")).await;
        let key3 = observation.parked_key(&format!("{group_key}-call-3")).await;
        resolve_when_registered(
            &host,
            key0,
            crate::Resolution::Ok(serde_json::json!({ "leaf": "incarnation", "via": "resolver" })),
        )
        .await;
        resolve_when_registered(
            &host,
            key3,
            crate::Resolution::Ok(serde_json::json!({ "leaf": "incarnation", "via": "resolver" })),
        )
        .await;
        let mut settled = vec![
            next_settlement(&scoped, &mut handle, 0).await,
            next_settlement(&scoped, &mut handle, 1).await,
            next_settlement(&scoped, &mut handle, 2).await,
            next_settlement(&scoped, &mut handle, 3).await,
        ];
        assert_process_settlements(&mut settled, &expected_parent);
        let survivor = settled
            .iter()
            .find(|settlement| settlement.position == 3)
            .expect("the second deferred leaf settled");
        assert!(
            survivor.outcome.is_ok(),
            "the resolved survivor settles: {:?}",
            survivor.outcome
        );
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the group closes");
    }

    // Each deferred leaf ran exactly once — the survivor's journaled Pending
    // replayed on the durable tiers — under its recorded session, with the
    // recorded incarnation's name as its enclosing process.
    let runs = observation.executions_of("law_deferred");
    assert_eq!(
        runs.len(),
        2,
        "each deferred leaf ran exactly once: {runs:?}"
    );
    for run in &runs {
        assert_eq!(
            run.session_id,
            session_id.as_str(),
            "the leaf ran under its recorded session"
        );
        assert_eq!(
            run.enclosing_process.as_deref(),
            Some(process_id.as_str()),
            "the leaf's context reported the recorded enclosing process"
        );
    }
}
