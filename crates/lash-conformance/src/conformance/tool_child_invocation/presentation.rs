//! The presentation-boundary laws (FIG-3420, ADR 0099 §6): a tool child's
//! model-facing return is the product of an ordered chain of composable
//! [`crate::plugin::ToolPresentationStep`]s, journaled once through the
//! child's `PresentToolResult` effect, and replay serves the record.
//!
//! What is asserted, all through the contract surface:
//!
//! * two steps compose in registration order on the first run, and a
//!   crash/redrive — or a same-host reopen where the tier keeps no journal —
//!   serves the recorded presentation without running either step again;
//! * a changed presentation environment on a successor host cannot change
//!   what the child settled: the reopened group answers the recorded return,
//!   and the successor's steps never run;
//! * the real budget plugin composes with another step in one chain — the
//!   return is both budget-truncated and marker-stamped;
//! * a retained full output is a durable session artifact, named by its
//!   [`crate::AttachmentRef`] in the hint, readable through a second facade
//!   over the same store, never a worker-local path — and replaying the
//!   settlement does not `put` it again.

use pretty_assertions::assert_eq;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// One presentation step that counts its runs and appends `[marker]` as a
/// trailing text part. The counter is the law's proof that replay served the
/// recorded presentation rather than running the chain again.
fn marker_step(
    marker: &'static str,
    runs: Arc<AtomicUsize>,
) -> crate::plugin::ToolPresentationStep {
    Arc::new(move |input: crate::plugin::ToolPresentationInput| {
        runs.fetch_add(1, Ordering::SeqCst);
        let mut next = input.previous;
        next.parts
            .push(crate::ModelToolReturnPart::text(format!("[{marker}]")));
        Box::pin(async move { Ok::<_, crate::PluginError>(next) })
    })
}

/// One plugin factory carrying `steps`, in list order, as its whole spec.
fn steps_factory(
    steps: Vec<crate::plugin::ToolPresentationStep>,
) -> Arc<dyn crate::plugin::PluginFactory> {
    let mut spec = crate::plugin::PluginSpec::new();
    for step in steps {
        spec = spec.with_presentation_step(step);
    }
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "law-presentation",
        spec,
    ))
}

/// The text a settlement's recorded return presents, parts concatenated.
fn presented_text(settlement: &crate::runtime::effect::ToolSettlement) -> String {
    settlement
        .model_return
        .parts
        .iter()
        .filter_map(|part| match part {
            crate::ModelToolReturnPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The rank-0 tool-invocation settlement of `settlement`, or a panic naming
/// what it actually is.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn invocation_settlement(
    settlement: &crate::GroupSettlement,
) -> &crate::runtime::effect::ToolSettlement {
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation { settlement, .. }) = &settlement.outcome
    else {
        panic!("rank 0 settles a tool invocation: {settlement:?}")
    };
    settlement.validate().expect("the settlement validates");
    settlement
}

/// A presentation-law group: one catalog-admitted leaf at rank 0.
fn presentation_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    tool_id: &str,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> crate::RuntimeEffectGroup {
    single_leaf_group(
        scope,
        session_id,
        group_key,
        env_ref,
        tool_id,
        routing,
        cancellation,
    )
}

/// Opens `group` on `host` and serves the rank-0 settlement.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn settle_rank_zero<'a>(
    host: &'a Arc<dyn crate::EffectHost>,
    scope: &crate::ExecutionScope,
    group: crate::RuntimeEffectGroup,
) -> (
    crate::ScopedEffectController<'a>,
    crate::EffectGroupHandle,
    crate::GroupSettlement,
) {
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("the group opens under the live opener");
    let settlement = next_settlement(&scoped, &mut handle, 0).await;
    (scoped, handle, settlement)
}

