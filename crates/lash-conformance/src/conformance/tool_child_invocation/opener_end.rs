//! The opener-end laws: every group an aggregate forms is finished and
//! incorporated by the opener's own end, whatever stopped its consumer
//! (FIG-3397, ADR 0099 §6, §7, §13).
//!
//! Both laws race a loser that the tier cannot finish on its own: the
//! `law_spend_commit` leaf spends a managed-LLM call, commits its final record
//! and parks inside its drain on an [`IntentSink`]. Its spend (§13) and the
//! process its drain starts (§6) are facts only the opener's incorporation
//! lands, so they are what the laws observe — through the opener's own usage
//! sink and possession set, the two channels its accounting commits from.

use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::incorporation::RecordingCharge;
use super::*;
use lash_core::testing::conformance_support::EffectGroupLifecycle;

/// An opener's execution context over `host`, as a turn's phase context is:
/// its own opener state, the host's closing seam, the charge sink its
/// incorporations land in, and the cancellation its turn would carry.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn opener_context(
    host: &Arc<dyn crate::EffectHost>,
    session_id: &crate::SessionId,
    provider: Arc<dyn crate::ToolProvider>,
    processes: Arc<dyn crate::ProcessService>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    charge: Arc<RecordingCharge>,
    cancel: CancellationToken,
) -> crate::RuntimeExecutionContext<'static> {
    let admitted = crate::admit(opener_scope(session_id));
    let controller = host
        .scoped_static(admitted)
        .expect("the host lends a scoped controller")
        .expect("this host hands out owned scoped controllers");
    let tool_registry = crate::ToolRegistry::from_tool_provider_with_orchestrating_tools(
        Arc::clone(&provider),
        vec![law_orchestrating_tool()],
    )
    .expect("the law's leaf provider and orchestrating tool register disjoint ids");
    crate::testing::TestExecutionContextBuilder::new(crate::testing::TestExecutionPorts::over_host(
        Arc::clone(host),
        process_env_store,
    ))
    .session_id(session_id.clone())
    .provider(provider)
    .tool_catalog(crate::ToolCatalog::from_tool_definitions(leaf_definitions()))
    .tool_registry(Arc::new(tool_registry))
    .processes(processes)
    .direct_completions(
        crate::DirectCompletionClient::from_fn(|_request, _source| Ok(law_direct_completion()))
            .with_usage_charge_sink(charge),
    )
    .borrowed_effect_controller(controller)
    .route_tool_children()
    .build()
    .into_runtime()
    .with_opener_state(crate::session::OpenerState::new(
        crate::session::OpenerWorkBound::default(),
    ))
    .with_group_closing(host.effect_group_closing())
    .with_cancellation_token(cancel)
}

/// The one turn every run of a law's session belongs to: a retried end is the
/// same opener, so it is the same scope.
fn opener_scope(session_id: &crate::SessionId) -> crate::ExecutionScope {
    crate::ExecutionScope::turn(
        session_id.clone(),
        crate::TurnId::from(format!("{session_id}-turn")),
    )
}

fn call(session_id: &crate::SessionId, name: &str, tool: &str) -> crate::ToolInvocation {
    crate::ToolInvocation::new(
        format!("{session_id}-{name}"),
        crate::ToolId::from(tool),
        serde_json::json!({ "leaf": name }),
    )
}

/// One aggregate, addressed by its command key (FIG-3586). The key names the
/// session, as a production cell's does through its own replay key; the group
/// it forms is keyed under the opener's `{scope}:group:` prefix, so the
/// opener's end finishes it.
fn aggregate(
    session_id: &crate::SessionId,
    consumer: crate::session::ToolAggregateConsumer,
    calls: Vec<crate::ToolInvocation>,
) -> crate::session::ToolAggregateRequest {
    crate::session::ToolAggregateRequest {
        leaves: calls
            .into_iter()
            .map(crate::session::ToolAggregateLeaf::Tool)
            .collect(),
        consumer,
        settled_value_after: None,
        command: crate::CommandReplayKey::new(format!("{session_id}:aggregate:lk2:0000000000")),
    }
}

/// The one group the session pins, while it pins one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn pinned_group(host: &Arc<dyn crate::EffectHost>, session_id: &crate::SessionId) -> String {
    let closing = host
        .effect_group_closing()
        .expect("a tier that runs group laws hands out its closing seam");
    let pins = closing
        .read_session_pins(session_id)
        .await
        .expect("the session's pins are readable");
    assert_eq!(pins.len(), 1, "the aggregate formed one group: {pins:?}");
    pins.into_iter().next().expect("one pin")
}

