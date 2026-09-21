//! One cross-tier law: a durable effect group's tool children run through the
//! handler-level invocation driver, not a prepared batch (FIG-2266, ADR 0099
//! §2, §3, §6, §13).
//!
//! The group suite proves the *group* contract — retained membership, rank
//! order, reopen fencing. This law proves the *child* contract: each member of
//! a group is a `ToolInvocation` envelope carrying a `ToolChildRequest`, and the
//! host routes it to the driver that runs retry, deferred-completion
//! coordination, grant/catalog branching and orchestration at handler level
//! while only the atomic `ToolAttempt` executes inside a recorded body.
//!
//! What is asserted, all through the contract surface:
//!
//! * a **plain** leaf settles with its resolved `ModelToolReturn`;
//! * a **retry** leaf's failed attempt is journaled — the successful second
//!   attempt settles the child, and the settlement records the retry;
//! * a **deferred** leaf parks on its own journaled await at handler level and
//!   settles only when an out-of-band resolution lands against the key it
//!   took — the group serves nothing for it in between;
//! * a **granted** leaf executes under its recorded `ToolExecutionGrant`, which
//!   is authority the live catalog never saw;
//! * an **intents** leaf's declarations are realized by the child after its
//!   attempt commits — a process start lands in `possession`, a process event
//!   lands in the registry, and both outcomes ride the settlement;
//! * a **usage** leaf's managed-LLM spend inside the attempt is captured into
//!   the journaled `ToolAttempt` outcome and aggregated onto the settlement;
//! * an **orchestrating** leaf runs its body directly — no invented outer
//!   `ToolAttempt` — and its work is real: a nested call runs through the
//!   child's own rebound dispatch as a journaled attempt, and the durable
//!   process start the body realizes rides the settlement's `possession`.
//!
//! A second scenario covers the recovery routing rule: a reopened group whose
//! child's opener is not live on this host is **not refused** and **not run** —
//! the child stays accepted — and once the opener registers on the recovering
//! host, a further reopen runs it to a settlement the first host never saw.
//!
//! The tier arrives as a host factory: two calls are two views of one
//! substrate (for the SQL tiers, two connections over one store; for the
//! in-memory reference host, the same object, whose substrate is the
//! process). Restate is not registered here for the same reason it is absent
//! from the batch law: it reports `supports_concurrent_effects() == false`
//! today, and no expected-failure mechanism exists or may be added.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lash_sansio::sync::MutexExt as _;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;

use crate::runtime::effect::ToolChildCompletionRouting;

/// How long the law waits for a settlement that must arrive.
const SETTLE_BUDGET: Duration = Duration::from_secs(30);

/// How long the law waits to observe that a settlement does *not* arrive.
///
/// Wall-clock is legitimate here in a way the batch law's rendezvous forbids:
/// the assertion is an *absence*, and absence cannot be proven by rendezvous.
/// The bound is generous enough that a genuinely-settling child would have
/// settled.
const ABSENCE_BUDGET: Duration = Duration::from_secs(2);

/// The leaf tool ids. Each names one lane of the driver the law exercises.
const LEAF_PLAIN: &str = "tool:law_plain";
const LEAF_RETRY: &str = "tool:law_retry";
const LEAF_DEFERRED: &str = "tool:law_deferred";
const LEAF_GRANTED: &str = "tool:law_granted";
const LEAF_INTENTS: &str = "tool:law_intents";
const LEAF_USAGE: &str = "tool:law_usage";
const LEAF_ORCHESTRATING: &str = "tool:law_orchestrating";
/// The leaf the recovery scenario parks. Distinct from the deferred leaf so
/// the two scenarios' completion keys and execution logs cannot collide.
const LEAF_RECOVERY: &str = "tool:law_recovery";

/// One host over the substrate under test, plus the drain it hands out.
pub struct ToolChildWorld {
    /// The effect host a law opens groups through.
    pub host: Arc<dyn crate::EffectHost>,
    /// The group drain over the same journal, where the tier keeps one. `None`
    /// on the in-memory reference host, which journals nothing across a
    /// process boundary and so has nothing to drain; the recovery law reads
    /// this to decide which half of the routing contract the tier can speak
    /// to.
    pub drain: Option<Arc<dyn crate::testing::conformance_support::StoreEffectGroupDrain>>,
}

/// What a law needs the next world to be built with.
pub struct ToolChildWorldSpec {
    /// The effect-claim lease window. The recovery law's crash phase needs
    /// claims that lapse inside a test's patience; the lane law needs claims
    /// that cannot. A tier without claim leases ignores it.
    pub lease_ttl_ms: u64,
}

/// Builds a world over one substrate, as many times as a law needs.
///
/// Callable from any runtime — the recovery law calls it from a runtime it is
/// about to destroy — so a backend must open its substrate handles inside the
/// future rather than capturing handles bound to the test's runtime.
pub type ToolChildWorldFactory = Arc<
    dyn Fn(ToolChildWorldSpec) -> std::pin::Pin<Box<dyn Future<Output = ToolChildWorld> + Send>>
        + Send
        + Sync,
>;

/// Builds a fresh process registry for one scenario, supplied by the tier.
///
/// A factory rather than a handle because each scenario opens its own
/// registry: a durable registry must not carry the previous scenario's rows.
pub type ToolChildRegistryFactory = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Arc<dyn crate::ProcessRegistry>> + Send>>
        + Send
        + Sync,
>;

/// What a tier supplies: a world factory over one substrate, a process
/// registry factory, and the completion routing it would record for a
/// deferrable child.
#[derive(Clone)]
pub struct ToolChildLawFixture {
    /// Two calls are two views of one substrate. On the SQL tiers each call
    /// builds a fresh host over the shared store; on the in-memory reference
    /// tier both calls return the same host, because the process *is* the
    /// substrate there.
    pub make_world: ToolChildWorldFactory,
    /// A fresh process registry on the substrate the host factory serves.
    pub make_registry: ToolChildRegistryFactory,
    /// The completion routing this tier would record for a deferrable child:
    /// `Durable` where a resolution survives the worker, `ProcessLifetime`
    /// where the host's keys die with the process (ADR 0099 §14).
    pub deferrable_routing: ToolChildDeferrableRouting,
}