/// The two-step chain the composition laws register: step A appends `[a]`,
/// step B appends `[b]`, each counting its runs into its own cell.
fn ab_steps() -> (
    Vec<Arc<dyn crate::plugin::PluginFactory>>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let a_runs = Arc::new(AtomicUsize::new(0));
    let b_runs = Arc::new(AtomicUsize::new(0));
    let factories = vec![steps_factory(vec![
        marker_step("a", Arc::clone(&a_runs)),
        marker_step("b", Arc::clone(&b_runs)),
    ])];
    (factories, a_runs, b_runs)
}

/// The changed environment a replay law registers on the successor: step A as
/// before, but step B' appends `[b2]`. If replay ran the chain again — rather
/// than serving the journaled `PresentToolResult` outcome — the returned text
/// would end `[a][b2]` and these counters would move.
fn ab2_steps() -> (
    Vec<Arc<dyn crate::plugin::PluginFactory>>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let a_runs = Arc::new(AtomicUsize::new(0));
    let b2_runs = Arc::new(AtomicUsize::new(0));
    let factories = vec![steps_factory(vec![
        marker_step("a", Arc::clone(&a_runs)),
        marker_step("b2", Arc::clone(&b2_runs)),
    ])];
    (factories, a_runs, b2_runs)
}

/// Two ordered presentation steps compose deterministically on the first run
/// and replay serves the recorded result (FIG-3420).
///
/// Step A appends `[a]`, step B appends `[b]`; registered in that order the
/// settled return ends `…[a][b]` and each step ran once. A replay — a
/// crash/redrive on a drain-bearing tier, a same-host reopen where the tier
/// keeps no journal — serves the recorded `ToolPresentation`: the text is
/// unchanged and neither counter moves.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn two_presentation_steps_compose_deterministically_on_first_run_and_replay(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-compose"));
    let turn_id = crate::TurnId::from(format!("{prefix}-compose-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-compose-group");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let (factories, a_runs, b_runs) = ab_steps();

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    if probe.drain.is_none() {
        // No journal outlives the process on this tier: the replay arm is the
        // same-host reopen, which serves the journaled settlement verbatim.
        let host = probe.host;
        let _guard = register_opener_with_extras(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
            opener,
            tokio_util::sync::CancellationToken::new(),
            OpenerExtras {
                plugin_factories: factories,
                attachment_store: None,
            },
        );
        let group = presentation_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            LEAF_PLAIN,
            ToolChildCompletionRouting::Inline,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        );
        let (scoped, handle, settlement) = settle_rank_zero(&host, &scope, group.clone()).await;
        let presented = presented_text(invocation_settlement(&settlement));
        assert!(
            presented.ends_with("[a][b]"),
            "the steps composed in registration order: {presented:?}"
        );
        assert_eq!(a_runs.load(Ordering::SeqCst), 1, "step A ran once");
        assert_eq!(b_runs.load(Ordering::SeqCst), 1, "step B ran once");
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the group closes");

        // The replay: same group, served from the record — no step re-runs.
        let (scoped, handle, replayed) = settle_rank_zero(&host, &scope, group).await;
        assert!(
            presented_text(invocation_settlement(&replayed)).ends_with("[a][b]"),
            "the reopen serves the recorded settlement"
        );
        assert_eq!(
            a_runs.load(Ordering::SeqCst),
            1,
            "replay served the recorded presentation; step A did not re-run"
        );
        assert_eq!(
            b_runs.load(Ordering::SeqCst),
            1,
            "replay served the recorded presentation; step B did not re-run"
        );
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the reopened group closes");
        return;
    }

    // The durable arm: the first run happens in a worker that then dies; a
    // successor over the same journal serves the recorded presentation.
    let (crash_factories, a_runs, b_runs) = (factories, a_runs, b_runs);
    let (env_store, env_ref) = (scenario.process_env_store.clone(), scenario.env_ref.clone());
    crashed_world(fixture, {
        let scope = scope.clone();
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let provider = Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>;
        let registry = Arc::clone(&scenario.registry);
        let opener = opener.clone();
        move |world| {
            Box::pin(async move {
                let _guard = register_opener_with_extras(
                    &world.host,
                    &scope,
                    provider,
                    registry,
                    env_store,
                    opener,
                    tokio_util::sync::CancellationToken::new(),
                    OpenerExtras {
                        plugin_factories: crash_factories,
                        attachment_store: None,
                    },
                );
                let group = presentation_group(
                    &scope,
                    &session_id,
                    &group_key,
                    &env_ref,
                    LEAF_PLAIN,
                    ToolChildCompletionRouting::Inline,
                    recorded_cancellation_authority(&world.host, &crate::admit(scope.clone()))
                        .await,
                );
                let (scoped, handle, settlement) =
                    settle_rank_zero(&world.host, &scope, group).await;
                let presented = presented_text(invocation_settlement(&settlement));
                assert!(
                    presented.ends_with("[a][b]"),
                    "the steps composed in registration order: {presented:?}"
                );
                scoped
                    .controller()
                    .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                    .await
                    .expect("the group closes before the crash");
            })
        }
    })
    .await;
    assert_eq!(a_runs.load(Ordering::SeqCst), 1, "step A ran once");
    assert_eq!(b_runs.load(Ordering::SeqCst), 1, "step B ran once");

    // The successor: same journal, same registered steps. The reopened group
    // serves the recorded settlement — nothing re-runs.
    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&successor.host, &scenario.process_env_store);
    until_claims_lapse(&successor, &group_key).await;
    let _guard = register_opener_with_extras(
        &successor.host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras {
            plugin_factories: vec![steps_factory(vec![
                marker_step("a", Arc::clone(&a_runs)),
                marker_step("b", Arc::clone(&b_runs)),
            ])],
            attachment_store: None,
        },
    );
    let group = presentation_group(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        LEAF_PLAIN,
        ToolChildCompletionRouting::Inline,
        recorded_cancellation_authority(&successor.host, &crate::admit(scope.clone())).await,
    );
    let (scoped, handle, replayed) = settle_rank_zero(&successor.host, &scope, group).await;
    let presented = presented_text(invocation_settlement(&replayed));
    assert!(
        presented.ends_with("[a][b]"),
        "the redrive serves the recorded presentation: {presented:?}"
    );
    assert_eq!(
        a_runs.load(Ordering::SeqCst) + b_runs.load(Ordering::SeqCst),
        2,
        "the redrive ran no presentation step"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor closes");
}

