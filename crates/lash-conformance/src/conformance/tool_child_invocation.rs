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
//!   `ToolAttempt` — and observes the §3 ruling: a group child holds no
//!   `RuntimeExecutionContext`, so `call_tool_batch` answers the
//!   out-of-process-replay refusal.
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
    pub completion_routing: ToolChildCompletionRouting,
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
    fn record(&self, tool: &str, attempt: u32, execution_binding: serde_json::Value) {
        self.executions.lock_recover().push(LeafExecution {
            tool: tool.to_string(),
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
            attempts: Vec::new(),
        },
    }
}

/// The orchestrating child's body. It exists to prove two §3 facts at once:
/// the child ran through the orchestrating lane — no `ToolAttempt` framed it —
/// and the child holds no `RuntimeExecutionContext`, which is exactly what
/// `call_tool_batch` reports.
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
        let refused = replies
            .iter()
            .all(|reply| !matches!(reply.output.outcome, crate::ToolCallOutcome::Success(_)));
        crate::ToolOutcome::ok(serde_json::json!({
            "leaf": "orchestrating",
            "replies": replies.len(),
            "nested_batch_unavailable": refused,
        }))
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
) -> crate::runtime::effect::LiveOpenerGuard {
    let installed = install_child_host(host, &process_env_store);
    let dispatch = opener_dispatch(host, scope, provider, registry, process_env_store);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let context =
        crate::runtime::effect::LiveOpenerContext::capture_with_event_sender(&dispatch, event_tx)
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
    let children = vec![
        leaf(0, LEAF_PLAIN, ToolChildCompletionRouting::Inline),
        leaf(1, LEAF_RETRY, ToolChildCompletionRouting::Inline),
        leaf(2, LEAF_DEFERRED, routing),
        granted,
        leaf(4, LEAF_INTENTS, ToolChildCompletionRouting::Inline),
        leaf(5, LEAF_USAGE, ToolChildCompletionRouting::Inline),
        leaf(6, LEAF_ORCHESTRATING, ToolChildCompletionRouting::Inline),
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
    );

    let parent = parent_invocation(&scope);
    let group = lane_group(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        &parent,
        fixture.completion_routing,
    );
    let scoped = host.scoped(scope.clone()).expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(group)
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
    for rank in 0..7 {
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
        (0..7).collect::<Vec<_>>(),
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
    assert_eq!(scenario.observation.executions_of("law_plain").len(), 1);

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
    // the leaf provider never saw it — and the child held no runtime execution
    // context, which is the §3 ruling a nested batch reports.
    assert!(
        scenario
            .observation
            .executions_of("law_orchestrating")
            .is_empty(),
        "an orchestrating child never enters the leaf provider"
    );
    let orchestrating = &outcomes[6];
    if !orchestrating.0.record.output.is_success() {
        panic!("the orchestrating leaf settles: {:?}", orchestrating.0)
    }
    let value = orchestrating.0.record.output.value_for_projection();
    assert_eq!(
        value["nested_batch_unavailable"],
        serde_json::json!(true),
        "a group child holds no runtime execution context"
    );
}

// =============================================================================
// Recovery: an opener that is not live on this host
// =============================================================================

/// A single-child group: the recovery leaf alone, parked on its deferred
/// completion key.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn recovery_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
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
            LEAF_RECOVERY,
            LEAF_RECOVERY.trim_start_matches("tool:"),
            catalog_admission(LEAF_RECOVERY),
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
    .expect("the recovery group assembles")
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
            fixture.completion_routing,
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
        );
        let mut handle = scoped
            .controller()
            .open_effect_group(recovery_group(
                &scope,
                &session_id,
                &group_key,
                &env_ref,
                fixture.completion_routing,
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
        let routing = fixture.completion_routing;
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
                        routing,
                    ))
                    .await
                    .expect("the group opens under the live opener");
                // Wait until the child has parked on its completion key, so
                // what the dead process leaves is a *claimed* unsettled child,
                // not a row that never ran.
                let key = observation.parked_key(&call_id).await;
                let _ = key;
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
            fixture.completion_routing,
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
            fixture.completion_routing,
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