/// Which routing fact a tier records for a deferrable child.
///
/// A kind rather than the value itself because `ProcessLifetime` carries the
/// issuing registry's identity, which is only known once the world is built —
/// [`deferrable_routing`] resolves the kind against the host at group
/// construction.
#[derive(Clone, Copy, Debug)]
pub enum ToolChildDeferrableRouting {
    /// A completion resolution survives the worker that issued it.
    Durable,
    /// Completion keys die with the issuing process.
    ProcessLifetime,
}

/// Resolves the tier's deferrable routing kind into the recorded fact, binding
/// a process-lifetime key to this host's registry identity.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a host mints a non-empty registry identity"
)]
fn deferrable_routing(
    kind: ToolChildDeferrableRouting,
    host: &Arc<dyn crate::EffectHost>,
) -> ToolChildCompletionRouting {
    match kind {
        ToolChildDeferrableRouting::Durable => ToolChildCompletionRouting::Durable,
        ToolChildDeferrableRouting::ProcessLifetime => {
            ToolChildCompletionRouting::ProcessLifetime {
                issuer: crate::runtime::TurnControlBindingId::new(host.turn_control_binding_id())
                    .expect("a host's registry identity is a valid binding id"),
            }
        }
    }
}

/// The lease window the lane law and the recovery law's live phases use:
/// longer than the law, so a claim expiring mid-test can never be mistaken
/// for the journal honoring it.
const LIVE_LEASE_MS: u64 = 60_000;

/// The lease window the recovery law's crash phase uses: long enough that the
/// parked child's claim is not lost while its process is still working, short
/// enough that a dead process's claim becomes reclaimable inside a test's
/// patience.
const CRASH_LEASE_MS: u64 = 900;

/// How often the recovery law polls for lapsed claims.
const POLL: Duration = Duration::from_millis(25);

/// What one leaf execution reported to the law.
#[derive(Clone, Debug)]
struct LeafExecution {
    /// The tool name the leaf was invoked under.
    tool: String,
    /// The session the attempt's context was bound to — the child's recorded
    /// one when the driver honoured the retained request, the lending
    /// opener's when it did not.
    session_id: String,
    /// The one-based attempt number the context stamped.
    attempt: u32,
    /// The execution binding the attempt carried — the grant's, for a granted
    /// leaf; `Null` for a catalog one.
    execution_binding: serde_json::Value,
}

/// The shared observation surface between the leaf provider and the law.
#[derive(Default)]
struct LawObservation {
    /// Executions in the order the runtime produced them.
    executions: std::sync::Mutex<Vec<LeafExecution>>,
    /// The completion key each deferrable leaf took, keyed by call id.
    parked_keys: std::sync::Mutex<HashMap<String, crate::AwaitEventKey>>,
    /// Signalled whenever `parked_keys` gains an entry.
    parked: Notify,
}

impl LawObservation {
    fn record(
        &self,
        tool: &str,
        session_id: &str,
        attempt: u32,
        execution_binding: serde_json::Value,
    ) {
        self.executions.lock_recover().push(LeafExecution {
            tool: tool.to_string(),
            session_id: session_id.to_string(),
            attempt,
            execution_binding,
        });
    }

    fn executions_of(&self, tool: &str) -> Vec<LeafExecution> {
        self.executions
            .lock_recover()
            .iter()
            .filter(|execution| execution.tool == tool)
            .cloned()
            .collect()
    }

    fn park(&self, call_id: &str, key: crate::AwaitEventKey) {
        self.parked_keys
            .lock_recover()
            .insert(call_id.to_string(), key);
        self.parked.notify_waiters();
    }

    /// Waits until the leaf under `call_id` has parked, and answers its key.
    async fn parked_key(&self, call_id: &str) -> crate::AwaitEventKey {
        tokio::time::timeout(SETTLE_BUDGET, async {
            loop {
                if let Some(key) = self.parked_keys.lock_recover().get(call_id).cloned() {
                    return key;
                }
                self.parked.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("the deferred leaf under `{call_id}` never parked on a completion key")
        })
    }
}

/// One definition per leaf lane, plus the retry policy the retry leaf needs.
fn leaf_definitions() -> Vec<crate::ToolDefinition> {
    [
        LEAF_PLAIN,
        LEAF_RETRY,
        LEAF_DEFERRED,
        LEAF_GRANTED,
        LEAF_INTENTS,
        LEAF_USAGE,
        LEAF_RECOVERY,
    ]
    .into_iter()
    .map(|id| {
        let name = id.trim_start_matches("tool:");
        let mut definition = crate::ToolDefinition::raw(
            id,
            name,
            format!("conformance leaf {name}"),
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        );
        if id == LEAF_RETRY {
            definition = definition.with_retry_policy(crate::ToolRetryPolicy::safe(3, 0, 0));
        }
        definition
    })
    .collect()
}

/// The grant the granted leaf is admitted under: authority the live catalog
/// never saw, carried by the recorded request alone.
fn leaf_grant() -> crate::ToolExecutionGrant {
    let definition = leaf_definitions()
        .into_iter()
        .find(|definition| definition.manifest().id == crate::ToolId::from(LEAF_GRANTED))
        .unwrap_or_else(|| unreachable!("the granted leaf definition exists"));
    crate::ToolExecutionGrant::from_definition(definition)
        .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID)
        .with_execution_binding(serde_json::json!({ "route": "granted-by-request" }))
}

/// The grant the same-ID-orchestrator child is admitted under: authority over
/// the very id the live registry holds as orchestrating, so any lane decision
/// that consulted the registration instead of the recorded admission would
/// route the call into an orchestrating body the grant never described.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn orchestrator_id_grant() -> crate::ToolExecutionGrant {
    let definition = crate::ToolDefinition::raw(
        LEAF_ORCHESTRATING,
        LEAF_ORCHESTRATING.trim_start_matches("tool:"),
        "conformance orchestrating leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    );
    crate::ToolExecutionGrant::from_definition(definition)
        .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID)
        .with_execution_binding(serde_json::json!({ "route": "granted-over-orchestrator" }))
}

/// The leaf provider every child but the orchestrating one executes under.
struct LawLeafProvider {
    definitions: Vec<crate::ToolDefinition>,
    observation: Arc<LawObservation>,
    session_id: crate::SessionId,
    /// The process the intents leaf emits to, registered by the law.
    intent_target: crate::ProcessId,
    /// What the intents leaf declares as its started process's metadata, so the
    /// law can find the derived process id in the settlement's possession.
    start_metadata: serde_json::Value,
}