/// A changed presentation environment on replay cannot change the recorded
/// presentation (FIG-3420): the child's `PresentToolResult` ran once under the
/// env that produced the settlement, and a successor whose step B appends
/// `[b2]` still serves `…[a][b]` — its steps never run at all.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_changed_presentation_environment_on_replay_does_not_change_the_recorded_presentation(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-env"));
    let turn_id = crate::TurnId::from(format!("{prefix}-env-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-env-group");
    let call_id = format!("{group_key}-call-0");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let (factories, a_runs, b_runs) = ab_steps();

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    if probe.drain.is_none() {
        // No journal outlives the process: the reopen serves the recorded
        // settlement even after the opener is re-registered under a changed
        // step chain.
        let host = probe.host;
        let guard = register_opener_with_extras(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
            opener.clone(),
            tokio_util::sync::CancellationToken::new(),
            OpenerExtras {
                plugin_factories: factories,
                attachment_store: None,
            },
        );
        let group = presentation_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            LEAF_PLAIN,
            ToolChildCompletionRouting::Inline,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        );
        let (scoped, handle, settlement) = settle_rank_zero(&host, &scope, group.clone()).await;
        assert!(
            presented_text(invocation_settlement(&settlement)).ends_with("[a][b]"),
            "the first run composed [a][b]"
        );
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the group closes");
        drop(guard);

        // A re-registered opener is a new incarnation; what it observes depends
        // on whether the tier keeps the journal the `PresentToolResult` record
        // lives in.
        let (changed_factories, a2_runs, b2_runs) = ab2_steps();
        let _guard = register_opener_with_extras(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
            opener,
            tokio_util::sync::CancellationToken::new(),
            OpenerExtras {
                plugin_factories: changed_factories,
                attachment_store: None,
            },
        );
        let (scoped, handle, replayed) = settle_rank_zero(&host, &scope, group).await;
        match fixture.deferrable_routing {
            ToolChildDeferrableRouting::ProcessLifetime => {
                // The in-memory tier keeps no journal, so the reopened group
                // re-executes and the changed chain runs: the law observes
                // `[a][b2]` — exactly the bypass a journaled replay must never
                // serve.
                assert!(
                    presented_text(invocation_settlement(&replayed)).ends_with("[a][b2]"),
                    "a new opener incarnation re-runs rather than replaying: the fresh \
                     chain stamps [b2] — the answer a journaled replay must never give"
                );
                assert_eq!(
                    a2_runs.load(Ordering::SeqCst) + b2_runs.load(Ordering::SeqCst),
                    2,
                    "the changed environment's steps ran under the new incarnation"
                );
            }
            ToolChildDeferrableRouting::Durable => {
                // Restate keeps the journal host-side — a durable routing —
                // even though Lash walks no drain of its own, so the reopened
                // group serves the recorded presentation and the changed
                // chain's steps never run.
                let presented = presented_text(invocation_settlement(&replayed));
                assert!(
                    presented.ends_with("[a][b]"),
                    "the journaled presentation survives the opener registration: {presented:?}"
                );
                assert_eq!(
                    a2_runs.load(Ordering::SeqCst) + b2_runs.load(Ordering::SeqCst),
                    0,
                    "the changed environment's steps never ran"
                );
            }
        }
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the reopened group closes");
        return;
    }

    // The durable arm: the child settles under env [a][b] in a worker that
    // then dies; a successor registering env [a][b2] serves the record.
    crashed_world(fixture, {
        let scope = scope.clone();
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let call_id = call_id.clone();
        let provider = Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>;
        let env_store = Arc::clone(&scenario.process_env_store);
        let env_ref = scenario.env_ref.clone();
        let observation = Arc::clone(&scenario.observation);
        let routing_kind = fixture.deferrable_routing;
        let opener = opener.clone();
        move |world| {
            Box::pin(async move {
                let _guard = register_opener_with_extras(
                    &world.host,
                    &scope,
                    provider,
                    Arc::new(crate::TestLocalProcessRegistry::default()),
                    env_store,
                    opener,
                    tokio_util::sync::CancellationToken::new(),
                    OpenerExtras {
                        plugin_factories: factories,
                        attachment_store: None,
                    },
                );
                let scoped = world
                    .host
                    .scoped(crate::admit(scope.clone()))
                    .expect("the group scope binds");
                let mut handle = scoped
                    .controller()
                    .open_effect_group(presentation_group(
                        &scope,
                        &session_id,
                        &group_key,
                        &env_ref,
                        LEAF_DEFERRED,
                        deferrable_routing(routing_kind, &world.host),
                        recorded_cancellation_authority(&world.host, &crate::admit(scope.clone()))
                            .await,
                    ))
                    .await
                    .expect("the group opens under the live opener");
                // The deferred leaf parks; the law resolves it out of band and
                // the child runs its presentation boundary under env [a][b]
                // before the worker dies.
                let key = observation.parked_key(&call_id).await;
                resolve_when_registered(
                    &world.host,
                    key,
                    crate::Resolution::Ok(serde_json::json!({ "leaf": "presented" })),
                )
                .await;
                let settlement = next_settlement(&scoped, &mut handle, 0).await;
                assert!(
                    presented_text(invocation_settlement(&settlement)).ends_with("[a][b]"),
                    "the child presented under env [a][b] before the crash"
                );
            })
        }
    })
    .await;
    assert_eq!(a_runs.load(Ordering::SeqCst), 1, "step A ran once");
    assert_eq!(b_runs.load(Ordering::SeqCst), 1, "step B ran once");

    // The successor registers a changed chain. The reopened group serves the
    // journaled settlement; the changed steps never execute.
    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&successor.host, &scenario.process_env_store);
    until_claims_lapse(&successor, &group_key).await;
    let (changed_factories, a2_runs, b2_runs) = ab2_steps();
    let registry = (fixture.make_registry)().await;
    let _guard = register_opener_with_extras(
        &successor.host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        registry,
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras {
            plugin_factories: changed_factories,
            attachment_store: None,
        },
    );
    let (scoped, handle, replayed) = settle_rank_zero(
        &successor.host,
        &scope,
        presentation_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            LEAF_DEFERRED,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
            recorded_cancellation_authority(&successor.host, &crate::admit(scope.clone())).await,
        ),
    )
    .await;
    let presented = presented_text(invocation_settlement(&replayed));
    assert!(
        presented.ends_with("[a][b]"),
        "the recorded presentation survives the changed environment: {presented:?}"
    );
    assert_eq!(
        a2_runs.load(Ordering::SeqCst) + b2_runs.load(Ordering::SeqCst),
        0,
        "a replay that re-ran the chain would have run the changed steps"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor closes");
}

