//! The commit-boundary laws: a group child's final commits at its attempt
//! boundary (ADR 0099 §4) and drains are admitted in recorded commit order
//! (§5), on the tier's real substrate — not on a test double.
//!
//! Both laws drive the `law_commit` leaf, whose body waits for a release the
//! law controls and whose settlement declares two recorded intents routed
//! through a [`GatedProcessService`]: the [`IntentSink`] parks an intent write
//! on command, which is exactly the observation "this child is past its §4
//! commit and inside its drain" — a fact no journal column exposes directly.

use pretty_assertions::assert_eq;

use super::*;

/// A group of `LEAF_COMMIT` children: child `position` carries call id
/// `{group_key}-call-{position}` under `disposition`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the group's fields are the law's parameters; a struct would only rename the list"
)]
fn commit_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    children: usize,
    disposition: crate::LoserPolicy,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::RuntimeEffectGroup {
    let parent = parent_invocation(scope);
    let children = (0..children)
        .map(|position| {
            child_envelope(
                scope,
                group_key,
                position,
                leaf_request(
                    scope,
                    session_id,
                    &format!("{group_key}-call-{position}"),
                    LEAF_COMMIT,
                    LEAF_COMMIT.trim_start_matches("tool:"),
                    catalog_admission(LEAF_COMMIT),
                    routing.clone(),
                    env_ref,
                    &parent,
                    cancellation.clone(),
                ),
            )
        })
        .collect();
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
        disposition,
    )
    .expect("the commit group assembles")
}

/// Asserts `sink.landed()` stays empty for [`ABSENCE_BUDGET`]: no intent write
/// may land while a lower-commit sibling's drain is owed.
async fn assert_nothing_landed(sink: &IntentSink, context: &str) {
    tokio::time::sleep(ABSENCE_BUDGET).await;
    assert!(
        sink.landed().is_empty(),
        "{context}: an intent landed ahead of the committed-order barrier: {:?}",
        sink.landed()
    );
}