#[async_trait::async_trait]
impl crate::ToolProvider for LawLeafProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.definitions
            .iter()
            .map(|definition| definition.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.definitions
            .iter()
            .find(|definition| definition.manifest().name == name)
            .map(|definition| Arc::new(definition.contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        *tool_id == crate::ToolId::from(LEAF_DEFERRED)
            || *tool_id == crate::ToolId::from(LEAF_RECOVERY)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let context = call.context;
        let name = call.name().to_string();
        self.observation.record(
            &name,
            context.session_id(),
            context.attempt_number(),
            context.tool_execution_binding().clone(),
        );
        match name.as_str() {
            name if name == LEAF_PLAIN.trim_start_matches("tool:") => {
                crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::ok(
                    serde_json::json!({ "leaf": "plain" }),
                ))
            }
            name if name == LEAF_RETRY.trim_start_matches("tool:") => {
                if context.attempt_number() == 1 {
                    crate::ToolOutcome::failure(crate::ToolFailure::safe_retry(
                        crate::ToolFailureClass::Internal,
                        "law_retry_first_attempt",
                        "the first attempt is journaled as a retryable failure",
                        Some(0),
                    ))
                    .into()
                } else {
                    crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::ok(
                        serde_json::json!({ "leaf": "retry", "attempt": context.attempt_number() }),
                    ))
                }
            }
            name if name == LEAF_DEFERRED.trim_start_matches("tool:")
                || name == LEAF_RECOVERY.trim_start_matches("tool:") =>
            {
                match context.completion_key() {
                    Ok(key) => {
                        let call_id = context
                            .tool_call_id()
                            .unwrap_or("missing-call-id")
                            .to_string();
                        self.observation.park(&call_id, key);
                        crate::ToolAttemptOutcome::pending(crate::PendingCompletion::new())
                    }
                    Err(error) => crate::ToolOutcome::err_fmt(format!(
                        "the deferred leaf could not take a completion key: {error}"
                    ))
                    .into(),
                }
            }
            name if name == LEAF_GRANTED.trim_start_matches("tool:") => {
                crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::ok(
                    serde_json::json!({ "leaf": "granted" }),
                ))
            }
            // Reached only by the rank-7 child: a catalog-admitted call on
            // this id is claimed by the orchestrating lane before the provider
            // is asked, so a provider execution under this name is, by
            // construction, a call that ran as a leaf under its own grant.
            name if name == LEAF_ORCHESTRATING.trim_start_matches("tool:") => {
                crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::ok(
                    serde_json::json!({ "leaf": "orchestrating-as-leaf" }),
                ))
            }
            name if name == LEAF_INTENTS.trim_start_matches("tool:") => {
                crate::ToolAttemptOutcome::done(
                    crate::ToolOutcomeDone::ok(serde_json::json!({ "leaf": "intents" })),
                    crate::ToolIntents::v3(vec![
                        crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
                            session_id: self.session_id.clone(),
                            declaration: crate::ProcessStartDeclaration::external(
                                crate::ProcessOriginator::host(),
                                self.start_metadata.clone(),
                                crate::ProcessLifecyclePolicy::new(
                                    crate::ParentScope::Host,
                                    crate::OnParentEnd::Abandon,
                                ),
                            ),
                        })),
                        crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                            session_id: self.session_id.clone(),
                            process_id: self.intent_target.clone(),
                            event_type: "law.intent-event".to_string(),
                            payload: serde_json::json!({ "leaf": "intents" }),
                        }),
                    ]),
                )
            }
            name if name == LEAF_USAGE.trim_start_matches("tool:") => {
                match context
                    .direct_completions()
                    .complete(
                        crate::DirectRequest::text("law-model", "spend inside the attempt"),
                        "law-usage-leaf",
                    )
                    .await
                {
                    Ok(completion) => {
                        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::ok(
                            serde_json::json!({ "leaf": "usage", "text": completion.text }),
                        ))
                    }
                    Err(error) => {
                        crate::ToolOutcome::err_fmt(format!("direct completion failed: {error}"))
                            .into()
                    }
                }
            }
            other => {
                crate::ToolOutcome::err_fmt(format!("the law has no leaf named {other}")).into()
            }
        }
    }
}

/// The canned managed-LLM completion the usage leaf's direct call is answered
/// with, and the spend the settlement must carry.
fn law_direct_completion() -> crate::DirectCompletion {
    crate::DirectCompletion {
        text: "law direct completion".to_string(),
        usage: crate::TokenUsage {
            input_tokens: 41,
            output_tokens: 7,
            ..crate::TokenUsage::default()
        },
        llm_call: crate::LlmCallRecord {
            call_id: crate::LlmCallId("law-direct-call".to_string()),
            label: None,
            replay_drops: Vec::new(),
            attempts: vec![crate::AttemptRecord {
                ordinal: 1,
                started_at: 0,
                duration: std::time::Duration::ZERO,
                outcome: crate::AttemptOutcome::Completed,
                protocol_position: crate::ProtocolPosition::ResponseObserved,
                retry_budget_consumed: false,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: Some(crate::llm::types::LlmUsage {
                    input_tokens: 41,
                    output_tokens: 7,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                }),
                usage_disposition: crate::AttemptUsageDisposition::default(),
            }],
        },
    }
}

/// The orchestrating child's body. It does the work §2 and §6 actually
/// describe: a nested call coordinated through the child's own rebound
/// dispatch — a journaled `ToolAttempt` under the child's admitted
/// controller, not a `RuntimeExecutionContext` borrow — and a durable
/// process start, which the settlement's `possession` must record because an
/// orchestrating body has no attempt frame whose intents would carry it.
struct LawOrchestratingTool {
    definition: crate::ToolDefinition,
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation for LawOrchestratingTool {
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
                "nested-law-call",
                crate::ToolId::from(LEAF_PLAIN),
                serde_json::json!({}),
            )])
            .await;
        let nested_ok = matches!(
            replies.first().map(|reply| &reply.output.outcome),
            Some(crate::ToolCallOutcome::Success(_))
        );
        // The started id derives from the body's own call id, so a redrive
        // re-requests the same start rather than minting a second process.
        let started_id = crate::ProcessId::from(format!(
            "{}-started",
            context.tool_call_id().unwrap_or("law-orchestrating")
        ));
        match context
            .start_process(crate::ProcessStartRequest::external(
                started_id,
                crate::ProcessOriginator::host(),
                serde_json::json!({ "lane": "orchestrating" }),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            ))
            .await
        {
            Ok(view) => crate::ToolOutcome::ok(serde_json::json!({
                "leaf": "orchestrating",
                "replies": replies.len(),
                "nested_ok": nested_ok,
                "started": view.process_id.to_string(),
            })),
            Err(error) => crate::ToolOutcome::err_fmt(format!(
                "the orchestrating body's process start failed: {error}"
            )),
        }
    }
}

