//! FIG-4079 (FIG-4073): the laws that pin a tool call's identity.
//!
//! A tool author keys idempotency on the identity lash hands its attempt
//! (ADR 0042, ADR 0110 §3). These laws state what that identity must do on
//! every tier: stay the same across every re-run of one logical call — a
//! crash replay, a retry after a reported failure, a redrive that reads a
//! recorded outcome back — and differ for every other call, even when the
//! model's provider hands two calls the same call id.
//!
//! Every law drives real turns through the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): a fresh runtime
//! over the tier's host and stores per execution, a scripted model and the
//! probe tools below, which record the identity each attempt saw.
//!
//! What a law reads as "the identity" is [`AttemptIdentity`], taken in one
//! place ([`AttemptIdentity::of`]), so the cutover to a lash-minted call id
//! (FIG-4080) changes one function and no law.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::ToolDefinitionBindingExt as _;
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

mod admission;
mod drift;
mod laws;
mod replay;
pub use admission::*;
pub use drift::*;
pub use laws::*;
pub use replay::*;

/// What a registering tier hands every tool-call identity law.
#[derive(Clone)]
pub struct ToolCallIdentityTier {
    /// Distinguishes this tier's sessions from every other tier's, and every
    /// run's from the last on a tier whose state outlives a run.
    pub prefix: String,
    pub effect_host: Arc<dyn crate::EffectHost>,
    pub stores: Arc<dyn crate::StoreSet>,
    /// Runs each turn, and crashes and redrives it.
    pub runner: Arc<dyn crate::ConformanceTurnRunner>,
    /// The RLM protocol plugin factories the code-cell laws run under: the
    /// part of the tier this crate cannot construct.
    pub rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    /// The RLM protocol with its process lifecycle on, and the process
    /// controls a cell's `processes.start` needs: what the process-admission
    /// law's cells run under.
    pub process_rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
}

/// How long a turn gets before the law fails rather than hangs.
const TURN_BUDGET: Duration = Duration::from_secs(90);

/// How long a law waits for a fact a healthy run reaches in well under a
/// second.
const PATIENCE: Duration = Duration::from_secs(60);

/// How long a settled call's outcome is given to become durable before a law
/// crashes the turn around it.
const SETTLEMENT_GRACE: Duration = Duration::from_millis(500);

/// The probe that answers at once, or holds, or fails its first attempt.
const PROBE: &str = "identity_probe";

/// The probe that parks on its completion key and resolves it itself.
const DEFERRED: &str = "identity_deferred";

/// The probe whose prepare phase seals a fresh payload each time it runs, and
/// whose body waits on the law's gate before its effect.
const DRIFTING: &str = "identity_drifting";

/// The tool-facing identity one attempt saw.
///
/// `call_id` is the key a tool author is told to key idempotency on.
/// Until FIG-4080 it is the optional `tool_call_id()` accessor — the model
/// provider's raw call id — and `replay_key` the optional
/// `lash-tool:{session}:{call_id}:{tool}` key derived from it; FIG-4080 puts
/// the lash-minted `ToolCallId` here and deletes both accessors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptIdentity {
    pub(crate) call_id: Option<String>,
    pub(crate) replay_key: Option<String>,
    pub(crate) attempt: u32,
}

impl AttemptIdentity {
    /// The one place a law reads the identity lash hands an attempt.
    fn of(context: &crate::AttemptContext<'_>) -> Self {
        Self {
            call_id: context.tool_call_id().map(str::to_owned),
            replay_key: context.replay_key().map(str::to_owned),
            attempt: context.attempt_number(),
        }
    }

    /// The idempotency keys a tool author can key on: every one must differ
    /// between two logical calls.
    fn keys(&self) -> [(&'static str, Option<&str>); 2] {
        [
            ("tool_call_id", self.call_id.as_deref()),
            ("replay_key", self.replay_key.as_deref()),
        ]
    }
}

/// One execution of a probe tool's body.
#[derive(Clone, Debug)]
pub(crate) struct Execution {
    /// The call's `label` argument: which logical call the law meant.
    pub(crate) label: String,
    pub(crate) identity: AttemptIdentity,
    /// The payload the call's prepare phase sealed.
    pub(crate) prepared: serde_json::Value,
    /// The completion key a deferred probe parked on.
    pub(crate) completion_key: Option<crate::AwaitEventKey>,
}

/// A gate a held probe waits on until the law opens it.
#[derive(Default)]
struct Gate {
    open: std::sync::atomic::AtomicBool,
    opened: tokio::sync::Notify,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.opened.notify_waiters();
    }

