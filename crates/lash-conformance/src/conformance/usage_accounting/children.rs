#![expect(clippy::expect_used, reason = "conformance fixture assertions")]
use super::*;
use lash_core::core_internal::ToolChildHostRuntimeOps as _;
use lash_core::runtime::effect::{EffectLayer, LayeredEffectHost};

#[derive(Default)]
pub(super) struct Children {
    ledgers: std::sync::Mutex<Vec<crate::session::IncorporationLedger>>,
}

impl Children {
    #[expect(clippy::expect_used, reason = "conformance fixture assertions")]
    pub(super) async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let index = call.args["call"].as_u64().expect("child position");
        for abort in [false, true] {
            let request = crate::DirectRequest::text(
                serde_json::json!({
                    "usage_child": {"index": index, "abort": abort}
                })
                .to_string(),
            );
            let completion = call
                .context
                .direct_completions()
                .complete(request, "usage-child")
                .await
                .expect("managed nested call returns its attempt evidence");
            assert_eq!(
                completion.llm_call.attempts.len(),
                if abort { 1 } else { 2 }
            );
            if abort {
                assert_eq!(
                    completion.llm_call.attempts[0].outcome,
                    crate::AttemptOutcome::Aborted
                );
            }
        }
        crate::ToolOutcome::ok(serde_json::json!({"child": index})).into()
    }
}

/// The first operation after incorporation records the crash boundary. The
/// changed build reconstructs it differently before any opener model dispatch.
struct AfterIncorporation {
    drift: bool,
    crash: crate::ConformanceCrash,
}