/// Waits until `group_key`'s recorded lifecycle is `closing`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn until_closing(host: &Arc<dyn crate::EffectHost>, group_key: &str) {
    let closing = host
        .effect_group_closing()
        .expect("a tier that runs group laws hands out its closing seam");
    tokio::time::timeout(SETTLE_BUDGET, async {
        loop {
            if matches!(
                closing
                    .read_group_lifecycle(group_key)
                    .await
                    .expect("the lifecycle is readable"),
                Some(EffectGroupLifecycle::Closing { .. })
            ) {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the opener's end records `closing`");
}

/// What an opener's end must have incorporated from the spending loser: its
/// one spend, once, and the process its drain started.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_loser_incorporated(
    context: &crate::RuntimeExecutionContext<'_>,
    charge: &RecordingCharge,
    host: &Arc<dyn crate::EffectHost>,
    session_id: &crate::SessionId,
    law: &str,
) {
    assert_eq!(
        charge.count(),
        1,
        "{law}: the loser's spend was charged into the opener's ledger exactly once"
    );
    assert!(
        !context.started_process_ids().is_empty(),
        "{law}: the process the loser's drain started is the opener's possession"
    );
    let pins = host
        .effect_group_closing()
        .expect("the closing seam")
        .read_session_pins(session_id)
        .await
        .expect("the session's pins are readable");
    assert!(
        pins.is_empty(),
        "{law}: the group settled, so nothing pins the session against deletion: {pins:?}"
    );
}

/// §6, §7, §13: a turn cancelled while its `any` is parked on rank 2 — rank 1
/// consumed as a rejection — hands the group back to its opener. A loser whose
/// final record committed before the cancel keeps its authority, drains and
/// ranks afterwards; the opener's end incorporates that rank: its spend and
/// the process it started.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_cancelled_aggregates_committed_loser_is_incorporated_by_its_openers_end(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;
    let session_id = crate::SessionId::from(format!("{prefix}-cancelled-aggregate"));
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
    let charge = Arc::new(RecordingCharge::default());
    let cancel = CancellationToken::new();
    let context = opener_context(
        &host,
        &session_id,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        processes,
        Arc::clone(&scenario.process_env_store),
        Arc::clone(&charge),
        cancel.clone(),
    );
    let spender = format!("{session_id}-spender");
    scenario.observation.hold(&spender);
    let consumer = context.clone();
    let request = aggregate(
        &session_id,
        crate::session::ToolAggregateConsumer::Any,
        vec![
            call(&session_id, "rejected", LEAF_FAIL),
            call(&session_id, "parked", LEAF_DEFERRED),
            call(&session_id, "spender", LEAF_SPEND_COMMIT),
        ],
    );
    let consuming = crate::task::spawn(async move { consumer.call_tool_aggregate(request).await });

    // The parked loser holds its completion key; the spender has spent,
    // committed its final record and is inside its drain.
    scenario
        .observation
        .parked_key(&format!("{session_id}-parked"))
        .await;
    scenario.observation.release(&spender);
    sink.await_blocked(&spender).await;

    cancel.cancel();
    let outcome = tokio::time::timeout(SETTLE_BUDGET, consuming)
        .await
        .expect("the cancelled aggregate answers")
        .expect("the consuming task");
    assert!(
        matches!(
            outcome,
            crate::session::ToolAggregateOutcome::HostControl(_)
        ),
        "a cancelled await answers on the host-control channel"
    );

    // The committed loser finishes its drain under the closed group.
    sink.release_all();
    sink.await_landed_len(1).await;

    context
        .close_opener_groups()
        .await
        .expect("the opener's end closes, finalizes and incorporates");
    assert_loser_incorporated(
        &context,
        &charge,
        &host,
        &session_id,
        "a cancelled aggregate's committed loser",
    )
    .await;
}

/// §7, §13, W16: an opener's end records `closing` and then fails — the worker
/// dies, or the end is abandoned — before it finalizes. The opener's retried
/// end (the same scope, a fresh process) finds the `closing` group it formed,
/// finishes the loser's owed drain, and incorporates the loser's spend and
/// possession; the session is then deletable.
///
/// * On a durable tier the first end dies with its runtime while parked in
///   finalization, so the host's own finalizer dies with it and `closing` is
///   all the journal holds.
/// * On a drain-less tier the first end is abandoned mid-finalization, and
///   the retried end runs while the loser's drain is still held — so the law
///   can see that the end waits for the obligation rather than skipping a
///   group it did not open in this process.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_retried_openers_end_finishes_the_closing_group_its_first_end_left(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-retried-end"));
    let spender = format!("{session_id}-spender");
    let race = || {
        aggregate(
            &session_id,
            crate::session::ToolAggregateConsumer::Race,
            vec![
                call(&session_id, "winner", LEAF_PLAIN),
                call(&session_id, "spender", LEAF_SPEND_COMMIT),
            ],
        )
    };

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    if probe.drain.is_none() {
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
        scenario.observation.hold(&spender);
        let first = opener_context(
            &host,
            &session_id,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            Arc::clone(&processes),
            Arc::clone(&scenario.process_env_store),
            Arc::new(RecordingCharge::default()),
            CancellationToken::new(),
        );
        let outcome = first.call_tool_aggregate(race()).await;
        assert!(
            matches!(
                outcome,
                crate::session::ToolAggregateOutcome::Selected { leaf: 0, .. }
            ),
            "the plain leaf wins while the spender is held"
        );
        let group_key = pinned_group(&host, &session_id).await;
        scenario.observation.release(&spender);
        sink.await_blocked(&spender).await;

        // The first end closes, then parks in finalization on the held
        // drain, and is abandoned there.
        let ending = first.clone();
        let first_end = crate::task::spawn(async move { ending.close_opener_groups().await });
        until_closing(&host, &group_key).await;
        first_end.abort();

        let charge = Arc::new(RecordingCharge::default());
        let retried = opener_context(
            &host,
            &session_id,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            processes,
            Arc::clone(&scenario.process_env_store),
            Arc::clone(&charge),
            CancellationToken::new(),
        );
        let ending = retried.clone();
        let retried_end = crate::task::spawn(async move { ending.close_opener_groups().await });
        tokio::time::sleep(ABSENCE_BUDGET).await;
        assert!(
            !retried_end.is_finished(),
            "the retried end waits for the closing group's owed drain rather than skipping it"
        );
        sink.release_all();
        tokio::time::timeout(SETTLE_BUDGET, retried_end)
            .await
            .expect("the retried end finishes once the drain lands")
            .expect("the retried end's task")
            .expect("the retried end finalizes and incorporates");
        assert_loser_incorporated(
            &retried,
            &charge,
            &host,
            &session_id,
            "an abandoned end's closing group",
        )
        .await;
        return;
    }
    drop(probe);

    // The durable half: the first end dies with its worker. The crashed
    // world and its successor share one durable store set.
    let stores = (fixture.make_processes)().await;
    let intent_target =
        register_intent_target(stores.process_registry().as_ref(), &session_id).await;
    let group_key = Arc::new(std::sync::Mutex::new(None::<String>));
    crashed_world(fixture, {
        let session_id = session_id.clone();
        let spender = spender.clone();
        let group_key = Arc::clone(&group_key);
        let first_race = race();
        let stores = Arc::clone(&stores);
        let intent_target = intent_target.clone();
        move |world| {
            Box::pin(async move {
                let observation = Arc::new(LawObservation::default());
                let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
                    definitions: leaf_definitions(),
                    observation: Arc::clone(&observation),
                    session_id: session_id.clone(),
                    intent_target: intent_target.clone(),
                    start_metadata: serde_json::Value::Null,
                });
                let crash_processes = Arc::clone(&stores);
                let env_store = crash_processes.process_env_store();
                let _env_ref =
                    crate::testing::process_execution_env_fixture(env_store.as_ref()).await;
                let sink = Arc::new(IntentSink::default());
                sink.hold_all();
                let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
                    inner: crate::testing::effect_backed_process_service(
                        crash_processes.process_registry(),
                        Arc::clone(&env_store),
                    ),
                    sink: Arc::clone(&sink),
                });
                observation.hold(&spender);
                let first = opener_context(
                    &world.host,
                    &session_id,
                    provider,
                    processes,
                    env_store,
                    Arc::new(RecordingCharge::default()),
                    CancellationToken::new(),
                );
                let outcome = first.call_tool_aggregate(first_race).await;
                assert!(
                    matches!(
                        outcome,
                        crate::session::ToolAggregateOutcome::Selected { leaf: 0, .. }
                    ),
                    "the plain leaf wins while the spender is held"
                );
                let key = pinned_group(&world.host, &session_id).await;
                observation.release(&spender);
                sink.await_blocked(&spender).await;
                let ending = first.clone();
                crate::task::spawn(async move { ending.close_opener_groups().await });
                until_closing(&world.host, &key).await;
                *group_key.lock_recover() = Some(key);
                // Returning drops the runtime with the end parked in
                // finalization and the loser's drain still held.
            })
        }
    })
    .await;
    let group_key = group_key
        .lock_recover()
        .clone()
        .expect("the crashed worker formed the group");

    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let scenario = scenario_on(
        stores,
        &session_id,
        serde_json::Value::Null,
        Some(intent_target),
    )
    .await;
    install_child_host(&successor.host, &scenario.process_env_store);
    until_claims_lapse(&successor, &group_key).await;
    let closing = successor
        .host
        .effect_group_closing()
        .expect("the closing seam");
    assert!(
        matches!(
            closing
                .read_group_lifecycle(&group_key)
                .await
                .expect("the lifecycle is readable"),
            Some(EffectGroupLifecycle::Closing { .. })
        ),
        "the crash left the group closing"
    );
    let charge = Arc::new(RecordingCharge::default());
    let retried = opener_context(
        &successor.host,
        &session_id,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        crate::testing::effect_backed_process_service(
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
        ),
        Arc::clone(&scenario.process_env_store),
        Arc::clone(&charge),
        CancellationToken::new(),
    );
    tokio::time::timeout(SETTLE_BUDGET, retried.close_opener_groups())
        .await
        .expect("the retried end finishes")
        .expect("the retried end finalizes and incorporates");
    assert_loser_incorporated(
        &retried,
        &charge,
        &successor.host,
        &session_id,
        "a crashed end's closing group",
    )
    .await;
}
