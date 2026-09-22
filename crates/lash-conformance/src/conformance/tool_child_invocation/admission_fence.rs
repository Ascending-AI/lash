//! The admission-fence law: a group child that is cancel-decided may mint no
//! new semantic admission beneath it (ADR 0099 §4, FIG-3470).
//!
//! The child is orchestrating and stays uncommitted: its body makes one
//! nested call to the `law_fence` leaf, whose attempt claim is admitted under
//! the child's own binding while the child is undecided and whose body then
//! parks on the law's gate. The opener closes the group under `Cancel`, which
//! commits the orchestrator's cancel decision — and physically stops the
//! in-flight subtree: on every tier the uncommitted child's execution is
//! terminated at the decision (the token is a physical-stop signal, not
//! authorization), so the parked leaf never returns and its declared intents
//! are never minted. The gated process service records nothing and the
//! definition registry holds no slot.
//!
//! The typed evidence is then the bound controller itself: the law asks the
//! host for the controller the child would have minted its remaining writes
//! through — `scoped_for_group_child`, the same seam `ToolChildHost` drives —
//! and pushes a `RegisterDefinition` admission through it. The substrate
//! answers with `RuntimeEffectGroupChildCancelDecided`: the SQL claim refuses
//! the insert inside its transaction, the native controller refuses under the
//! group mutex, and a handler-bound Restate controller would ask the index's
//! `admit_semantic`. A tier that cannot mint an owned bound controller outside
//! a handler (the Restate ingress host) skips the probe.

use pretty_assertions::assert_eq;

use super::*;

/// The leaf the fence law's orchestrating body calls nested: its body waits
/// on the `held` gate exactly like `law_commit`, then returns a terminal
/// declaring the three sinks the law watches.
const LEAF_FENCE: &str = "tool:law_fence";

/// The engine kind the leaf's `RegisterProcessDefinition` declaration names,
/// registered on the opener's dispatch so the intent's resolve step succeeds
/// and the journaled CAS write is what the fence refuses.
pub(super) const LAW_FENCE_ENGINE_KIND: &str = "law-fence-engine";

/// The call id the orchestrating body gives its nested `law_fence` call — the
/// gate key the leaf's body parks on.
const FENCE_NESTED_CALL: &str = "fence-nested-call";

/// A process engine that exists only to answer `resolve`: the intent
/// realization resolves the pinned reference through it, so the journaled
/// write — not the resolution — is what the fence refuses. The default
/// `resolve` asserts an unknown signature, which `unclaimed` references
/// adopt.
struct LawFenceEngine;

#[async_trait::async_trait]
impl crate::ProcessEngine for LawFenceEngine {
    fn kind(&self) -> &'static str {
        LAW_FENCE_ENGINE_KIND
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        unreachable!("the fence law registers a definition; it never runs one")
    }
}