    async fn passed(&self) {
        loop {
            let opened = self.opened.notified();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            opened.await;
        }
    }
}

/// Everything the probes did, across every execution of every turn.
#[derive(Default)]
pub(crate) struct Witness {
    executions: std::sync::Mutex<Vec<Execution>>,
    /// Bodies that started, by label, including those still running.
    started: std::sync::Mutex<Vec<String>>,
    gate: Gate,
    /// How many times the drifting probe's prepare phase ran.
    pub(crate) prepares: AtomicUsize,
}

impl Witness {
    pub(crate) fn executions(&self) -> Vec<Execution> {
        self.executions.lock_recover().clone()
    }

    pub(crate) fn of(&self, label: &str) -> Vec<Execution> {
        self.executions()
            .into_iter()
            .filter(|execution| execution.label == label)
            .collect()
    }

    pub(crate) fn started(&self, label: &str) -> usize {
        self.started
            .lock_recover()
            .iter()
            .filter(|started| *started == label)
            .count()
    }

    pub(crate) fn open_gate(&self) {
        self.gate.open();
    }

    fn record(&self, execution: Execution) {
        self.executions.lock_recover().push(execution);
    }

    /// Waits until `ready` holds, or fails the law.
    pub(crate) async fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        tokio::time::timeout(PATIENCE, async {
            while !ready(self) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} within {PATIENCE:?}"));
    }
}

/// A probe call's arguments.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProbeArgs {
    pub(crate) label: String,
    /// The body records its identity, then waits on the law's gate: the
    /// effect happened and its outcome is not recorded until the gate opens.
    #[serde(default)]
    pub(crate) hold: bool,
    /// The first attempt reports a retryable failure after its effect.
    #[serde(default)]
    pub(crate) fail_first: bool,
    /// The call's result switches the turn to a follow-on agent frame whose
    /// task is [`follow_on_task`] of the label.
    #[serde(default)]
    pub(crate) switch: bool,
}

/// The task of the follow-on frame a switching probe opens.
pub(crate) fn follow_on_task(label: &str) -> String {
    format!("tool-call identity follow-on frame of {label}")
}

impl ProbeArgs {
    pub(crate) fn label(label: &str) -> Self {
        Self {
            label: label.to_string(),
            ..Self::default()
        }
    }

    pub(crate) fn held(label: &str) -> Self {
        Self {
            hold: true,
            ..Self::label(label)
        }
    }

    pub(crate) fn failing_first(label: &str) -> Self {
        Self {
            fail_first: true,
            ..Self::label(label)
        }
    }
}

fn probe_definition(name: &str) -> crate::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Records the identity its attempt saw and answers with its label.",
        object.clone(),
        object,
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], name))
    .with_retry_policy(crate::ToolRetryPolicy::safe(3, 1, 1))
}

/// The probe tools: [`PROBE`] and [`DEFERRED`].
struct IdentityProbes {
    witness: Arc<Witness>,
    effect_host: Arc<dyn crate::EffectHost>,
}