#[async_trait::async_trait]
impl EffectLayer for AfterIncorporation {
    async fn execute_effect(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        envelope: crate::RuntimeEffectEnvelope,
        executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(
            envelope.command,
            crate::RuntimeEffectCommand::LlmCall { .. }
        ) {
            inner
                .execute_effect(
                    crate::RuntimeEffectEnvelope::new(
                        crate::RuntimeEffectInvocation::new(
                            crate::EffectAddress::new(
                                envelope.invocation.address().execution_scope.clone(),
                                "after-child-incorporation",
                            )
                            .expect("stable boundary"),
                            crate::RuntimeAttribution::none(),
                            "incorporated",
                        ),
                        crate::RuntimeEffectCommand::LanguageRuntimeValue {
                            operation: if self.drift {
                                "after-incorporation-v2"
                            } else {
                                "after-incorporation-v1"
                            }
                            .into(),
                        },
                    ),
                    crate::RuntimeEffectLocalExecutor::testing(|_| async {
                        Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: serde_json::json!("incorporated"),
                        })
                    }),
                )
                .await?;
            assert!(!self.drift, "changed boundary refuses replay");
            self.crash.fire();
            std::future::pending::<()>().await;
        }
        inner.execute_effect(envelope, executor).await
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
async fn incorporate(world: &World, scoped: &crate::ScopedEffectController<'_>) {
    let runtime = world.runtime().await;
    let lent = world
        .tier
        .effect_host
        .scoped_static(world.admitted())
        .expect("scope")
        .expect("owned controller");
    let dispatch = Arc::new(
        runtime
            .tool_child_dispatch(lent)
            .expect("real managed dispatch"),
    );
    let envs = world.tier.stores.process_env_store();
    let child_host = world
        .tier
        .effect_host
        .install_tool_child_host(crate::runtime::effect::ToolChildHost::new(
            &world.tier.effect_host,
            Arc::clone(&envs),
            Arc::clone(&dispatch.clock),
        ))
        .expect("child host");
    let lent = world
        .tier
        .effect_host
        .scoped_static(world.admitted())
        .expect("scope")
        .expect("owned controller");
    let (guard, _) = child_host.openers().register(
        crate::EffectOpener::for_scope(&world.admitted()).expect("opener"),
        crate::runtime::effect::LiveOpenerContext::capture(
            &dispatch,
            lent,
            tokio_util::sync::CancellationToken::new(),
        ),
    );
    // The opener's handler may suspend at the open's first await, and its
    // registration ends with it. Pin its context for the group's children
    // first, as the session's own tool-group opener does.
    let group_key = "usage-three-children";
    child_host.pin_open_tool_group(
        group_key,
        &crate::EffectOpener::for_scope(&world.admitted()).expect("opener"),
        0..3,
    );
    let env = dispatch
        .execution_env_spec
        .stable_ref()
        .expect("environment reference");
    let claim = crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
        crate::HostArtifactPin::mint(),
    ))
    .expect("environment host pin");
    envs.publish_process_execution_env(
        &claim,
        &env,
        &dispatch
            .execution_env_spec
            .to_store_bytes()
            .expect("environment bytes"),
    )
    .await
    .expect("publish captured environment");
    let scope = world.admitted();
    let parent = crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(scope.scope().clone(), "usage-children").expect("parent address"),
        crate::RuntimeAttribution::none(),
        "usage-children",
    );
    let binding = world
        .tier
        .effect_host
        .turn_control_binding(scoped)
        .await
        .expect("record cancellation binding");
    let definition = probe_definition();
    let children: Vec<_> = (0..3)
        .map(|index| {
            let request = crate::runtime::effect::ToolChildRequest::new(
                crate::PreparedToolCall {
                    call_id: crate::ToolCallId::fixture(&format!("usage-child-{index}")),
                    provider_call_id: None,
                    tool_id: definition.manifest().id,
                    tool_name: PROBE.into(),
                    args: serde_json::json!({"call": index}),
                    replay: None,
                    prepared_payload: serde_json::Value::Null,
                },
                crate::runtime::effect::ToolChildAdmission::Catalog {
                    manifest: Box::new(definition.manifest()),
                },
                crate::tool_dispatch::ToolAttemptLineage::under(parent.clone()),
                crate::runtime::effect::ToolChildScope {
                    opener: crate::EffectOpener::for_scope(&scope).expect("opener"),
                    owner: dispatch.owner.clone(),
                },
                crate::TurnControlBindingId::new(binding.binding_id().to_string())
                    .expect("cancellation authority"),
                env.clone(),
                crate::runtime::effect::ToolChildCompletionRouting::Inline,
                crate::runtime::effect::ToolChildSessionFacts {
                    tool_surface: vec![definition.clone()],
                    ..Default::default()
                },
            );
            crate::RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(
                        scope.scope().clone(),
                        format!("{group_key}:child:{index}"),
                    )
                    .expect("child address"),
                    crate::RuntimeAttribution::none(),
                    "child",
                ),
                crate::RuntimeEffectCommand::ToolInvocation {
                    request: Box::new(request),
                },
            )
        })
        .collect();
    let group = crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.scope().clone(), "usage-three-children:group")
                .expect("group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key,
        children,
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("three-child group");
    let mut handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("open group");
    let context = crate::testing::TestExecutionContextBuilder::over_controller(scoped.clone())
        .session_id(world.session_id.clone())
        .direct_completions(dispatch.direct_completions.clone())
        .build()
        .into_runtime();
    for _ in 0..3 {
        scoped
            .controller()
            .await_next_settlement(
                &mut handle,
                crate::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new()),
            )
            .await
            .expect("all three children settle");
        context
            .incorporate_group_prefix(&handle)
            .await
            .expect("incorporate paid child");
    }
    let ledger = context.incorporation_ledger_snapshot();
    assert_eq!(ledger.incorporated.len(), 3);
    let encoded = serde_json::to_value(&ledger).expect("actual ledger serializes");
    assert_eq!(
        encoded
            .as_object()
            .expect("ledger object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["incorporated"],
        "incorporation carries identities only"
    );
    world
        .children
        .as_ref()
        .expect("children enabled")
        .ledgers
        .lock_recover()
        .push(ledger);
    assert!(
        context
            .incorporate_group_prefix(&handle)
            .await
            .expect("same prefix again")
            .is_empty()
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("close settled group");
    drop(guard);
}

fn attempt(world: &World, drift: bool) -> crate::ConformanceTurnAttempt {
    let world = world.clone();
    Arc::new(move |scoped| {
        let world = world.clone();
        Box::pin(async move {
            incorporate(&world, &scoped).await;
            let scoped = LayeredEffectHost::layer_scoped(
                scoped,
                Arc::new(AfterIncorporation {
                    drift,
                    crash: world.kill.clone(),
                }),
            )
            .expect("post-incorporation layer");
            let result = world.shift(scoped).await;
            assert!(
                result.as_ref().is_err_and(|error| error.code.parks_turn()),
                "the replayed opener parks: {result:?}"
            );
            crate::ConformanceTurnEnd::of(&result)
        })
    })
}

/// E6: each child's real billed retry and aborted nested call is owned by
/// its ToolAttempt run. Incorporation, replay and permanent parking add no charge.
pub async fn tool_child_spend_counts_once_without_settlement_charging(tier: &UsageAccountingTier) {
    let mut world = World::new(tier, "three-paid-children", Script::completed(1));
    world.children = Some(Arc::new(Children::default()));
    tier.runner
        .run_turn_until_crash(world.admitted(), attempt(&world, false), world.kill.clone())
        .await;
    assert_eq!(world.invocations(), 9);
    let before = charged_children(&world).await;
    assert_eq!(before.completeness.unreported_attempts, 3);
    tier.runner
        .run_parking_turn_until_rested(world.admitted(), attempt(&world, true))
        .await;
    let ledgers = world
        .children
        .as_ref()
        .expect("children enabled")
        .ledgers
        .lock_recover()
        .clone();
    assert!(
        ledgers.len() >= 2,
        "the opener incorporated before and after its crash"
    );
    assert!(
        ledgers.iter().all(|ledger| ledger == &ledgers[0]),
        "replay restores exactly the incorporated identities"
    );
    let after = charged_children(&world).await;
    assert_eq!(
        after
            .rows
            .iter()
            .fold(crate::TokenUsage::default(), |total, row| total
                .saturating_add(&row.usage)
                .0),
        world.returned_total()
    );
    let facts = world.facts().await;
    assert_eq!(facts.len(), 9);
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.disposition() == crate::UsageReporting::Reported)
            .count(),
        6
    );
    assert_eq!(after, before, "incorporation never adds accounting");
    assert_eq!(
        world.invocations(),
        9,
        "replay and park dispatch no provider call"
    );
    let store =
        crate::conformance::law_session_store(tier.stores.as_ref(), &world.session_id).await;
    assert!(
        store
            .load_turn_park(&world.session_id)
            .await
            .expect("park")
            .is_some()
    );
}

async fn charged_children(world: &World) -> crate::OwnerUsage {
    let deadline = tokio::time::Instant::now() + DELIVERY;
    let usage = loop {
        let usage = world
            .tier
            .stores
            .usage_accounting()
            .load_owner_usage(&world.owner())
            .await
            .expect("read child usage");
        if usage.completeness.open_runs == 0 {
            break usage;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "child settlements delivered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let facts = world.facts().await;
    assert_eq!(facts.len(), 9);
    assert_eq!(
        facts
            .iter()
            .map(|fact| fact.identity())
            .collect::<BTreeSet<_>>()
            .len(),
        9
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.disposition() == crate::UsageReporting::Reported)
            .count(),
        world.returned_attempts()
    );
    assert_eq!(usage.completeness.unknown_runs, 0);
    assert_eq!(usage.completeness.conflicted_runs, 0);
    assert_eq!(usage.completeness.unreported_attempts, 3);
    usage
}