/// The orchestrating definition, minted through the first-party capability
/// boundary this crate is allowed to hold: conformance owns this law-local
/// contract, and no leaf provider is being upgraded.
#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is the unsafe capability boundary, and this crate owns the law-local tool contract it registers"
)]
fn law_orchestrating_tool() -> crate::tool_provider::orchestration::OrchestratingToolDef {
    let definition = crate::ToolDefinition::raw(
        LEAF_ORCHESTRATING,
        LEAF_ORCHESTRATING.trim_start_matches("tool:"),
        "conformance orchestrating leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    );
    let implementation: Arc<
        dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
    > = Arc::new(LawOrchestratingTool { definition });
    // SAFETY: lash-internal-conformance owns this law-local tool contract and
    // its body; no leaf provider is upgraded into the orchestrating lane.
    unsafe {
        crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(implementation)
    }
}

/// The dispatch context one opener lends its children on one host view.
///
/// Every field the driver rebinds comes from the child's recorded request;
/// everything the law asserts on is lent through here: the leaf provider and
/// its registry, the process service over the tier's registry, the canned
/// direct-completion client that feeds the real usage-recording path.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn opener_dispatch(
    host: &Arc<dyn crate::EffectHost>,
    scope: &crate::ExecutionScope,
    provider: Arc<dyn crate::ToolProvider>,
    registry: Arc<dyn crate::ProcessRegistry>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
) -> Arc<crate::tool_dispatch::ToolDispatchContext<'static>> {
    let controller = host
        .scoped_static(scope.clone())
        .expect("the host lends a scoped controller")
        .expect("this host hands out owned scoped controllers");
    let tool_registry = crate::ToolRegistry::from_tool_provider_with_orchestrating_tools(
        Arc::clone(&provider),
        vec![law_orchestrating_tool()],
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
    crate::testing::TestExecutionContextBuilder::new()
        .provider(provider)
        .tool_catalog(crate::ToolCatalog::from_tool_definitions(definitions))
        .tool_registry(Arc::new(tool_registry))
        .processes(crate::testing::effect_backed_process_service(registry))
        .direct_completions(crate::DirectCompletionClient::from_fn(
            |_request, _source| Ok(law_direct_completion()),
        ))
        .process_env_store(process_env_store)
        .borrowed_effect_controller(controller)
        .build()
        .dispatch
}

/// One child envelope: a `ToolInvocation` command whose request reconstructs
/// the child from the journal alone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn child_envelope(
    scope: &crate::ExecutionScope,
    group_key: &str,
    position: usize,
    request: crate::runtime::effect::ToolChildRequest,
) -> crate::RuntimeEffectEnvelope {
    crate::RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:child:{position}"))
                .expect("valid group-child address"),
            crate::RuntimeAttribution::none(),
            "effect",
        ),
        crate::RuntimeEffectCommand::ToolInvocation {
            request: Box::new(request),
        },
    )
}

/// The request one leaf's child is admitted under.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the request's fields are the leaf's parameters; a struct would only rename the list"
)]
fn leaf_request(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    call_id: &str,
    tool_id: &str,
    tool_name: &str,
    admission: crate::runtime::effect::ToolChildAdmission,
    routing: ToolChildCompletionRouting,
    env_ref: &crate::ProcessExecutionEnvRef,
    parent: &crate::RuntimeInvocation,
) -> crate::runtime::effect::ToolChildRequest {
    let crate::ExecutionScope::Turn { .. } = scope else {
        unreachable!("the law's children are admitted under a turn scope")
    };
    crate::runtime::effect::ToolChildRequest::new(
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
            opener: crate::EffectOpener::for_scope(scope, None)
                .expect("a turn scope derives an opener"),
            admitted_scope: scope.clone(),
            session_id: session_id.clone(),
            agent_frame_id: crate::FrameNodeId::new("law-frame").expect("a valid frame id"),
        },
        env_ref.clone(),
        routing,
    )
}

fn catalog_admission(tool_id: &str) -> crate::runtime::effect::ToolChildAdmission {
    let definitions = leaf_definitions();
    let mut all = definitions.clone();
    all.push(crate::ToolDefinition::raw(
        LEAF_ORCHESTRATING,
        LEAF_ORCHESTRATING.trim_start_matches("tool:"),
        "conformance orchestrating leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    ));
    let manifest = all
        .into_iter()
        .find(|definition| definition.manifest().id == crate::ToolId::from(tool_id))
        .unwrap_or_else(|| unreachable!("every leaf has a definition"))
        .manifest();
    crate::runtime::effect::ToolChildAdmission::Catalog {
        manifest: Box::new(manifest),
    }
}

/// Installs the tool-child host on `host`, returning the installed resolver.
///
/// Separate from [`register_opener`] because the recovery law needs a host
/// whose resolver is wired but whose opener is absent: `executor_for` answers
/// `None` there for a different reason than "no resolver at all", and the law
/// is about the first.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn install_child_host(
    host: &Arc<dyn crate::EffectHost>,
    process_env_store: &Arc<dyn crate::ProcessExecutionEnvStore>,
) -> Arc<crate::runtime::effect::ToolChildHost> {
    host.install_tool_child_host(crate::runtime::effect::ToolChildHost::new(
        host,
        Arc::clone(process_env_store),
    ))
    .expect("a tier that routes tool children installs the child host")
}

/// Registers `opener` as live on `host` with the dispatch context the children
/// will be lent, installing the tool-child host if it is not installed yet.
///
/// The returned guard is the registration's whole lifetime: dropping it is the
/// "this worker's opener is gone" edge the recovery phase drives. The
/// event-channel forwarder is bounded by the registration, exactly as the turn
/// path's is.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn register_opener(
    host: &Arc<dyn crate::EffectHost>,
    scope: &crate::ExecutionScope,
    provider: Arc<dyn crate::ToolProvider>,
    registry: Arc<dyn crate::ProcessRegistry>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    opener: crate::EffectOpener,
    cooperative: tokio_util::sync::CancellationToken,
) -> crate::runtime::effect::LiveOpenerGuard {
    let installed = install_child_host(host, &process_env_store);
    let dispatch = opener_dispatch(host, scope, provider, registry, process_env_store);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let context = crate::runtime::effect::LiveOpenerContext::capture_with_event_sender(
        &dispatch,
        event_tx,
        cooperative,
    )
    .expect("the law's dispatch context is 'static");
    let (guard, ended) = installed.openers().register(opener, context);
    // The registration owns the sender's lifetime: the forwarder ends when the
    // entry leaves the registry, not when the channel's last clone drops.
    crate::task::spawn(async move {
        tokio::select! {
            _ = ended.cancelled() => {}
            _ = async { while event_rx.recv().await.is_some() {} } => {}
        }
    });
    guard
}