/// W6/W7: a child whose final record won the §4 point is protected — a close
/// under `Cancel` cannot interrupt its drain, and the drain it owes completes
/// with the committed settlement's intents exactly once.
///
/// * On a durable tier the crash window is real: the child commits, parks
///   inside its drain, the caller closes under `Cancel`, and the process dies.
///   The successor's drain replays the sealed drain input — the body never
///   runs again — and a reopen serves the success terminal at rank 0, not the
///   cancelled failure the close asked for.
/// * On the in-memory tier the same protection holds in-process: the close
///   decides no committed position, the child's held drain finishes once
///   released, and the intents land in declaration order. A closed group's
///   ranks are unreadable by contract on this tier, so the landed intents and
///   the single body execution are the success evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_childs_final_is_protected_and_its_drain_is_finished(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-protected"));
    let turn_id = crate::TurnId::from(format!("{prefix}-protected-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-protected-group");
    let call_0 = format!("{group_key}-call-0");
    let intent_target = crate::ProcessId::from(format!("{session_id}-intent-target"));

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    if probe.drain.is_none() {
        // The in-memory half: the close's cancel token must not interrupt the
        // committed child's drain.
        let host = probe.host;
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let sink = Arc::new(IntentSink::default());
        sink.hold_all();
        let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
            inner: crate::testing::effect_backed_process_service(
                Arc::clone(&scenario.registry),
                Arc::clone(&scenario.process_env_store),
            ),
            sink: Arc::clone(&sink),
        });
        let _guard = register_opener_with_processes(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            processes,
            Arc::clone(&scenario.process_env_store),
            opener,
            tokio_util::sync::CancellationToken::new(),
        );
        let scoped = host
            .scoped(crate::admit(scope.clone()))
            .expect("the group scope binds");
        let handle = scoped
            .controller()
            .open_effect_group(commit_group(
                &scope,
                &session_id,
                &group_key,
                &scenario.env_ref,
                1,
                crate::LoserPolicy::Cancel,
                deferrable_routing(fixture.deferrable_routing, &host),
                recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
            ))
            .await
            .expect("the group opens under the live opener");
        // The child committed and is parked inside its drain.
        sink.await_blocked(&call_0).await;
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::Cancel)
            .await
            .expect("the caller closes under Cancel");
        // The close decided nothing for the committed child: its drain is
        // still held, nothing landed, and the body ran once.
        assert_nothing_landed(&sink, "a committed child closed under Cancel").await;
        assert_eq!(
            scenario.observation.executions_of("law_commit").len(),
            1,
            "the leaf body ran exactly once"
        );
        sink.release_all();
        sink.await_landed_len(2).await;
        assert_eq!(
            sink.landed(),
            vec![(call_0.clone(), "start"), (call_0.clone(), "event")],
            "the protected child's drain landed its declared intents in order"
        );
        assert_eq!(
            scenario.observation.executions_of("law_commit").len(),
            1,
            "the protected child's body never re-ran"
        );
        return;
    }

    // The durable half: the child commits, parks inside its drain, the caller
    // closes under Cancel, and the process dies between the §4 point and the
    // §5 discharge.
    let observation = Arc::new(LawObservation::default());
    let crash_sink = Arc::new(IntentSink::default());
    crashed_world(fixture, {
        let scope = scope.clone();
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let call_0 = call_0.clone();
        let intent_target = intent_target.clone();
        let observation = Arc::clone(&observation);
        let sink = Arc::clone(&crash_sink);
        let routing_kind = fixture.deferrable_routing;
        let opener = opener.clone();
        let make_processes = Arc::clone(&fixture.make_processes);
        move |world| {
            Box::pin(async move {
                sink.hold_all();
                let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
                    definitions: leaf_definitions(),
                    observation,
                    session_id: session_id.clone(),
                    intent_target: intent_target.clone(),
                    start_metadata: serde_json::Value::Null,
                });
                let crash_processes = make_processes().await;
                let env_store = crash_processes.process_env_store;
                let env_ref =
                    crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
                let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
                    inner: crate::testing::effect_backed_process_service(
                        crash_processes.registry,
                        Arc::clone(&env_store),
                    ),
                    sink: Arc::clone(&sink),
                });
                let _guard = register_opener_with_processes(
                    &world.host,
                    &scope,
                    provider,
                    processes,
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
                    .open_effect_group(commit_group(
                        &scope,
                        &session_id,
                        &group_key,
                        &env_ref,
                        1,
                        crate::LoserPolicy::Cancel,
                        deferrable_routing(routing_kind, &world.host),
                        recorded_cancellation_authority(&world.host, &crate::admit(scope.clone()))
                            .await,
                    ))
                    .await
                    .expect("the group opens under the live opener");
                // Past the §4 commit and inside the drain: the first intent
                // write is parked on the sink.
                sink.await_blocked(&call_0).await;
                scoped
                    .controller()
                    .close_effect_group(handle, crate::LoserPolicy::Cancel)
                    .await
                    .expect("the caller closes under Cancel");
                // Returning drops the runtime with the drain still parked.
            })
        }
    })
    .await;
    assert_eq!(
        observation.executions_of("law_commit").len(),
        1,
        "the crashed worker ran the leaf body exactly once"
    );
    assert!(
        crash_sink.landed().is_empty(),
        "no intent landed before the crash"
    );

    // The successor: the journaled committed child is re-driven by the drain,
    // which replays the sealed drain input — the body does not run again.
    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let env_store = (fixture.make_processes)().await.process_env_store;
    let env_ref = crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
    install_child_host(&successor.host, &env_store);
    until_claims_lapse(&successor, &group_key).await;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let sink = Arc::new(IntentSink::default());
    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
        inner: crate::testing::effect_backed_process_service(
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
        ),
        sink: Arc::clone(&sink),
    });
    let _guard = register_opener_with_processes(
        &successor.host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        processes,
        env_store,
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let report = successor
        .drain
        .as_ref()
        .expect("a durable tier hands out a drain")
        .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
        .await
        .expect("a drain pass over the journaled group runs");
    assert!(
        report.children.iter().all(|child| matches!(
            child.outcome,
            crate::testing::conformance_support::ChildDrainOutcome::Settled
        )),
        "the committed child drains to a settlement on the successor: {report:?}"
    );
    assert_eq!(
        sink.landed(),
        vec![(call_0.clone(), "start"), (call_0.clone(), "event")],
        "the committed child's sealed intents landed exactly once, in order"
    );
    assert_eq!(
        observation.executions_of("law_commit").len()
            + scenario.observation.executions_of("law_commit").len(),
        1,
        "the journaled attempt replayed; the leaf body never re-ran"
    );

    // A reopen on the successor serves rank 0: the committed success, not the
    // cancelled failure the close asked for.
    let scoped = successor
        .host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(commit_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            1,
            crate::LoserPolicy::Cancel,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
            recorded_cancellation_authority(&successor.host, &crate::admit(scope.clone())).await,
        ))
        .await
        .expect("the identical group reopens on the successor");
    let settlement = next_settlement(&scoped, &mut handle, 0).await;
    assert_eq!(settlement.position, 0);
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
        outcome,
        settlement,
    }) = &settlement.outcome
    else {
        panic!("the recovered child settles a tool invocation: {settlement:?}")
    };
    let output = format!("{:?}", outcome.record.output);
    assert!(
        !output.contains("RuntimeEffectGroupChildCancelled"),
        "rank 0 is the committed final, not the cancelled outcome the close asked for: {output}"
    );
    assert!(
        matches!(
            outcome.record.output.outcome,
            crate::ToolCallOutcome::Success(_)
        ),
        "rank 0 is the committed child's success"
    );
    assert_eq!(
        settlement.possession.len(),
        1,
        "the realized start names the derived process in the settlement's possession"
    );
    let started = scenario
        .registry
        .get_process(&settlement.possession[0])
        .await
        .expect("the possessed process reads")
        .expect("the possessed process exists");
    let crate::ProcessInput::External { metadata } = &*started.input else {
        panic!(
            "the realized start is an external process: {:?}",
            started.input
        )
    };
    assert_eq!(
        metadata["call_id"],
        serde_json::json!(call_0),
        "the realized start carries the committed child's call id"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::Cancel)
        .await
        .expect("the group closes");
}