/// The orchestrating body the law's child runs: one nested call to the held
/// `law_fence` leaf, then a terminal.
struct LawFenceOrchestratingTool {
    definition: crate::ToolDefinition,
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation
    for LawFenceOrchestratingTool
{
    fn manifest(&self) -> crate::ToolManifest {
        self.definition.manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(self.definition.contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        let replies = context
            .call_tool_batch(vec![crate::ToolInvocation::new(
                FENCE_NESTED_CALL,
                crate::ToolId::from(LEAF_FENCE),
                serde_json::json!({}),
            )])
            .await;
        crate::ToolOutcome::ok(serde_json::json!({
            "leaf": "fence-orchestrating",
            "replies": replies.len(),
        }))
    }
}

/// The fence law's orchestrating definition, minted through the same
/// first-party capability boundary `law_orchestrating_tool` uses.
#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is the unsafe capability boundary, and this crate owns the law-local tool contract it registers"
)]
fn law_fence_orchestrating_tool() -> crate::tool_provider::orchestration::OrchestratingToolDef {
    let definition = crate::ToolDefinition::raw(
        LEAF_ORCHESTRATING,
        LEAF_ORCHESTRATING.trim_start_matches("tool:"),
        "conformance orchestrating leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    );
    let implementation: Arc<
        dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
    > = Arc::new(LawFenceOrchestratingTool { definition });
    // SAFETY: lash-internal-conformance owns this law-local tool contract and
    // its body; no leaf provider is upgraded into the orchestrating lane.
    unsafe {
        crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(implementation)
    }
}

/// The opener registration the law needs: `register_opener_with_processes`
/// plus the process-definition registry and the law's resolve-only engine,
/// so the nested leaf's `RegisterProcessDefinition` declaration resolves and
/// reaches the journaled write the fence refuses.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn register_fence_opener(
    host: &Arc<dyn crate::EffectHost>,
    scope: &crate::ExecutionScope,
    provider: Arc<dyn crate::ToolProvider>,
    processes: Arc<dyn crate::ProcessService>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    process_definitions: Arc<dyn crate::ProcessDefinitionRegistry>,
    opener: crate::EffectOpener,
    cooperative: tokio_util::sync::CancellationToken,
) -> crate::runtime::effect::LiveOpenerGuard {
    let installed = install_child_host(host, &process_env_store);
    let admitted = crate::AdmittedScope::new(scope.clone(), opener.process_ref().cloned())
        .expect("the opener's scope and incarnation agree");
    let controller = host
        .scoped_static(admitted.clone())
        .expect("the host lends a scoped controller")
        .expect("this host hands out owned scoped controllers");
    let tool_registry = crate::ToolRegistry::from_tool_provider_with_orchestrating_tools(
        Arc::clone(&provider),
        vec![law_fence_orchestrating_tool()],
    )
    .expect("the law's leaf provider and orchestrating tool register disjoint ids");
    let mut definitions = leaf_definitions();
    definitions.push(crate::ToolDefinition::raw(
        LEAF_ORCHESTRATING,
        LEAF_ORCHESTRATING.trim_start_matches("tool:"),
        "conformance orchestrating leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    ));
    let engines = crate::ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(LawFenceEngine)),
    );
    let dispatch = crate::testing::TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(crate::ToolCatalog::from_tool_definitions(definitions))
        .tool_registry(Arc::new(tool_registry))
        .processes(processes)
        .process_definitions(process_definitions)
        .process_engines(engines)
        .direct_completions(crate::DirectCompletionClient::from_fn(
            |_request, _source| Ok(law_direct_completion()),
        ))
        .process_env_store(process_env_store)
        .borrowed_effect_controller(controller)
        .build()
        .dispatch;
    let lent_controller = host
        .scoped_static(admitted)
        .expect("the host lends a scoped controller")
        .expect("this host hands out owned scoped controllers");
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let context = crate::runtime::effect::LiveOpenerContext::capture_with_event_sender(
        &dispatch,
        lent_controller,
        event_tx,
        cooperative,
    );
    let (guard, ended) = installed.openers().register(opener, context);
    crate::task::spawn(async move {
        tokio::select! {
            _ = ended.cancelled() => {}
            _ = async { while event_rx.recv().await.is_some() {} } => {}
        }
    });
    guard
}

/// A single-child group whose one member runs the orchestrating lane. Returns
/// the child's binding alongside the group, so the law can mint the bound
/// controller the child's remaining writes would have run through.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn fence_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    routing: ToolChildCompletionRouting,
    cancellation: Option<crate::TurnControlBindingId>,
) -> (crate::RuntimeEffectGroup, crate::GroupChildBinding) {
    let parent = parent_invocation(scope);
    let child = child_envelope(
        scope,
        group_key,
        0,
        leaf_request(
            scope,
            session_id,
            &format!("{group_key}-call-0"),
            LEAF_ORCHESTRATING,
            LEAF_ORCHESTRATING.trim_start_matches("tool:"),
            catalog_admission(LEAF_ORCHESTRATING),
            routing,
            env_ref,
            &parent,
            cancellation,
        ),
    );
    let binding = crate::GroupChildBinding {
        child: child.invocation.address.clone(),
        membership: (*child
            .group
            .as_deref()
            .expect("a group child carries its retained membership"))
        .clone(),
    };
    let group = crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key,
        vec![child],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::Cancel,
    )
    .expect("the fence group assembles");
    (group, binding)
}