impl IdentityProbes {
    async fn deferred(
        &self,
        call: &crate::ToolCall<'_>,
        args: ProbeArgs,
        identity: AttemptIdentity,
    ) -> crate::ToolAttemptOutcome {
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(error) => {
                return crate::ToolOutcome::err_fmt(format!(
                    "the deferred probe takes no completion key on this tier: {error}"
                ))
                .into();
            }
        };
        self.witness.record(Execution {
            label: args.label.clone(),
            identity,
            prepared: call.context.prepared_payload().clone(),
            completion_key: Some(key.clone()),
        });
        // The call resolves its own key with its own label: a call that reads
        // another label consumed another call's completion.
        let resolver = Arc::clone(&self.effect_host);
        crate::task::spawn(async move {
            let _ = resolver
                .await_event_resolver()
                .resolve_await_event(
                    &key,
                    crate::Resolution::Ok(serde_json::json!({ "label": args.label })),
                )
                .await;
        });
        crate::ToolAttemptOutcome::Pending(crate::PendingCompletion::new())
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for IdentityProbes {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        [PROBE, DEFERRED, DRIFTING]
            .into_iter()
            .map(|name| probe_definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        [PROBE, DEFERRED, DRIFTING]
            .contains(&name)
            .then(|| Arc::new(probe_definition(name).contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == probe_definition(DEFERRED).id()
    }

    async fn prepare_tool_call(
        &self,
        call: crate::ToolPrepareCall<'_>,
    ) -> Result<crate::PreparedToolCall, crate::ToolOutcome> {
        let drifting = call.tool_id == *probe_definition(DRIFTING).id();
        let mut prepared = crate::PreparedToolCall::identity(call.tool_id, call.pending);
        if drifting {
            let seal = self.witness.prepares.fetch_add(1, Ordering::SeqCst) + 1;
            prepared.prepared_payload = serde_json::json!({ "seal": seal });
        }
        Ok(prepared)
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let args = match serde_json::from_value::<ProbeArgs>(call.args.clone()) {
            Ok(args) => args,
            Err(error) => return crate::ToolOutcome::err_fmt(error).into(),
        };
        let identity = AttemptIdentity::of(call.context);
        self.witness.started.lock_recover().push(args.label.clone());
        if call.name() == DEFERRED {
            return self.deferred(&call, args, identity).await;
        }
        if call.name() == DRIFTING {
            // Nothing has happened yet: the gate holds the call between its
            // admission and its effect.
            self.witness.gate.passed().await;
        }
        // The recorded execution is the probe's effect; a held probe's gate
        // then holds its outcome back from the journal.
        self.witness.record(Execution {
            label: args.label.clone(),
            identity: identity.clone(),
            prepared: call.context.prepared_payload().clone(),
            completion_key: None,
        });
        if args.hold {
            self.witness.gate.passed().await;
        }
        if args.fail_first && identity.attempt == 1 {
            return crate::ToolOutcome::failure(crate::ToolFailure::safe_retry(
                crate::ToolFailureClass::External,
                "identity_probe_timeout",
                "the probe's effect happened and its first attempt reported a timeout",
                Some(1),
            ))
            .into();
        }
        let output = crate::ToolCallOutput::success(serde_json::json!({ "label": args.label }));
        if !args.switch {
            return crate::ToolAttemptOutcome::done_without_intents(
                crate::ToolOutcomeDone::from_output(output),
            );
        }
        let Ok(frame_key) = crate::FrameKey::from_caller_material(&args.label) else {
            return crate::ToolOutcome::err_fmt("a probe's label is frame material").into();
        };
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            output.with_control(crate::ToolControl::SwitchAgentFrame {
                frame_key,
                initial_nodes: Vec::new(),
                task: Some(follow_on_task(&args.label)),
            }),
        ))
    }
}

/// Compacts a session to one fixed summary: a compaction without a model
/// call.
struct FixedCompactor;

#[async_trait::async_trait]
impl crate::facade_support::ContextCompactor for FixedCompactor {
    fn id(&self) -> &'static str {
        "conformance.tool_call_identity.compactor"
    }

    async fn compact(
        &self,
        _ctx: &crate::facade_support::CompactionContext<'_>,
    ) -> Result<Option<crate::facade_support::ContextCompaction>, crate::facade_support::ContextError>
    {
        Ok(Some(crate::facade_support::ContextCompaction::new(vec![
            lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, SUMMARY)
                    .with_origin(lash_core::MessageOrigin::Plugin {
                        plugin_id: "conformance_tool_call_identity".to_string(),
                        transient: false,
                    }),
            ),
        ])))
    }
}

/// A model response calling `calls`, each `(provider call id, tool, args)`.
pub(crate) fn calls(calls: &[(&str, &str, ProbeArgs)]) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: calls
            .iter()
            .map(|(call_id, tool, args)| crate::LlmOutputPart::ToolCall {
                call_id: (*call_id).to_string(),
                tool_name: (*tool).to_string(),
                input_json: serde_json::to_string(args).unwrap_or_default(),
                replay: None,
            })
            .collect(),
        ..crate::LlmResponse::default()
    }
}

/// A model response calling one tool with arguments that are not a probe's.
pub(crate) fn raw_call(
    call_id: &str,
    tool: &str,
    input: serde_json::Value,
) -> crate::LlmOutputPart {
    crate::LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool.to_string(),
        input_json: input.to_string(),
        replay: None,
    }
}