/// The real budget plugin and another presentation step compose in one chain
/// (FIG-3420): the settled return is budget-truncated *and* carries the
/// second step's marker. Under the retired singleton registration a second
/// projector could not even register; the step chain composes instead.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_oracle_and_the_budget_plugin_coexist(fixture: &ToolChildLawFixture, prefix: &str) {
    let session_id = crate::SessionId::from(format!("{prefix}-coexist"));
    let turn_id = crate::TurnId::from(format!("{prefix}-coexist-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-coexist-group");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let oracle_runs = Arc::new(AtomicUsize::new(0));

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let _guard = register_opener_with_extras(
        &world.host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras {
            plugin_factories: vec![
                Arc::new(
                    lash_plugin_tool_output_budget::ToolOutputBudgetPluginFactory::new(
                        lash_plugin_tool_output_budget::ToolOutputBudgetConfig {
                            mode: lash_plugin_tool_output_budget::ToolOutputBudgetMode::Bytes,
                            limit: 512,
                            max_lines:
                                lash_plugin_tool_output_budget::DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
                            retain_full_output: false,
                        },
                    ),
                ),
                steps_factory(vec![marker_step("oracle", Arc::clone(&oracle_runs))]),
            ],
            attachment_store: None,
        },
    );
    let (scoped, handle, settlement) = settle_rank_zero(
        &world.host,
        &scope,
        presentation_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            LEAF_BIG,
            ToolChildCompletionRouting::Inline,
            recorded_cancellation_authority(&world.host, &crate::admit(scope.clone())).await,
        ),
    )
    .await;
    let presented = presented_text(invocation_settlement(&settlement));
    assert!(
        presented.contains("bytes truncated"),
        "the budget step truncated the oversized output: {presented:?}"
    );
    assert!(
        presented.ends_with("[oracle]"),
        "the second step ran after the budget step: {presented:?}"
    );
    assert_eq!(oracle_runs.load(Ordering::SeqCst), 1);
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");
}