/// W19/W20: drains are admitted in recorded final-commit order, not source
/// order. Child B (source position 1) commits first and parks inside its
/// drain; child A (position 0) commits second and must hold at the §5 barrier
/// behind B — no intent of A's lands while B's drain is owed, and the
/// settlement ranks read B then A.
///
/// On a durable tier the crash half runs first: both children committed, B
/// parked mid-drain, A held at the barrier, and the process dies. The
/// successor re-drives both; the recovered drain lands B's intents then A's —
/// commit order, not whichever redrive reached its intents first — and the
/// bodies never re-run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn drains_are_admitted_in_recorded_commit_order(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-commit-order"));
    let turn_id = crate::TurnId::from(format!("{prefix}-commit-order-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let call_a_of = |group_key: &str| format!("{group_key}-call-0");
    let call_b_of = |group_key: &str| format!("{group_key}-call-1");
    let expected_landed = |group_key: &str| {
        vec![
            (call_b_of(group_key), "start"),
            (call_b_of(group_key), "event"),
            (call_a_of(group_key), "start"),
            (call_a_of(group_key), "event"),
        ]
    };

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    if probe.drain.is_some() {
        // The crash half: A held at its body, B free. B commits first and
        // parks inside its drain; A is released, commits second, and waits at
        // the §5 barrier — then the process dies.
        let group_key = format!("{prefix}-commit-order-crash");
        let call_a = call_a_of(&group_key);
        let call_b = call_b_of(&group_key);
        let intent_target = crate::ProcessId::from(format!("{session_id}-intent-target"));
        let observation = Arc::new(LawObservation::default());
        let crash_sink = Arc::new(IntentSink::default());
        crashed_world(fixture, {
            let scope = scope.clone();
            let session_id = session_id.clone();
            let group_key = group_key.clone();
            let call_a = call_a.clone();
            let call_b = call_b.clone();
            let intent_target = intent_target.clone();
            let observation = Arc::clone(&observation);
            let sink = Arc::clone(&crash_sink);
            let routing_kind = fixture.deferrable_routing;
            let opener = opener.clone();
            let make_processes = Arc::clone(&fixture.make_processes);
            move |world| {
                Box::pin(async move {
                    observation.hold(&call_a);
                    sink.hold(&call_b);
                    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
                        definitions: leaf_definitions(),
                        observation: Arc::clone(&observation),
                        session_id: session_id.clone(),
                        intent_target: intent_target.clone(),
                        start_metadata: serde_json::Value::Null,
                    });
                    let crash_processes = make_processes().await;
                    let env_store = crash_processes.process_env_store;
                    let env_ref =
                        crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
                    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
                        inner: crate::testing::effect_backed_process_service(
                            crash_processes.registry,
                            Arc::clone(&env_store),
                        ),
                        sink: Arc::clone(&sink),
                    });
                    let _guard = register_opener_with_processes(
                        &world.host,
                        &scope,
                        provider,
                        processes,
                        env_store,
                        opener,
                        tokio_util::sync::CancellationToken::new(),
                    );
                    let scoped = world
                        .host
                        .scoped(crate::admit(scope.clone()))
                        .expect("the group scope binds");
                    let _handle = scoped
                        .controller()
                        .open_effect_group(commit_group(
                            &scope,
                            &session_id,
                            &group_key,
                            &env_ref,
                            2,
                            crate::LoserPolicy::RunToCompletion,
                            deferrable_routing(routing_kind, &world.host),
                            recorded_cancellation_authority(
                                &world.host,
                                &crate::admit(scope.clone()),
                            )
                            .await,
                        ))
                        .await
                        .expect("the group opens under the live opener");
                    // B's body runs immediately; it commits (commit_seq 1) and
                    // parks at its first intent write.
                    sink.await_blocked(&call_b).await;
                    // Releasing A's body lets A's attempt finish and commit
                    // (commit_seq 2); its drain then waits at the barrier
                    // behind B — it never reaches its own intent writes.
                    observation.release(&call_a);
                    tokio::time::timeout(SETTLE_BUDGET, async {
                        loop {
                            if observation.executions_of("law_commit").len() == 2 {
                                break;
                            }
                            tokio::time::sleep(POLL).await;
                        }
                    })
                    .await
                    .expect("A's leaf body ran once released");
                    // A's commit lands shortly after its body returns; the
                    // sleep is a settle window, and the barrier is what keeps
                    // A's intents from landing in it.
                    tokio::time::sleep(ABSENCE_BUDGET).await;
                    assert!(
                        sink.landed().is_empty(),
                        "A's intents did not land while B's drain was owed"
                    );
                })
            }
        })
        .await;
        assert_eq!(
            observation.executions_of("law_commit").len(),
            2,
            "both bodies ran exactly once before the crash"
        );
        assert!(
            crash_sink.landed().is_empty(),
            "no intent landed before the crash"
        );

        // The successor: both children committed with B's drain owed and A's
        // held behind it. The drain redrives them in commit order.
        let successor = (fixture.make_world)(ToolChildWorldSpec {
            lease_ttl_ms: LIVE_LEASE_MS,
        })
        .await;
        let env_store = (fixture.make_processes)().await.process_env_store;
        let env_ref = crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
        install_child_host(&successor.host, &env_store);
        until_claims_lapse(&successor, &group_key).await;
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let sink = Arc::new(IntentSink::default());
        sink.hold(&call_b);
        let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
            inner: crate::testing::effect_backed_process_service(
                Arc::clone(&scenario.registry),
                Arc::clone(&scenario.process_env_store),
            ),
            sink: Arc::clone(&sink),
        });
        let _guard = register_opener_with_processes(
            &successor.host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            processes,
            env_store,
            opener.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let drain = Arc::clone(
            successor
                .drain
                .as_ref()
                .expect("a durable tier hands out a drain"),
        );
        let drain_key = group_key.clone();
        let draining = crate::task::spawn(async move {
            drain
                .drain_group(&drain_key, &tokio_util::sync::CancellationToken::new())
                .await
        });
        // B is parked inside its drain again; A's redrive answers
        // `AlreadyCommitted` and waits at the barrier — nothing lands.
        sink.await_blocked(&call_b).await;
        assert_nothing_landed(&sink, "the recovered drain").await;
        sink.release(&call_b);
        let report = draining
            .await
            .expect("the drain task joins")
            .expect("the recovery drain pass runs");
        assert!(
            report.children.iter().all(|child| matches!(
                child.outcome,
                crate::testing::conformance_support::ChildDrainOutcome::Settled
            )),
            "both committed children settle on the successor: {report:?}"
        );
        assert_eq!(
            sink.landed(),
            expected_landed(&group_key),
            "the recovered drain landed intents in commit order, not source order"
        );
        assert_eq!(
            observation.executions_of("law_commit").len()
                + scenario.observation.executions_of("law_commit").len(),
            2,
            "the journaled attempts replayed; no body re-ran"
        );

        // A reopen serves the ranks in commit order: B (position 1) then A
        // (position 0).
        let scoped = successor
            .host
            .scoped(crate::admit(scope.clone()))
            .expect("the group scope binds");
        let mut handle = scoped
            .controller()
            .open_effect_group(commit_group(
                &scope,
                &session_id,
                &group_key,
                &env_ref,
                2,
                crate::LoserPolicy::RunToCompletion,
                deferrable_routing(fixture.deferrable_routing, &successor.host),
                recorded_cancellation_authority(&successor.host, &crate::admit(scope.clone()))
                    .await,
            ))
            .await
            .expect("the identical group reopens on the successor");
        let first = next_settlement(&scoped, &mut handle, 0).await;
        let second = next_settlement(&scoped, &mut handle, 1).await;
        assert_eq!(
            first.position, 1,
            "rank 0 is the child that committed first: B, at source position 1"
        );
        assert_eq!(
            second.position, 0,
            "rank 1 is the child that committed second: A, at source position 0"
        );
        assert!(first.outcome.is_ok() && second.outcome.is_ok());
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the group closes");
    }

    // The live half, on every tier: the same commit order holds while the
    // group is still open.
    let group_key = format!("{prefix}-commit-order");
    let call_a = call_a_of(&group_key);
    let call_b = call_b_of(&group_key);
    let host = probe.host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let sink = Arc::new(IntentSink::default());
    scenario.observation.hold(&call_a);
    sink.hold(&call_b);
    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
        inner: crate::testing::effect_backed_process_service(
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
        ),
        sink: Arc::clone(&sink),
    });
    let _guard = register_opener_with_processes(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        processes,
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(commit_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            2,
            crate::LoserPolicy::RunToCompletion,
            deferrable_routing(fixture.deferrable_routing, &host),
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        ))
        .await
        .expect("the group opens under the live opener");
    // B commits first (commit_seq 1) and parks at its first intent write.
    sink.await_blocked(&call_b).await;
    // A's body is released; its attempt finishes and commits second — its
    // drain must wait at the §5 barrier behind B's owed drain.
    scenario.observation.release(&call_a);
    assert_nothing_landed(&sink, "A committed behind B's owed drain").await;
    sink.release(&call_b);
    sink.await_landed_len(4).await;
    assert_eq!(
        sink.landed(),
        expected_landed(&group_key),
        "the drains landed their intents in commit order, not source order"
    );
    let first = next_settlement(&scoped, &mut handle, 0).await;
    let second = next_settlement(&scoped, &mut handle, 1).await;
    assert_eq!(
        first.position, 1,
        "rank 0 is the child that committed first: B, at source position 1"
    );
    assert_eq!(
        second.position, 0,
        "rank 1 is the child that committed second: A, at source position 0"
    );
    assert!(first.outcome.is_ok() && second.outcome.is_ok());
    assert_eq!(
        scenario.observation.executions_of("law_commit").len(),
        2,
        "each leaf body ran exactly once"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");
}