/// A model response that is one code cell.
pub(crate) fn cell(source: &str) -> crate::LlmResponse {
    text(&format!("<typescript>\n{source}\n</typescript>"))
}

pub(crate) fn text(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        ..crate::LlmResponse::default()
    }
}

/// One law's world: a session on the tier, the probes' witness and the
/// model's call count.
#[derive(Clone)]
pub(crate) struct World {
    tier: ToolCallIdentityTier,
    pub(crate) session_id: SessionId,
    pub(crate) witness: Arc<Witness>,
    pub(crate) model_calls: Arc<AtomicUsize>,
    /// What the model answers in each segment it is asked in: the text that
    /// opens the segment (a turn's input, a follow-on frame's task) and the
    /// answers, in order.
    scripts: Arc<std::sync::Mutex<Vec<(String, Arc<Vec<crate::LlmResponse>>)>>>,
    protocol: Protocol,
    /// The process registry the session starts processes in.
    process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
    /// The process registry and work the session starts processes on.
    processes: Option<WorldProcesses>,
}

/// Which protocol a law's session runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    /// The standard protocol: the model calls tools natively.
    Standard,
    /// RLM: the model answers with code cells.
    Code,
    /// RLM whose cells can start Lashlang processes.
    CodeWithProcesses,
}

/// A world's process registry and the work wiring the tier's engine runs its
/// processes' segments with.
#[derive(Clone)]
struct WorldProcesses {
    registry: Arc<dyn crate::ProcessRegistry>,
    wiring: crate::ProcessWorkWiring,
}

/// What the model answers in a segment no script opens: a compaction's
/// summary request.
const SUMMARY: &str = "tool-call identity law summary";

/// One turn of a law's session: its id, what the user says, and what the
/// model answers, in order. A request is answered by how many answers its
/// segment already holds, so a replay that asks again is answered as the
/// first execution was.
#[derive(Clone)]
pub(crate) struct ScriptedTurn {
    pub(crate) turn_id: TurnId,
    pub(crate) input: String,
}

impl World {
    pub(crate) fn new(tier: &ToolCallIdentityTier, law: &str) -> Self {
        Self {
            tier: tier.clone(),
            session_id: SessionId::from(format!("{}-{law}", tier.prefix)),
            witness: Arc::new(Witness::default()),
            model_calls: Arc::new(AtomicUsize::new(0)),
            scripts: Arc::default(),
            protocol: Protocol::Standard,
            process_registry: None,
            processes: None,
        }
    }

    /// A world whose session runs under the tier's RLM protocol: the model
    /// answers with code cells.
    pub(crate) fn code(tier: &ToolCallIdentityTier, law: &str) -> Self {
        Self {
            protocol: Protocol::Code,
            ..Self::new(tier, law)
        }
    }