/// Everything one scenario needs that is not the host: the observation log,
/// the leaf provider, the process registry and the published environment.
struct Scenario {
    observation: Arc<LawObservation>,
    provider: Arc<LawLeafProvider>,
    registry: Arc<dyn crate::ProcessRegistry>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    env_ref: crate::ProcessExecutionEnvRef,
    intent_target: crate::ProcessId,
}

/// Stands up the scenario state: the process registry with the intent target
/// registered, the environment the children's requests record, and the leaf
/// provider that serves them.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn scenario(
    fixture: &ToolChildLawFixture,
    session_id: &crate::SessionId,
    start_metadata: serde_json::Value,
) -> Scenario {
    let registry = (fixture.make_registry)().await;
    let intent_target = crate::ProcessId::from(format!("{session_id}-intent-target"));
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                intent_target.clone(),
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "law.intent-event".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
            std::slice::from_ref(session_id),
        )
        .await
        .expect("register the intent target process");
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();
    let observation = Arc::new(LawObservation::default());
    Scenario {
        provider: Arc::new(LawLeafProvider {
            definitions: leaf_definitions(),
            observation: Arc::clone(&observation),
            session_id: session_id.clone(),
            intent_target: intent_target.clone(),
            start_metadata,
        }),
        observation,
        registry,
        process_env_store,
        env_ref,
        intent_target,
    }
}

/// The parent invocation the children's recorded identities derive from — the
/// cell invocation a real batch would carry, built the way production mints
/// one rather than spelled as a string.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn parent_invocation(scope: &crate::ExecutionScope) -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::EffectAddress::new(scope.clone(), "law:batch-cell")
            .expect("a valid parent effect address"),
        crate::RuntimeAttribution::none(),
        "law:batch-cell",
    )
}

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