/// W8: a cancel decision that lands while a group child's nested admission is
/// in flight fences every semantic sink the child still owes (FIG-3470, ADR
/// 0099 §4).
///
/// The orchestrating child is uncommitted. Its body calls the `law_fence`
/// leaf, whose attempt claim is admitted under the child's binding and whose
/// body then parks on the law's gate. The opener closes the group under
/// `Cancel`: the child's cancel decision commits and the substrate physically
/// stops the in-flight subtree, so the leaf never mints the `StartProcess`,
/// `EmitProcessEvent` and `RegisterProcessDefinition` its terminal declared —
/// the gated process service records nothing and the definition registry
/// holds no slot.
///
/// The fence is then probed directly: the controller the child's remaining
/// writes would have minted through — `scoped_for_group_child` — refuses a
/// `RegisterDefinition` admission with the typed
/// `RuntimeEffectGroupChildCancelDecided`. On a durable tier the reopen half
/// serves rank 0 as the cancelled terminal the close committed; on the
/// in-memory and Restate tiers the group's ranks are unreadable by contract
/// once closed, so the probe's typed refusal and the untouched sinks are the
/// evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_cancel_decided_before_a_nested_sink_is_refused_at_the_sink(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-fence"));
    let turn_id = crate::TurnId::from(format!("{prefix}-fence-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-fence-group");
    let owner_scope = crate::TriggerOwnerScope::session(session_id.clone());
    let child_admitted =
        crate::AdmittedScope::unpinned(scope.clone()).expect("a turn scope admits unpinned");

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let definitions: Arc<dyn crate::ProcessDefinitionRegistry> =
        Arc::new(crate::InMemoryProcessDefinitionRegistry::default());
    let sink = Arc::new(IntentSink::default());
    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
        inner: crate::testing::effect_backed_process_service(Arc::clone(&scenario.registry)),
        sink: Arc::clone(&sink),
    });
    // The nested leaf's body parks until the law releases it — held before
    // the group opens so a fast child cannot slip the gate.
    scenario.observation.hold(FENCE_NESTED_CALL);
    let _guard = register_fence_opener(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&processes),
        Arc::clone(&scenario.process_env_store),
        Arc::clone(&definitions),
        opener.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let (group, binding) = fence_group(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        deferrable_routing(fixture.deferrable_routing, &host),
        recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
    );
    let handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("the group opens under the live opener");

    // The nested leaf's body is parked on the gate: its attempt claim was
    // admitted under the orchestrator's binding while the child was still
    // undecided.
    tokio::time::timeout(SETTLE_BUDGET, async {
        loop {
            if !scenario.observation.executions_of("law_fence").is_empty() {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the nested leaf's attempt was admitted and its body parked");

    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::Cancel)
        .await
        .expect("the caller closes under Cancel");
    scenario.observation.release(FENCE_NESTED_CALL);

    // The typed evidence: mint the controller the child's remaining writes
    // would have run through and push a semantic admission through it. The
    // substrate answers `RuntimeEffectGroupChildCancelDecided` — the decision
    // is committed, so nothing the child still owes may mint. A tier that
    // cannot mint an owned bound controller outside a handler (the Restate
    // ingress host) reports no controller and skips the probe.
    match host.scoped_for_group_child(child_admitted, binding) {
        Ok(Some(bound)) => {
            let envelope = crate::RuntimeEffectEnvelope::new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(scope.clone(), format!("{group_key}:fenced-write"))
                        .expect("valid probe address"),
                    crate::RuntimeAttribution::none(),
                    format!("{group_key}:fenced-write"),
                ),
                crate::RuntimeEffectCommand::process(crate::ProcessCommand::RegisterDefinition {
                    owner_scope: owner_scope.clone(),
                    name: format!("{group_key}-fenced-definition"),
                    pinned: crate::ProcessDefinitionRef::unclaimed(
                        LAW_FENCE_ENGINE_KIND,
                        serde_json::json!({ "program": "law-fence" }),
                    ),
                    expectation: None,
                }),
            );
            let error = bound
                .execute_effect(
                    envelope,
                    crate::RuntimeEffectLocalExecutor::process_definitions(Arc::clone(
                        &definitions,
                    )),
                )
                .await
                .expect_err(
                    "a bound controller refuses the minted write once the child is \
                             cancel-decided",
                );
            assert_eq!(
                error.code.as_str(),
                "runtime_effect_group_child_cancel_decided",
                "the refusal is the typed cancel-decided admission error"
            );
        }
        Ok(None) => {
            // The host mints no owned scoped controller at all — the probe has
            // no seam to reach.
        }
        Err(error) if error.code == crate::RuntimeErrorCode::EffectGroupUnsupported => {
            // The tier cannot mint an owned bound controller outside a
            // handler (the Restate ingress host): the fence lives inside the
            // child's invocation there, so the probe has no seam to reach.
        }
        Err(error) => panic!("minting the bound controller failed: {error:?}"),
    }

    assert!(
        sink.landed().is_empty(),
        "the cancelled child's declared intents never reached the process service: {:?}",
        sink.landed()
    );
    let registered = definitions
        .list_definitions(&owner_scope)
        .await
        .expect("the definition registry lists");
    assert!(
        registered.is_empty(),
        "the cancelled child's definition registration never landed: {registered:?}"
    );

    if world.drain.is_some() {
        // A durable tier serves the cancel-decided child's recorded terminal
        // to a reopen: rank 0 is the cancelled outcome, not a body result.
        let scoped = host
            .scoped(crate::admit(scope.clone()))
            .expect("the group scope binds");
        let (group, _) = fence_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            deferrable_routing(fixture.deferrable_routing, &host),
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        );
        let mut handle = scoped
            .controller()
            .open_effect_group(group)
            .await
            .expect("the identical group reopens");
        let settlement = next_settlement(&scoped, &mut handle, 0).await;
        assert_eq!(settlement.position, 0);
        let error = settlement
            .outcome
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("rank 0 is the cancelled terminal: {settlement:?}"));
        assert_eq!(
            error.code.as_str(),
            "runtime_effect_group_child_cancelled",
            "rank 0 is the cancelled terminal the close decided"
        );
    }
}