    /// A world whose code cells start Lashlang processes, whose segments the
    /// tier's engine runs on a worker over the same plugins.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the worker is built from the setup above"
    )]
    pub(crate) fn code_with_processes(tier: &ToolCallIdentityTier, law: &str) -> Self {
        let registry = tier.stores.process_registry();
        let mut world = Self {
            protocol: Protocol::CodeWithProcesses,
            process_registry: Some(Arc::clone(&registry)),
            ..Self::new(tier, law)
        };
        let (config, factories) = world.host_and_factories();
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(world.session_id.clone());
        // One watch, two consumers: the runtime's process port and the
        // worker observe the same registry handle.
        let watched = crate::facade_support::watch_process_registry(registry);
        let worker = lash_core_worker::DurableProcessWorker::new(
            lash_core_worker::DurableProcessWorkerConfig::new(
                Arc::new(crate::facade_support::PluginHost::new(factories)),
                config,
                crate::ProcessWorkWiring::new(
                    watched.clone(),
                    Arc::new(crate::NoProcessWork::new(&watched)),
                ),
                Arc::new(crate::NoSessionWork::new()),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_policy(policy),
        )
        .expect("build the tool-call identity process worker");
        world.processes = Some(WorldProcesses {
            registry: Arc::clone(watched.registry()),
            wiring: tier.runner.process_work(watched, worker),
        });
        world
    }

    /// The model answers the segment `opening` opens with `responses`.
    pub(crate) fn script(&self, opening: &str, responses: Vec<crate::LlmResponse>) {
        self.scripts
            .lock_recover()
            .push((opening.to_string(), Arc::new(responses)));
    }

    pub(crate) fn runner(&self) -> &Arc<dyn crate::ConformanceTurnRunner> {
        &self.tier.runner
    }

    pub(crate) fn turn(&self, name: &str, responses: Vec<crate::LlmResponse>) -> ScriptedTurn {
        let input = format!("tool-call identity law: {name}");
        self.script(&input, responses);
        ScriptedTurn {
            turn_id: TurnId::from(format!("{}-{name}", self.session_id)),
            input,
        }
    }

    pub(crate) fn admitted(&self, turn: &ScriptedTurn) -> crate::AdmittedScope {
        crate::admit(crate::ExecutionScope::turn(&self.session_id, &turn.turn_id))
    }

    /// The one model of the law's world. A request belongs to the segment
    /// its last user-segment message opens, and is answered by how many
    /// answers follow that message; a segment no script opens is a
    /// compaction's summary request.
    fn model(&self) -> crate::testing::TestProvider {
        let scripts = Arc::clone(&self.scripts);
        let model_calls = Arc::clone(&self.model_calls);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                model_calls.fetch_add(1, Ordering::SeqCst);
                let opened = request
                    .messages
                    .iter()
                    .rposition(|message| message.starts_user_segment);
                let script = opened.and_then(|opened| {
                    let opening = message_text(&request.messages[opened]);
                    scripts
                        .lock_recover()
                        .iter()
                        .find(|(text, _)| opening.contains(text.as_str()))
                        .map(|(_, responses)| Arc::clone(responses))
                });
                let response = match (script, opened) {
                    (Some(responses), Some(opened)) => {
                        let answered = request.messages[opened + 1..]
                            .iter()
                            .filter(|message| {
                                matches!(message.role, lash_sansio::llm::types::LlmRole::Assistant)
                            })
                            .count();
                        responses[answered.min(responses.len() - 1)].clone()
                    }
                    _ => text(SUMMARY),
                };
                async move { Ok(response) }
            })
            .build()
    }

    /// One execution of `turn`: a fresh runtime over the tier's host and
    /// stores, loading the session the earlier turns committed, driving the
    /// turn on the controller the tier lends it.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(crate) async fn drive(
        &self,
        turn: &ScriptedTurn,
        scope: crate::ScopedEffectController<'_>,
        phase_probe: Option<Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>>,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        let mut runtime = self.runtime(phase_probe).await;
        let mut input = crate::TurnInput::text(turn.input.clone());
        input.trace_turn_id = Some(turn.turn_id.clone());
        tokio::time::timeout(
            TURN_BUDGET,
            runtime.drive_turn(
                input,
                crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
            ),
        )
        .await
        .expect("the tool-call identity turn settles within its budget")
    }

    /// The session's store on the tier.
    pub(crate) async fn store(&self) -> Arc<dyn crate::RuntimePersistence> {
        crate::conformance::law_session_store(self.tier.stores.as_ref(), &self.session_id).await
    }

    /// A fresh runtime over the tier's host and stores, loading the session
    /// the earlier executions committed.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(crate) async fn runtime(
        &self,
        phase_probe: Option<Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>>,
    ) -> crate::LashRuntime {
        let (config, factories) = self.host_and_factories();
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        let mut builder =
            crate::LashRuntime::builder(config, crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_plugin_host(crate::facade_support::PluginHost::new(factories))
                .with_store(self.store().await)
                .with_queued_work(Arc::new(crate::NoSessionWork::new()));
        if let Some(processes) = &self.processes {
            builder = builder
                .with_process_registry(Arc::clone(&processes.registry))
                .with_process_work(processes.wiring.clone());
        }
        let mut runtime = Box::pin(builder.build())
            .await
            .expect("build the tool-call identity runtime");
        if let Some(probe) = phase_probe {
            runtime.set_turn_phase_probe(probe);
        }
        runtime
    }

    /// The runtime host config over the tier, answering with the law's
    /// model, and the plugins the session runs: its protocol and the probes.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the tier's plugins declare installable engines"
    )]
    fn host_and_factories(
        &self,
    ) -> (
        crate::RuntimeHostConfig,
        Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    ) {
        let mut law_backend = crate::LawBackend::over_stores(
            Arc::clone(&self.tier.stores),
            Arc::clone(&self.tier.effect_host),
        );
        if let Some(registry) = &self.process_registry {
            law_backend = law_backend.with_process_registry(Arc::clone(registry));
        }
        let mut config = law_backend.host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
            self.model().into_handle(),
        ));
        let probes: Arc<dyn crate::ToolProvider> = Arc::new(IdentityProbes {
            witness: Arc::clone(&self.witness),
            effect_host: Arc::clone(&self.tier.effect_host),
        });
        let protocol = match self.protocol {
            Protocol::Standard => crate::testing::test_standard_protocol_factories(),
            Protocol::Code => self.tier.rlm.clone(),
            Protocol::CodeWithProcesses => self.tier.process_rlm.clone(),
        };
        let factories = protocol
            .into_iter()
            .chain([Arc::new(crate::plugin::StaticPluginFactory::new(
                "conformance-tool-call-identity",
                crate::facade_support::PluginSpec::new()
                    .with_tool_provider(probes)
                    .with_context_compactor(100, Arc::new(FixedCompactor)),
            ))
                as Arc<dyn crate::facade_support::PluginFactory>])
            .collect::<Vec<_>>();
        if self.process_registry.is_some() {
            config = crate::facade_support::PluginHost::new(factories.clone())
                .install_process_engine_contributions(config, true)
                .expect("install the protocol's process-engine contributions");
        }
        (config, factories)
    }

    /// An attempt that drives `turn` and hands its result to `report`.
    pub(crate) fn attempt(
        &self,
        turn: &ScriptedTurn,
        report: tokio::sync::mpsc::UnboundedSender<
            Result<crate::AssembledTurn, crate::RuntimeError>,
        >,
    ) -> crate::ConformanceTurnAttempt {
        let world = self.clone();
        let turn = turn.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let turn = turn.clone();
            let report = report.clone();
            Box::pin(async move {
                let driven = world.drive(&turn, scope, None).await;
                let end = crate::ConformanceTurnEnd::of(&driven);
                let _ = report.send(driven);
                end
            })
        })
    }

    /// Runs `turn` once on the tier and returns what it assembled; the tier
    /// then sheds the scenario's weight.
    pub(crate) async fn run(&self, turn: &ScriptedTurn) -> crate::AssembledTurn {
        let assembled = self.run_kept(turn).await;
        self.runner().scenario_finished().await;
        assembled
    }

    /// [`Self::run`], keeping what the tier journaled for the law to read.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the tier's runner runs the attempt it is handed"
    )]
    pub(crate) async fn run_kept(&self, turn: &ScriptedTurn) -> crate::AssembledTurn {
        let (report, mut reported) = tokio::sync::mpsc::unbounded_channel();
        self.runner()
            .run_turn(self.admitted(turn), self.attempt(turn, report))
            .await;
        reported
            .recv()
            .await
            .expect("the tier's runner ran the turn")
            .unwrap_or_else(|error| panic!("the law's turn `{}` runs: {error}", turn.turn_id))
    }
}