/// The deferred leaf's out-of-band resolver: resolves `key` against `host`,
/// retrying while the await has not registered yet on this substrate.
async fn resolve_when_registered(
    host: &Arc<dyn crate::EffectHost>,
    key: crate::AwaitEventKey,
    resolution: crate::Resolution,
) {
    let deadline = std::time::Instant::now() + SETTLE_BUDGET;
    loop {
        match host.resolve_await_event(&key, resolution.clone()).await {
            Ok(crate::ResolveOutcome::Accepted)
            | Ok(crate::ResolveOutcome::AlreadyResolved { .. }) => return,
            Ok(crate::ResolveOutcome::UnknownOrRevoked) | Err(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the deferred leaf's await never registered a resolvable key"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Polls `host`'s outstanding await registry until `key` is listed — the
/// journal-visible half of `parked_key`, which fires inside the attempt body
/// before the coordinator's commits have landed.
async fn await_key_registered(
    host: &Arc<dyn crate::EffectHost>,
    session_id: &crate::SessionId,
    key: &crate::AwaitEventKey,
) {
    let deadline = std::time::Instant::now() + SETTLE_BUDGET;
    loop {
        match host.list_outstanding_await_event_keys(session_id).await {
            Ok(keys) if keys.contains(key) => return,
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the deferred leaf's await key never became durable"
                );
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

/// Awaits one settlement inside the budget, naming the awaited rank on timeout.
async fn next_settlement(
    scoped: &crate::ScopedEffectController<'_>,
    handle: &mut crate::EffectGroupHandle,
    rank: usize,
) -> crate::GroupSettlement {
    tokio::time::timeout(
        SETTLE_BUDGET,
        scoped
            .controller()
            .await_next_settlement(handle, tokio_util::sync::CancellationToken::new()),
    )
    .await
    .unwrap_or_else(|_| panic!("settlement for rank {rank} never arrived"))
    .unwrap_or_else(|error| panic!("settlement for rank {rank} failed to be served: {error}"))
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
    let opener =
        crate::EffectOpener::for_scope(&scope, None).expect("a turn scope derives an opener");
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
    );
    let scoped = host.scoped(scope.clone()).expect("the group scope binds");
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
        .events_after(&scenario.intent_target, 0)
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

// =============================================================================
// Recovery: an opener that is not live on this host
// =============================================================================

/// A single-child group: one catalog-admitted leaf at rank 0.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn single_leaf_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    tool_id: &str,
    routing: ToolChildCompletionRouting,
) -> crate::RuntimeEffectGroup {
    let parent = parent_invocation(scope);
    let child = child_envelope(
        scope,
        group_key,
        0,
        leaf_request(
            scope,
            session_id,
            &format!("{group_key}-call-0"),
            tool_id,
            tool_id.trim_start_matches("tool:"),
            catalog_admission(tool_id),
            routing,
            env_ref,
            &parent,
        ),
    );
    crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key,
        vec![child],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the single-leaf group assembles")
}

/// A single-child group: the recovery leaf alone, parked on its deferred
/// completion key.
fn recovery_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    routing: ToolChildCompletionRouting,
) -> crate::RuntimeEffectGroup {
    single_leaf_group(
        scope,
        session_id,
        group_key,
        env_ref,
        LEAF_RECOVERY,
        routing,
    )
}

/// The crash: run `phase` on a world of its own, on a runtime of its own, and
/// destroy the runtime afterwards.
///
/// Dropping a Tokio runtime drops every task it owns — the host-owned child
/// tasks among them — and the host's substrate handles with them. What is left
/// behind is what a killed worker leaves: journaled rows under claims nobody
/// renews.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn crashed_world<P>(fixture: &ToolChildLawFixture, phase: P)
where
    P: FnOnce(ToolChildWorld) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + 'static,
{
    let make = Arc::clone(&fixture.make_world);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the crashing process gets a runtime of its own");
        runtime.block_on(async move {
            let world = make(ToolChildWorldSpec {
                lease_ttl_ms: CRASH_LEASE_MS,
            })
            .await;
            phase(world).await;
        });
        drop(runtime);
    })
    .join()
    .expect("the crashing process runs its phase before dying");
}

/// Waits until the crashed process's claims on `group_key` have lapsed.
///
/// Expiry is the substrate's clock, not the test's, so the law polls with a
/// probe host rather than sleeping a guessed interval. The probe's resolver is
/// wired but its opener is absent, so a probe pass answers `NoExecutor` and
/// writes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn until_claims_lapse(world: &ToolChildWorld, group_key: &str) {
    let drain = world
        .drain
        .as_ref()
        .expect("the crash scenario runs only where the tier keeps a journal");
    tokio::time::timeout(SETTLE_BUDGET, async {
        loop {
            let report = drain
                .drain_group(group_key, &tokio_util::sync::CancellationToken::new())
                .await
                .expect("a probe pass over the journaled group runs");
            if report.children.iter().all(|child| {
                !matches!(
                    child.outcome,
                    crate::testing::conformance_support::ChildDrainOutcome::LeaseLive { .. }
                )
            }) {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("a dead worker's claims lapse");
}

/// The recovery law: an opener that is not live on this host leaves its child
/// accepted — not failed, not run — and the child completes once the same
/// opener registers on the recovering host (ADR 0099 §1, W1).
///
/// Two shapes of the same statement:
///
/// * On a tier with a durable journal the group outlives the worker that
///   opened it. The crash phase opens it and parks its deferred child, then
///   dies. A successor host whose resolver is wired but whose opener is absent
///   drains the group and reports the child as `NoExecutor` — the typed "not
///   mine", not a failure and not an execution. Registering the same
///   `EffectOpener` on the successor and draining again runs the child to a
///   settlement a reopen serves, and the leaf's body never runs twice: the
///   journaled `Pending` attempt is replayed, the deferred resolver is
///   re-armed, and the out-of-band resolution is what settles it.
/// * On the in-memory tier the process *is* the substrate, so the observable
///   edge is the first open: with the opener unregistered the open is refused
///   before anything is journaled, and the identical group opens and settles
///   once the opener registers.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unregistered_opener_leaves_the_child_accepted(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-recovery"));
    let turn_id = crate::TurnId::from(format!("{prefix}-recovery-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener =
        crate::EffectOpener::for_scope(&scope, None).expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-recovery-group");
    let (process_env_store, env_ref) = crate::testing::process_execution_env_fixture();

    let probe_world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    if probe_world.drain.is_none() {
        // The in-memory tier: no journal survives the process, so the routing
        // gate is observable only at first open — the group is refused before
        // anything is recorded, and the same key opens once the opener is
        // live.
        let host = probe_world.host;
        install_child_host(&host, &process_env_store);
        let scoped = host.scoped(scope.clone()).expect("the group scope binds");
        let group = recovery_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            deferrable_routing(fixture.deferrable_routing, &host),
        );
        let refusal = scoped
            .controller()
            .open_effect_group(group)
            .await
            .expect_err("a child whose opener is not live refuses the first open");
        assert!(
            refusal.to_string().contains("child 0"),
            "the refusal names the child it cannot route: {refusal}"
        );

        let registry =
            Arc::new(crate::TestLocalProcessRegistry::default()) as Arc<dyn crate::ProcessRegistry>;
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let _guard = register_opener(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            registry,
            Arc::clone(&process_env_store),
            opener,
            tokio_util::sync::CancellationToken::new(),
        );
        let mut handle = scoped
            .controller()
            .open_effect_group(recovery_group(
                &scope,
                &session_id,
                &group_key,
                &env_ref,
                deferrable_routing(fixture.deferrable_routing, &host),
            ))
            .await
            .expect("the identical group opens once the opener is live");
        let key = scenario
            .observation
            .parked_key(&format!("{group_key}-call-0"))
            .await;
        host.resolve_await_event(
            &key,
            crate::Resolution::Ok(serde_json::json!({ "leaf": "recovery", "via": "resolver" })),
        )
        .await
        .expect("the parked child's key resolves");
        let settlement = next_settlement(&scoped, &mut handle, 0).await;
        let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &settlement.outcome
        else {
            panic!("the recovered child settles a tool invocation: {settlement:?}")
        };
        assert!(
            format!("{:?}", outcome.record.output).contains("resolver"),
            "the settled output carries the out-of-band resolution"
        );
        assert_eq!(
            scenario.observation.executions_of("law_recovery").len(),
            1,
            "the leaf body ran exactly once"
        );
        return;
    }

    // The durable tiers: the group outlives the worker that opened it.
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
                    intent_target: crate::ProcessId::from("unused-in-recovery"),
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
                    .scoped(scope.clone())
                    .expect("the group scope binds");
                let handle = scoped
                    .controller()
                    .open_effect_group(recovery_group(
                        &scope,
                        &session_id,
                        &group_key,
                        &env_ref,
                        deferrable_routing(routing_kind, &world.host),
                    ))
                    .await
                    .expect("the group opens under the live opener");
                // Wait until the child has parked on its completion key, so
                // what the dead process leaves is a *claimed* unsettled child,
                // not a row that never ran. `park` fires inside the attempt
                // body — one commit before the journaled Pending row lands —
                // so the durable half of the wait is the armed resolver key
                // becoming visible to this host's journal: by then the
                // attempt row the resolver settles is already durable.
                let key = observation.parked_key(&call_id).await;
                await_key_registered(&world.host, &session_id, &key).await;
                // Close the caller's handle the way a finishing turn would:
                // the parked child is a loser under RunToCompletion and stays
                // owned by the host's task until the runtime dies under it.
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
        observation.executions_of("law_recovery").len(),
        1,
        "the crashed worker ran the leaf body exactly once"
    );

    // The successor: resolver wired, opener absent. The drain reports the
    // child as unrunnable on *this* host — the typed "not mine" — once the
    // dead worker's claim has lapsed.
    let successor = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&successor.host, &process_env_store);
    until_claims_lapse(&successor, &group_key).await;
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
            crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
        )),
        "with no live opener the child is reported unrunnable, not run and not failed: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_recovery").len(),
        1,
        "the successor did not re-execute the leaf body"
    );

    // A reopen on the successor tolerates the miss the same way: the group
    // opens, the child is not dispatched, and no settlement is served for its
    // rank while the opener stays absent.
    let scoped = successor
        .host
        .scoped(scope.clone())
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(recovery_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
        ))
        .await
        .expect("a reopen tolerates a child this host cannot run");
    assert!(
        tokio::time::timeout(
            ABSENCE_BUDGET,
            scoped
                .controller()
                .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new()),
        )
        .await
        .is_err(),
        "no settlement is served while the opener is absent: the child is accepted, not run and not failed"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor's observing handle closes");
    drop(scoped);

    // The opener registers on the successor — the same `EffectOpener`, derived
    // from the same scope — and the next drain runs the child. The journaled
    // `Pending` attempt replays (the body is not re-executed), the deferred
    // resolver is re-armed, and the out-of-band resolution settles it.
    let provider: Arc<dyn crate::ToolProvider> = Arc::new(LawLeafProvider {
        definitions: leaf_definitions(),
        observation: Arc::clone(&observation),
        session_id: session_id.clone(),
        intent_target: crate::ProcessId::from("unused-in-recovery"),
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
    let key = observation.parked_key(&call_id).await;
    resolve_when_registered(
        &successor.host,
        key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "recovery", "via": "resolver" })),
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
        "the reclaimed child settles through the successor's drain: {report:?}"
    );
    assert_eq!(
        observation.executions_of("law_recovery").len(),
        1,
        "the journaled Pending attempt replays; the leaf body never runs twice"
    );

    // Journal-visible: a caller reopening the group on the successor is served
    // the rank the drain settled, carrying the out-of-band resolution.
    let scoped = successor
        .host
        .scoped(scope.clone())
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(recovery_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            deferrable_routing(fixture.deferrable_routing, &successor.host),
        ))
        .await
        .expect("the successor reopens the drained group");
    let settlement = next_settlement(&scoped, &mut handle, 0).await;
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation {
        outcome,
        settlement,
    }) = &settlement.outcome
    else {
        panic!("the recovered child settles a tool invocation: {settlement:?}")
    };
    settlement.validate().expect("the settlement validates");
    assert!(
        format!("{:?}", outcome.record.output).contains("resolver"),
        "the settled output carries the out-of-band resolution"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the successor closes");
}