/// An `AttachmentStore` wrapper that counts `put`s, so the law can prove a
/// replayed presentation retains nothing twice.
#[derive(Default)]
struct CountingAttachmentStore {
    inner: crate::InMemoryAttachmentStore,
    puts: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::AttachmentStore for CountingAttachmentStore {
    fn persistence(&self) -> crate::AttachmentStorePersistence {
        self.inner.persistence()
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: crate::AttachmentCreateMeta,
    ) -> Result<crate::AttachmentRef, crate::AttachmentStoreError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(bytes, meta).await
    }

    async fn get(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<crate::StoredAttachment, crate::AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(&self, id: &crate::AttachmentId) -> Result<(), crate::AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<crate::StoredBlobRef>, crate::AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<Option<crate::StoredBlobRef>, crate::AttachmentStoreError> {
        self.inner.head(id).await
    }
}

/// A retained full output is a durable artifact, not a worker-local path
/// (FIG-3420): with `retain_full_output` the truncation hint names an
/// [`crate::AttachmentRef`] id that resolves through a *second* facade over
/// the same store, the hint carries no path, and a reopened group serves the
/// recorded presentation without a second `put`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_retained_full_output_is_a_durable_artifact_not_a_path(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-retain"));
    let turn_id = crate::TurnId::from(format!("{prefix}-retain-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-retain-group");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;

    // The law's own handle on the store the dispatch binds: one backend, two
    // facades — the child's, and the second the law reads through, standing in
    // for another host over the same substrate.
    let backend = Arc::new(CountingAttachmentStore::default());
    let attachment_store = Arc::new(crate::SessionAttachmentStore::ephemeral(
        Arc::clone(&backend) as Arc<dyn crate::AttachmentStore>,
    ));
    let second_reader = crate::SessionAttachmentStore::ephemeral(
        Arc::clone(&backend) as Arc<dyn crate::AttachmentStore>
    );

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let _guard = register_opener_with_extras(
        &world.host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras {
            plugin_factories: vec![Arc::new(
                lash_plugin_tool_output_budget::ToolOutputBudgetPluginFactory::new(
                    lash_plugin_tool_output_budget::ToolOutputBudgetConfig {
                        mode: lash_plugin_tool_output_budget::ToolOutputBudgetMode::Bytes,
                        limit: 512,
                        max_lines:
                            lash_plugin_tool_output_budget::DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
                        retain_full_output: true,
                    },
                ),
            )],
            attachment_store: Some(attachment_store),
        },
    );
    let group = presentation_group(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        LEAF_BIG,
        ToolChildCompletionRouting::Inline,
        recorded_cancellation_authority(&world.host, &crate::admit(scope.clone())).await,
    );
    let (scoped, handle, settlement) = settle_rank_zero(&world.host, &scope, group.clone()).await;
    let presented = presented_text(invocation_settlement(&settlement));

    // The hint names an attachment, never a filesystem path.
    assert!(
        presented.contains("retained as attachment"),
        "the hint names the retained attachment: {presented:?}"
    );
    assert!(
        !presented.contains("saved to:") && !presented.contains("full_output_path"),
        "the hint carries no worker-local path: {presented:?}"
    );
    let attachment_id = presented
        .split("retained as attachment ")
        .nth(1)
        .and_then(|rest| rest.split([' ', '(']).next())
        .map(str::trim)
        .and_then(|id| crate::AttachmentId::parse(id).ok())
        .expect("the hint names a parseable attachment id");
    assert_eq!(
        backend.puts.load(Ordering::SeqCst),
        1,
        "the retained blob was put exactly once"
    );

    // The ref resolves through a second facade over the same store — the
    // law's stand-in for a second host — to the full untruncated bytes.
    let stored = second_reader
        .get(&attachment_id)
        .await
        .expect("the retained ref resolves through the second facade");
    assert_eq!(
        stored.bytes,
        "x".repeat(BIG_OUTPUT_BYTES).into_bytes(),
        "the retained artifact is the full output"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");

    // Replay: the reopened group serves the recorded presentation; nothing
    // is retained twice.
    let (scoped, handle, replayed) = settle_rank_zero(&world.host, &scope, group).await;
    assert!(
        presented_text(invocation_settlement(&replayed)).contains("retained as attachment"),
        "the reopen serves the recorded settlement"
    );
    assert_eq!(
        backend.puts.load(Ordering::SeqCst),
        1,
        "replay served the recorded presentation; no second put ran"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the reopened group closes");
}