/// The text blocks of `message`, joined.
fn message_text(message: &lash_sansio::llm::types::LlmMessage) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            lash_sansio::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A turn's settled tool calls: each call's recorded output, by provider call
/// id and tool, in transcript order.
pub(crate) fn outputs(
    turn: &crate::AssembledTurn,
) -> Vec<(Option<String>, String, serde_json::Value)> {
    turn.tool_calls
        .iter()
        .map(|record| {
            (
                record.call_id.clone(),
                record.tool.clone(),
                record.output.value_for_projection(),
            )
        })
        .collect()
}

/// The turn finished.
pub(crate) fn assert_finished(what: &str, turn: &crate::AssembledTurn) {
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "{what}: the turn finishes: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
}

/// Runs a law whose contract holds only once `ticket` lands: its failure is
/// the expected divergence and is printed, and a pass fails, so the hold is
/// removed the moment the law holds.
pub async fn run_held_law(
    law: &'static str,
    ticket: &'static str,
    body: impl std::future::Future<Output = ()> + Send,
) {
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body)).await;
    match outcome {
        Ok(()) => panic!("{law} now holds: remove its hold on {ticket}"),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_string()))
                .unwrap_or_else(|| "non-string panic".to_string());
            eprintln!("HELD until {ticket}: {law}: {message}");
        }
    }
}