// =============================================================================
// Routing: a live opener that is not the recorded one
// =============================================================================

/// The present-but-foreign half of the routing gate, in both directions
/// (ADR 0099 §1, §3).
///
/// `an_unregistered_opener_leaves_the_child_accepted` proves the absent half:
/// no live opener, no run. This proves the mismatched half — a live opener
/// that is *not* the recorded one still cannot drive the child, whichever way
/// the pair is arranged — because a child's context is reconstructed from its
/// retained request, not lent from whichever opener happens to be registered.
/// The leak this closes is the one where "an opener is live" was authority
/// enough: a reopen under a different opener would have run the child under
/// *that* opener's session, frame and controller.
///
/// On a durable tier the group journals and its children stay accepted while
/// the recorded opener is elsewhere; on the in-memory tier the same gate is
/// observable at open. Either way, when the recorded opener registers the
/// child runs under its *recorded* session — which the leaf reports, so a
/// child that ran under the foreign opener's context instead would be caught
/// rather than merely counted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_foreign_opener_cannot_drive_another_openers_child(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_a = crate::SessionId::from(format!("{prefix}-mismatch-session-a"));
    let session_b = crate::SessionId::from(format!("{prefix}-mismatch-session-b"));
    let scope_a = crate::ExecutionScope::turn(
        session_a.clone(),
        crate::TurnId::from(format!("{prefix}-mismatch-turn-a")),
    );
    let scope_b = crate::ExecutionScope::turn(
        session_b.clone(),
        crate::TurnId::from(format!("{prefix}-mismatch-turn-b")),
    );
    let opener_a =
        crate::EffectOpener::for_scope(&scope_a, None).expect("a turn scope derives an opener");
    let opener_b =
        crate::EffectOpener::for_scope(&scope_b, None).expect("a turn scope derives an opener");
    let group_key_a = format!("{prefix}-mismatch-group-a");
    let group_key_b = format!("{prefix}-mismatch-group-b");
    let (env_store, env_ref) = crate::testing::process_execution_env_fixture();
    let observation = Arc::new(LawObservation::default());
    let registry = (fixture.make_registry)().await;

    let provider = |session_id: &crate::SessionId| -> Arc<dyn crate::ToolProvider> {
        Arc::new(LawLeafProvider {
            definitions: leaf_definitions(),
            observation: Arc::clone(&observation),
            session_id: session_id.clone(),
            intent_target: crate::ProcessId::from("unused-in-mismatch"),
            start_metadata: serde_json::Value::Null,
        })
    };
    let group = |scope: &crate::ExecutionScope, session_id: &crate::SessionId, group_key: &str| {
        single_leaf_group(
            scope,
            session_id,
            group_key,
            &env_ref,
            LEAF_PLAIN,
            ToolChildCompletionRouting::Inline,
        )
    };

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    install_child_host(&world.host, &env_store);

    if world.drain.is_some() {
        // The durable tiers. A first open refuses a child with no runner —
        // only a reopen tolerates one — so the groups are journaled while
        // their own openers are live, parked on their deferred keys, and the
        // worker dies leaving claimed unsettled children.
        for (scope, session_id, group_key, opener) in [
            (&scope_a, &session_a, &group_key_a, &opener_a),
            (&scope_b, &session_b, &group_key_b, &opener_b),
        ] {
            crashed_world(fixture, {
                let scope = scope.clone();
                let session_id = session_id.clone();
                let group_key = group_key.clone();
                let env_store = Arc::clone(&env_store);
                let env_ref = env_ref.clone();
                let observation = Arc::clone(&observation);
                let opener = opener.clone();
                let routing_kind = fixture.deferrable_routing;
                move |world| {
                    Box::pin(async move {
                        let _guard = register_opener(
                            &world.host,
                            &scope,
                            Arc::new(LawLeafProvider {
                                definitions: leaf_definitions(),
                                observation: Arc::clone(&observation),
                                session_id: session_id.clone(),
                                intent_target: crate::ProcessId::from("unused-in-mismatch"),
                                start_metadata: serde_json::Value::Null,
                            }),
                            Arc::new(crate::TestLocalProcessRegistry::default()),
                            env_store,
                            opener,
                            tokio_util::sync::CancellationToken::new(),
                        );
                        let scoped = world
                            .host
                            .scoped(scope.clone())
                            .expect("the group scope binds");
                        let handle = scoped
                            .controller()
                            .open_effect_group(single_leaf_group(
                                &scope,
                                &session_id,
                                &group_key,
                                &env_ref,
                                LEAF_DEFERRED,
                                deferrable_routing(routing_kind, &world.host),
                            ))
                            .await
                            .expect("the group opens under its live opener");
                        // The durable park: the journaled Pending row plus
                        // the armed resolver key, so the dead worker leaves a
                        // claimed unsettled child the successor can inspect.
                        let key = observation.parked_key(&format!("{group_key}-call-0")).await;
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
        }

        let successor = world;
        until_claims_lapse(&successor, &group_key_a).await;
        until_claims_lapse(&successor, &group_key_b).await;
        let drain = successor
            .drain
            .as_ref()
            .expect("a durable tier hands out a drain");

        // Direction one: only the foreign opener B is live. The A child is
        // reported unrunnable — B's live context is not a substitute for the
        // recorded opener.
        let guard_b = register_opener(
            &successor.host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let report = drain
            .drain_group(&group_key_a, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("the drain pass over the A group runs");
        assert!(
            report.children.iter().all(|child| matches!(
                child.outcome,
                crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
            )),
            "a live foreign opener cannot drive the recorded opener's child: {report:?}"
        );
        assert_eq!(
            observation.executions_of("law_deferred").len(),
            2,
            "the foreign opener's drain ran nothing — only the crashed worlds' \
             admissions have run the leaves"
        );

        // Direction two: B steps down, only the foreign opener A is live, and
        // the B child answers the same way.
        drop(guard_b);
        let guard_a = register_opener(
            &successor.host,
            &scope_a,
            provider(&session_a),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_a.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let report = drain
            .drain_group(&group_key_b, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("the drain pass over the B group runs");
        assert!(
            report.children.iter().all(|child| matches!(
                child.outcome,
                crate::testing::conformance_support::ChildDrainOutcome::NoExecutor
            )),
            "the foreign opener fails the other direction the same way: {report:?}"
        );
        assert_eq!(
            observation.executions_of("law_deferred").len(),
            2,
            "no child ran under a foreign opener in either direction"
        );

        // Both recorded openers live: each reclaiming drain replays the
        // journaled Pending attempt — the body never re-runs — and the
        // out-of-band resolutions settle both children.
        let _guard_b = register_opener(
            &successor.host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        for group_key in [group_key_a.as_str(), group_key_b.as_str()] {
            let drained = crate::task::spawn({
                let drain = Arc::clone(drain);
                let group_key = group_key.to_string();
                async move {
                    drain
                        .drain_group(&group_key, &tokio_util::sync::CancellationToken::new())
                        .await
                }
            });
            let key = observation.parked_key(&format!("{group_key}-call-0")).await;
            resolve_when_registered(
                &successor.host,
                key,
                crate::Resolution::Ok(serde_json::json!({ "leaf": "mismatch", "via": "resolver" })),
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
                "the recorded opener's drain settles its own child: {report:?}"
            );
        }
        drop(guard_a);
        let runs = observation.executions_of("law_deferred");
        assert_eq!(
            runs.len(),
            2,
            "each child ran its body exactly once — the journaled Pending replayed, never re-executed"
        );
        assert!(
            runs.iter().any(|run| run.session_id == session_a.as_str()),
            "the A child ran under its recorded session: {runs:?}"
        );
        assert!(
            runs.iter().any(|run| run.session_id == session_b.as_str()),
            "the B child ran under its recorded session: {runs:?}"
        );
        return;
    }

    {
        // The in-memory tier: the gate is the open itself, and a live foreign
        // opener does not satisfy it in either direction.
        let host = world.host;
        let scoped_a = host.scoped(scope_a.clone()).expect("the A scope binds");
        let scoped_b = host.scoped(scope_b.clone()).expect("the B scope binds");
        let guard_b = register_opener(
            &host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        scoped_a
            .controller()
            .open_effect_group(group(&scope_a, &session_a, &group_key_a))
            .await
            .expect_err("a group whose opener is foreign to the live one refuses to open");
        drop(guard_b);
        let guard_a = register_opener(
            &host,
            &scope_a,
            provider(&session_a),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_a.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        scoped_b
            .controller()
            .open_effect_group(group(&scope_b, &session_b, &group_key_b))
            .await
            .expect_err("the foreign direction refuses the same way");

        let _guard_b = register_opener(
            &host,
            &scope_b,
            provider(&session_b),
            Arc::clone(&registry),
            Arc::clone(&env_store),
            opener_b.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let mut handle_a = scoped_a
            .controller()
            .open_effect_group(group(&scope_a, &session_a, &group_key_a))
            .await
            .expect("the A group opens once its opener is live");
        let mut handle_b = scoped_b
            .controller()
            .open_effect_group(group(&scope_b, &session_b, &group_key_b))
            .await
            .expect("the B group opens once its opener is live");
        next_settlement(&scoped_a, &mut handle_a, 0).await;
        next_settlement(&scoped_b, &mut handle_b, 0).await;
        scoped_a
            .controller()
            .close_effect_group(handle_a, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the A group closes");
        scoped_b
            .controller()
            .close_effect_group(handle_b, crate::LoserPolicy::RunToCompletion)
            .await
            .expect("the B group closes");
        drop(guard_a);
    }

    // Each child ran exactly once, under the session its own request
    // recorded — never under the foreign opener's.
    let runs = observation.executions_of("law_plain");
    assert_eq!(runs.len(), 2, "each child ran exactly once");
    assert!(
        runs.iter().any(|run| run.session_id == session_a.as_str()),
        "the A child ran under its recorded session: {runs:?}"
    );
    assert!(
        runs.iter().any(|run| run.session_id == session_b.as_str()),
        "the B child ran under its recorded session: {runs:?}"
    );
}
