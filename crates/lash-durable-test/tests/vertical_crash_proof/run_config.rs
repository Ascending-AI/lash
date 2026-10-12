//! RUN-CONFIG-ONCE (FIG-6021): a run resolves its shape once, before its
//! first phase commits, and every phase checkpoint records it. A takeover, a
//! lost acknowledgement and a cancel read the record and never run the
//! run's configuration producers again, however the deployment drifted; an
//! uncommitted resolution is only derived again. A frame switch hands its
//! follow-on the running turn's recorded spec, or exactly the spec it
//! states, a resumed turn's switch included.
//!
//! The recorded config rides each phase commit, so a zombie's late write of
//! it is a phase commit the epoch fence refuses: the vertical matrix's P5
//! covers that fence at every labelled write.

use super::*;

/// The model a drifted deployment's definition overrides to.
const NEXT_MODEL: &str = "takeover-model-y";

/// The run definition every law's spec names; each deployment registers its
/// own revision-1 answer for it.
fn definition_ref() -> lash::DefinitionRef {
    lash::DefinitionRef::new("takeover-config-law", 1)
}

/// A run definition that records every snapshot it resolves and answers
/// `model` as the run's model override, `None` for the snapshot's.
#[derive(Clone, Default)]
struct CountingDefinition {
    seen: Arc<Mutex<Vec<lash_core::PersistedSessionConfig>>>,
    model: Option<&'static str>,
}

impl CountingDefinition {
    /// The drifted deployment's answer for the same revision.
    fn drifted() -> Self {
        Self {
            model: Some(NEXT_MODEL),
            ..Self::default()
        }
    }

    fn resolutions(&self) -> usize {
        self.seen.lock_recover().len()
    }
}

impl lash::RunDefinition for CountingDefinition {
    fn reference(&self) -> lash::DefinitionRef {
        definition_ref()
    }

    fn resolve(
        &self,
        snapshot: &lash_core::PersistedSessionConfig,
        _context: &serde_json::Value,
    ) -> Result<lash::RunOverrides, lash::RunDefinitionRefusal> {
        self.seen.lock_recover().push(snapshot.clone());
        Ok(lash::RunOverrides {
            model: self.model.map(lash_core::LlmProfileKey::new),
            ..lash::RunOverrides::default()
        })
    }
}

/// The host plugin's id: its config namespace and its run options' owner.
const HOST: &str = "run_config_host";

/// The key a send states the host's run options under.
const HOST_OPTIONS: lash::RunOptionsKey<HostRunOptions> = lash::RunOptionsKey::new(HOST);

/// What the host's prompt section renders opens with.
const HOST_SECTION_OPEN: &str = "[host ";

/// The host's recorded namespace: its standing audience, which a config
/// command sets, and the facts of the current invocation, which only a run
/// states.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
struct HostPrompt {
    #[serde(default)]
    audience: String,
    #[serde(default)]
    facts: String,
}

/// What a run states for the host: its invocation's facts. The audience is
/// no field of it.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
struct HostRunOptions {
    facts: String,
}

/// The host's standing edit.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
struct SetAudience {
    audience: String,
}

impl lash::plugins::ConfigCommand for SetAudience {
    type Owner = HostOwner;
    type Output = ();
    const NAME: &'static str = "set_audience";
}

/// The host's config owner. A drifted build applies a run's facts
/// differently, which a recorded run must never meet.
struct HostOwner {
    drifted: bool,
}

impl lash::plugins::ConfigOwner for HostOwner {
    type Create = HostPrompt;
    type Recorded = HostPrompt;
    type Refusal = String;
    type RunOptions = HostRunOptions;

    fn create(&self, input: Option<HostPrompt>) -> Result<Option<HostPrompt>, String> {
        Ok(Some(input.unwrap_or_default()))
    }

    fn validate(
        &self,
        _value: &HostPrompt,
        _base: Option<&HostPrompt>,
        _facts: &lash::plugins::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    /// A run's facts replace the namespace's; the standing audience stays.
    fn apply_run_options(
        &self,
        recorded: &HostPrompt,
        options: HostRunOptions,
    ) -> Result<HostPrompt, String> {
        let facts = if self.drifted {
            format!("{} (drifted)", options.facts)
        } else {
            options.facts
        };
        Ok(HostPrompt {
            facts,
            ..recorded.clone()
        })
    }
}

/// The host's plugin: its owner, and one prompt section that renders the
/// run's effective namespace, never a value the factory captured.
#[derive(Clone, Copy, Default)]
struct HostPluginFactory {
    drifted: bool,
}

impl lash::plugins::PluginFactory for HostPluginFactory {
    fn id(&self) -> &'static str {
        HOST
    }

    fn register_config(
        &self,
        registrar: &mut lash::plugins::ConfigRegistrar,
    ) -> Result<(), lash::plugins::ConfigRegistrationError> {
        registrar.owner_with_run_options(
            HostOwner {
                drifted: self.drifted,
            },
            HOST_OPTIONS,
        )?;
        registrar.command::<SetAudience>(|recorded, command| {
            Ok(lash::plugins::OwnerChange {
                recorded: HostPrompt {
                    audience: command.audience,
                    ..recorded.clone()
                },
                output: (),
            })
        })
    }

    fn build(
        &self,
        _ctx: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(HostPlugin))
    }
}

impl lash::plugins::PluginDefinition for HostPluginFactory {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(HOST)
    }
}

struct HostPlugin;

impl lash::plugins::SessionPlugin for HostPlugin {
    fn id(&self) -> &'static str {
        HOST
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        reg.prompt().section(
            lash::plugins::PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new("host-facts")
                    .map_err(|error| lash::plugins::PluginError::Registration(error.to_string()))?,
                lash::prompt::PromptPlacement::InitialInstructions,
            ),
            Arc::new(|input: &lash::plugins::PromptInput<'_>| {
                let prompt = input
                    .config::<HostPrompt>()
                    .map_err(|error| lash::plugins::PromptRenderError::new(error.to_string()))?
                    .unwrap_or_default();
                Ok(lash::plugins::SectionText::text(format!(
                    "{HOST_SECTION_OPEN}audience={} facts={}]",
                    prompt.audience, prompt.facts
                )))
            }),
        )
    }
}

/// How a deployment's model answers a request.
type Answer = Arc<dyn Fn(&LlmRequest, usize) -> LlmResponse + Send + Sync>;

/// One model request, as a law reads it back.
struct Seen {
    model: String,
    tools: Vec<String>,
    /// The request's instructions and messages, rendered.
    prompt: String,
}

/// What a law's deployments saw, across every node.
#[derive(Default)]
struct Observed {
    /// Every model request, in call order.
    requests: Mutex<Vec<Seen>>,
    /// The spec hash each `switch_frame` and `probe` call read from its
    /// turn, in call order, by tool.
    specs: Mutex<Vec<(&'static str, Option<lash_core::RunSpecHash>)>>,
}

impl Observed {
    fn models(&self) -> Vec<String> {
        self.requests
            .lock_recover()
            .iter()
            .map(|seen| seen.model.clone())
            .collect()
    }

    /// The host section each request rendered.
    fn host_sections(&self) -> Vec<String> {
        self.requests
            .lock_recover()
            .iter()
            .map(|seen| {
                let start = seen
                    .prompt
                    .find(HOST_SECTION_OPEN)
                    .unwrap_or_else(|| panic!("no host section in {}", seen.prompt));
                let rest = &seen.prompt[start + HOST_SECTION_OPEN.len()..];
                rest[..rest.find(']').expect("the section closes")].to_owned()
            })
            .collect()
    }

    fn offered(&self) -> Vec<Vec<String>> {
        self.requests
            .lock_recover()
            .iter()
            .map(|seen| {
                let mut tools = seen.tools.clone();
                tools.sort();
                tools
            })
            .collect()
    }
}

fn model_metadata(key: &str) -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(key)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// The tool-round answer: call `ext_write` once, then finish in prose.
fn write_then_finish() -> Answer {
    Arc::new(|request, _| {
        if request
            .messages
            .iter()
            .any(|message| message.role == lash_core::llm::types::LlmRole::Assistant)
        {
            text(request, "finished after takeover")
        } else {
            call(TOOL, r#"{"x":7}"#)
        }
    })
}

fn call(tool: &str, input: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: format!("{tool}-call"),
            tool_name: tool.to_owned(),
            input_json: input.to_owned(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

/// One deployment over `backend`: `definition` registered when given, only
/// the `models` keys served, and the host plugin built as `host` builds it.
struct Deployment<'a> {
    backend: &'a Backend,
    definition: Option<CountingDefinition>,
    host: HostPluginFactory,
    models: &'a [&'static str],
    answer: Answer,
    observed: &'a Arc<Observed>,
    tools: Vec<Arc<dyn lash_core::ToolProvider>>,
    boot: &'static str,
}

impl Deployment<'_> {
    fn core(self) -> lash::LashCore {
        let observed = Arc::clone(self.observed);
        let answer = self.answer;
        let model = lash_core::testing::TestProvider::builder()
            .kind("run-config-scripted")
            .requires_streaming(true)
            .complete(move |request: LlmRequest| {
                let observed = Arc::clone(&observed);
                let answer = Arc::clone(&answer);
                async move {
                    let index = {
                        let mut requests = observed.requests.lock_recover();
                        requests.push(Seen {
                            model: request.model.key().to_string(),
                            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
                            prompt: format!("{:?} {:?}", request.instructions, request.messages),
                        });
                        requests.len()
                    };
                    Ok(answer(&request, index))
                }
            })
            .build()
            .into_handle();
        let mut models = lash::LlmProfileRegistry::new();
        for key in self.models {
            models = models
                .register(
                    *key,
                    lash::RegisteredLlmProfile::new(model_metadata(key), model.clone()),
                )
                .expect("the model registers");
        }
        let mut builder = lash::LashCore::standard_builder(self.backend.clone())
            .serve_sessions(false)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .llm_profiles(Arc::new(models))
            .plugin(Arc::new(self.host));
        if let Some(definition) = self.definition {
            builder = builder.run_definition(definition);
        }
        for tools in self.tools {
            builder = builder.tools(tools);
        }
        builder
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("run-config-deployment"),
                lash::persistence::LeaseIncarnationId::new(self.boot),
            ))
            .expect("the core builds")
    }
}

/// The simulated nodes that run `core`'s sessions under `script`.
fn nodes(
    database: &Arc<dyn DurableStore>,
    clock: &Arc<SimClock>,
    backend: &Backend,
    core: &lash::LashCore,
    script: Script,
) -> SimNodes {
    SimNodes::new(
        Arc::clone(database),
        Arc::clone(clock),
        script,
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: Vec::new(),
            max_active: 1,
        },
        Arc::new(SessionActivation::new(
            backend.clone(),
            lash::testing::session_turn_services(core),
            Arc::new(Tripwire::default()),
        )),
    )
}

/// Step `nodes` until `done`, under a virtual watchdog.
async fn step_until<F, Fut>(nodes: &SimNodes, clock: &SimClock, what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let horizon = clock.logical_ms() + 600_000;
    while !done().await {
        assert!(
            clock.logical_ms() < horizon,
            "{what} exceeded its virtual watchdog:\n{}",
            nodes.script().rendered_trace()
        );
        let stepped = tokio::time::timeout(std::time::Duration::from_secs(30), nodes.step())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{what} hung in a step:\n{}",
                    nodes.script().rendered_trace()
                )
            });
        assert!(
            stepped.is_some(),
            "{what} stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
}

async fn session_idle(database: &Arc<dyn DurableStore>) -> bool {
    database
        .actor(&actor())
        .await
        .unwrap()
        .is_some_and(|row| row.state == ActorState::Idle)
}

fn root_spec() -> lash::SessionSpec {
    lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    )
    .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
}

/// One law's store, clock and backend: a deployment creates the session and
/// sends its one input, and nodes run it.
struct Harness {
    clock: Arc<SimClock>,
    database: Arc<dyn DurableStore>,
    backend: Backend,
}

impl Harness {
    async fn new() -> Self {
        let clock = SimClock::new();
        let stores = Arc::new(sim::memory(Arc::clone(&clock)).await);
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        Self {
            clock,
            database,
            backend: shipped_backend(stores),
        }
    }

    async fn send(&self, core: &lash::LashCore, input: &str, spec: lash::RunSpec) {
        let session = core
            .session(session())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                root_spec(),
            ))
            .await
            .expect("the session starts on the session model");
        session
            .send(lash::TurnInput::text(input))
            .id(run())
            .run(spec)
            .await
            .expect("the turn is accepted");
    }

    /// Run `core`'s nodes until `script`'s cut fires, then kill the node.
    async fn cut(&self, core: &lash::LashCore, script: Script) {
        let before = nodes(&self.database, &self.clock, &self.backend, core, script);
        before.start("a");
        before.quiesce().await;
        let horizon = self.clock.logical_ms() + 600_000;
        while before.script().cuts().is_empty() {
            assert!(
                self.clock.logical_ms() < horizon,
                "the cut never fired within its virtual watchdog:\n{}",
                before.script().rendered_trace()
            );
            let stepped = tokio::time::timeout(std::time::Duration::from_secs(30), before.step())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "the cut hung in a step:\n{}",
                        before.script().rendered_trace()
                    )
                });
            assert!(
                stepped.is_some(),
                "the cut never fired:\n{}",
                before.script().rendered_trace()
            );
        }
        before.kill("a");
        before.quiesce().await;
        assert_eq!(before.life("a"), Life::Dead);
    }
}

/// The facts the takeover laws' run states for the host.
const TAKEOVER_FACTS: &str = "takeover facts";

/// What one takeover law observed.
struct Takeover {
    first: CountingDefinition,
    cold: CountingDefinition,
    observed: Arc<Observed>,
    status: lash::TurnStatus,
}

/// Send one input under the counting definition and the host's run options
/// on deployment A, cut its first `model.start` under `fault`, and finish
/// the run on a drifted deployment B, whose definition answers another
/// model and whose host owner applies the options differently. A fault that
/// leaves node A alive finishes the run there instead.
async fn takeover(fault: Fault) -> Takeover {
    let scenario = Harness::new().await;
    let observed = Arc::new(Observed::default());
    let world = Arc::new(ExternalWorld::default());
    let first = CountingDefinition::default();
    let cold = CountingDefinition::drifted();
    let core_a = Deployment {
        backend: &scenario.backend,
        definition: Some(first.clone()),
        host: HostPluginFactory::default(),
        models: &[MODEL, NEXT_MODEL],
        answer: write_then_finish(),
        observed: &observed,
        tools: vec![ext_write(Arc::clone(&world))],
        boot: "first",
    }
    .core();
    let mut spec = lash::RunSpec::definition(definition_ref(), serde_json::Value::Null);
    spec.overrides
        .owners
        .set(
            HOST_OPTIONS,
            HostRunOptions {
                facts: TAKEOVER_FACTS.to_owned(),
            },
        )
        .expect("the host's options encode");
    scenario.send(&core_a, "write x, then finish", spec).await;
    let script = Script::new();
    script.cut_on("a", CommitLabel::MODEL_START, 1, fault);
    let finishing = if matches!(fault, Fault::AckHidden) {
        // The node lives: it reloads its turn and finishes it itself.
        let nodes = nodes(
            &scenario.database,
            &scenario.clock,
            &scenario.backend,
            &core_a,
            script,
        );
        nodes.start("a");
        step_until(
            &nodes,
            &scenario.clock,
            "the acknowledged-lost turn",
            || session_idle(&scenario.database),
        )
        .await;
        nodes.kill("a");
        nodes.quiesce().await;
        core_a
    } else {
        scenario.cut(&core_a, script).await;
        assert!(
            observed.models().is_empty(),
            "the cut precedes the first answer"
        );
        core_a.shutdown().await.expect("the first core stops");
        let core_b = Deployment {
            backend: &scenario.backend,
            definition: Some(cold.clone()),
            host: HostPluginFactory { drifted: true },
            models: &[MODEL, NEXT_MODEL],
            answer: write_then_finish(),
            observed: &observed,
            tools: vec![ext_write(Arc::clone(&world))],
            boot: "cold",
        }
        .core();
        let after = nodes(
            &scenario.database,
            &scenario.clock,
            &scenario.backend,
            &core_b,
            Script::new(),
        );
        after.start("b");
        step_until(&after, &scenario.clock, "the takeover", || {
            session_idle(&scenario.database)
        })
        .await;
        after.kill("b");
        after.quiesce().await;
        core_b
    };
    let output = finishing
        .session(session())
        .durable()
        .await
        .expect("the session's durable handle")
        .attach_id(run())
        .output()
        .await
        .expect("the run has a committed answer");
    finishing.shutdown().await.expect("the last core stops");
    assert_eq!(
        world.writes().values().map(Vec::len).collect::<Vec<_>>(),
        [1],
        "the run's one write reached the world once"
    );
    Takeover {
        first,
        cold,
        observed,
        status: output.status(),
    }
}

/// A run whose first `model.start` committed, then lost its node, finishes
/// on a deployment whose definition now answers another model: the resumed
/// call and the newly composed one both run the recorded model, and neither
/// deployment runs the definition again.
#[tokio::test]
async fn a_cold_takeover_reads_the_recorded_config_and_never_resolves_again_on_sqlite_memory() {
    let law = takeover(Fault::CommitThenAbort).await;
    assert_eq!(law.status, lash::TurnStatus::Answered);
    assert_eq!(law.first.resolutions(), 1, "the first owner resolved once");
    assert_eq!(
        law.cold.resolutions(),
        0,
        "a committed run reads its recorded config, never its definition"
    );
    assert_eq!(
        law.observed.models(),
        [MODEL, MODEL],
        "both the pinned and the later call run the recorded model"
    );
    let recorded = format!("audience= facts={TAKEOVER_FACTS}");
    assert_eq!(
        law.observed.host_sections(),
        [recorded.clone(), recorded],
        "the host section reads the run's recorded options before and after the takeover"
    );
}

/// A lost acknowledgement of the first `model.start` reloads the turn on
/// its own node, which reads the recorded config and resolves nothing.
#[tokio::test]
async fn a_lost_acknowledgement_reads_the_recorded_config_on_sqlite_memory() {
    let law = takeover(Fault::AckHidden).await;
    assert_eq!(law.status, lash::TurnStatus::Answered);
    assert_eq!(law.first.resolutions(), 1, "the owner resolved once");
    assert_eq!(law.observed.models(), [MODEL, MODEL]);
    let recorded = format!("audience= facts={TAKEOVER_FACTS}");
    assert_eq!(law.observed.host_sections(), [recorded.clone(), recorded]);
}

/// A node lost before the first `model.start` commits leaves nothing
/// recorded: the next owner derives the run's shape again, under its own
/// deployment, and every call of the run then runs that one derivation.
#[tokio::test]
async fn a_crash_before_the_first_phase_resolves_the_run_again_once_on_sqlite_memory() {
    let law = takeover(Fault::Abort).await;
    assert_eq!(law.status, lash::TurnStatus::Answered);
    assert_eq!(law.first.resolutions(), 1);
    assert_eq!(
        law.cold.resolutions(),
        1,
        "the uncommitted derivation is derived again"
    );
    assert_eq!(
        law.observed.models(),
        [NEXT_MODEL, NEXT_MODEL],
        "the run executes the derivation its first phase recorded"
    );
    let derived = format!("audience= facts={TAKEOVER_FACTS} (drifted)");
    assert_eq!(law.observed.host_sections(), [derived.clone(), derived]);
}

/// A2: a cancel of a committed run completes on a worker that serves
/// neither the run's definition nor its recorded model. The cancel reads the
/// recorded config and resolves nothing, the run ends `Cancelled`, the call
/// its committed round made stays in its committed result, and no model is
/// called.
#[tokio::test]
async fn a_cancel_completes_under_drift_and_keeps_its_committed_calls_on_sqlite_memory() {
    let scenario = Harness::new().await;
    let observed = Arc::new(Observed::default());
    let world = Arc::new(ExternalWorld::default());
    let first = CountingDefinition::default();
    let core_a = Deployment {
        backend: &scenario.backend,
        definition: Some(first.clone()),
        host: HostPluginFactory::default(),
        models: &[MODEL],
        answer: write_then_finish(),
        observed: &observed,
        tools: vec![ext_write(Arc::clone(&world))],
        boot: "first",
    }
    .core();
    scenario
        .send(
            &core_a,
            "write x, then finish",
            lash::RunSpec::definition(definition_ref(), serde_json::Value::Null),
        )
        .await;
    // The round's outcome has committed; its presentation commits with the
    // second call's `model.start`.
    let script = Script::new();
    script.cut_on(
        "a",
        CommitLabel::ROUND_PRESENT_MODEL_START,
        1,
        Fault::CommitThenAbort,
    );
    scenario.cut(&core_a, script).await;
    core_a.shutdown().await.expect("the first core stops");
    assert_eq!(observed.models(), [MODEL], "only the first call answered");
    assert_eq!(world.writes().len(), 1, "the round's call ran");

    let answer = lash_core::runtime::durable::session::request_turn_cancel(
        &scenario.backend,
        lash_durable::domain::TurnCancelRequest {
            session: session(),
            run: run(),
            request_id: "stop-under-drift".to_owned(),
            origin: None,
            reason: None,
            undelivered: lash_core::TurnCancelUndeliveredInputPolicy::Defer,
            mode: lash_core::TurnCancelMode::Immediate,
        },
    )
    .await
    .expect("the cancel request commits");
    assert_eq!(answer, lash_durable::domain::TurnCancelAnswer::Requested);

    // The cancelling deployment registers no run definition and serves only
    // another model.
    let core_b = Deployment {
        backend: &scenario.backend,
        definition: None,
        host: HostPluginFactory::default(),
        models: &[NEXT_MODEL],
        answer: write_then_finish(),
        observed: &observed,
        tools: vec![ext_write(Arc::clone(&world))],
        boot: "drifted",
    }
    .core();
    let after = nodes(
        &scenario.database,
        &scenario.clock,
        &scenario.backend,
        &core_b,
        Script::new(),
    );
    after.start("b");
    step_until(&after, &scenario.clock, "the cancel", || async {
        matches!(
            scenario.database.turn_end(&session(), &run()).await,
            Ok(Some(_))
        )
    })
    .await;
    after.kill("b");
    after.quiesce().await;
    match scenario.database.turn_end(&session(), &run()).await {
        Ok(Some(end)) => assert_eq!(
            end.kind(),
            lash_core_store::store::RunTerminalKind::Cancelled,
            "{end:?}"
        ),
        other => panic!("the cancelled run's end is {other:?}"),
    }
    let output = core_b
        .session(session())
        .durable()
        .await
        .expect("the session's durable handle")
        .attach_id(run())
        .output()
        .await
        .expect("the cancelled run has a committed answer");
    core_b.shutdown().await.expect("the drifted core stops");
    assert_eq!(output.status(), lash::TurnStatus::Cancelled);
    assert_eq!(
        output
            .result
            .tool_calls
            .iter()
            .map(|call| call.tool.as_str())
            .collect::<Vec<_>>(),
        [TOOL],
        "the cancelled run keeps the call its committed round made"
    );
    assert_eq!(first.resolutions(), 1, "the cancel resolved nothing");
    assert_eq!(observed.models(), [MODEL], "the cancel called no model");
    assert_eq!(world.writes().values().map(Vec::len).sum::<usize>(), 1);
}

const SWITCH_TOOL: &str = "switch_frame";
const PROBE_TOOL: &str = "probe";

/// `switch_frame`'s body: it records its turn's spec and switches frames,
/// handing the follow-on that spec, or `other` when the call asks for it.
struct SwitchFrame {
    observed: Arc<Observed>,
    other: lash::RunSpec,
}

#[async_trait::async_trait]
impl StaticToolExecute for SwitchFrame {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let running = call
            .context
            .run_spec()
            .expect("a tool body runs inside a turn")
            .clone();
        self.observed
            .specs
            .lock_recover()
            .push((SWITCH_TOOL, running.hash().expect("the spec encodes")));
        let next = if call.args.get("other") == Some(&serde_json::Value::Bool(true)) {
            self.other.clone()
        } else {
            running
        };
        ToolOutcome::switch_agent_frame(
            lash_core::FrameKey::from_caller_material(call.context.call_id().as_ref())
                .expect("non-empty caller material"),
            "carry on in the next frame".to_owned(),
            Vec::new(),
            next.next_run().expect("the spec encodes"),
        )
        .into()
    }
}

/// `probe`'s body: it records its turn's spec.
struct Probe {
    observed: Arc<Observed>,
}

#[async_trait::async_trait]
impl StaticToolExecute for Probe {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let running = call
            .context
            .run_spec()
            .expect("a tool body runs inside a turn");
        self.observed
            .specs
            .lock_recover()
            .push((PROBE_TOOL, running.hash().expect("the spec encodes")));
        ToolOutcome::ok(serde_json::json!({ "probed": true })).into()
    }
}

fn switch_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::control(
        SWITCH_TOOL,
        SWITCH_TOOL,
        "Switches the agent frame, under this run's spec or another.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "other": { "type": "boolean" } }
        }),
        lash_core::TurnControls::switch_agent_frame(),
    )
    .expect("switch_frame's schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

fn probe_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        PROBE_TOOL,
        PROBE_TOOL,
        "Reads the run's spec.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("probe's schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A6: a send narrows the run's tool access, and the switch produced on its
/// resumed turn hands the follow-on the running turn's recorded spec: the
/// follow-on offers the same narrowed tools and reads the same spec hash.
/// A switch that states another spec runs its follow-on under exactly that
/// spec.
#[tokio::test]
async fn a_frame_switch_hands_its_follow_on_the_recorded_spec_or_the_one_it_states_on_sqlite_memory()
 {
    let scenario = Harness::new().await;
    let observed = Arc::new(Observed::default());
    let narrowed = lash::RunSpec::overrides(lash::RunOverrides {
        tool_access: Some(
            lash::plugins::SessionToolAccess::restricted([switch_definition()])
                .expect("the switch alone"),
        ),
        ..lash::RunOverrides::default()
    });
    let other = lash::RunSpec::overrides(lash::RunOverrides {
        tool_access: Some(
            lash::plugins::SessionToolAccess::restricted([switch_definition(), probe_definition()])
                .expect("the switch and the probe"),
        ),
        ..lash::RunOverrides::default()
    });
    // The run switches, its follow-on switches under `other`, and that
    // follow-on probes, then answers.
    let answer: Answer = Arc::new(|request, index| match index {
        1 => call(SWITCH_TOOL, "{}"),
        2 => call(SWITCH_TOOL, r#"{"other":true}"#),
        3 => call(PROBE_TOOL, "{}"),
        _ => text(request, "done"),
    });
    let tools = |observed: &Arc<Observed>| -> Vec<Arc<dyn lash_core::ToolProvider>> {
        vec![
            Arc::new(StaticToolProvider::new(
                vec![switch_definition()],
                SwitchFrame {
                    observed: Arc::clone(observed),
                    other: other.clone(),
                },
            )),
            Arc::new(StaticToolProvider::new(
                vec![probe_definition()],
                Probe {
                    observed: Arc::clone(observed),
                },
            )),
        ]
    };
    let core_a = Deployment {
        backend: &scenario.backend,
        definition: None,
        host: HostPluginFactory::default(),
        models: &[MODEL],
        answer: Arc::clone(&answer),
        observed: &observed,
        tools: tools(&observed),
        boot: "first",
    }
    .core();
    scenario
        .send(&core_a, "switch frames", narrowed.clone())
        .await;
    // The first call's `model.start` commits, then its node dies: the
    // switch is produced on the resumed turn.
    let script = Script::new();
    script.cut_on("a", CommitLabel::MODEL_START, 1, Fault::CommitThenAbort);
    scenario.cut(&core_a, script).await;
    core_a.shutdown().await.expect("the first core stops");
    let core_b = Deployment {
        backend: &scenario.backend,
        definition: None,
        host: HostPluginFactory::default(),
        models: &[MODEL],
        answer,
        observed: &observed,
        tools: tools(&observed),
        boot: "cold",
    }
    .core();
    let after = nodes(
        &scenario.database,
        &scenario.clock,
        &scenario.backend,
        &core_b,
        Script::new(),
    );
    after.start("b");
    step_until(&after, &scenario.clock, "the follow-on chain", || async {
        observed.requests.lock_recover().len() >= 4 && session_idle(&scenario.database).await
    })
    .await;
    after.kill("b");
    after.quiesce().await;
    core_b.shutdown().await.expect("the cold core stops");

    let narrowed_hash = narrowed.hash().expect("encodes");
    let other_hash = other.hash().expect("encodes");
    assert_eq!(
        *observed.specs.lock_recover(),
        [
            (SWITCH_TOOL, narrowed_hash.clone()),
            (SWITCH_TOOL, narrowed_hash),
            (PROBE_TOOL, other_hash),
        ],
        "the resumed switch's follow-on runs the recorded spec, and a stated spec runs as stated"
    );
    assert_eq!(
        observed.offered(),
        [
            vec!["batch".to_owned(), SWITCH_TOOL.to_owned()],
            vec!["batch".to_owned(), SWITCH_TOOL.to_owned()],
            vec![
                "batch".to_owned(),
                PROBE_TOOL.to_owned(),
                SWITCH_TOOL.to_owned()
            ],
            vec![
                "batch".to_owned(),
                PROBE_TOOL.to_owned(),
                SWITCH_TOOL.to_owned()
            ],
        ],
        "the follow-on keeps the narrowed access (beside the protocol's own `batch`); the \
         stated spec's run offers its own"
    );
}

/// SEND-ISOLATION (FIG-6021): queued sends with different options and tool
/// access each run their own, over the session's config as it stands when
/// each run starts: a standing edit applied ahead of them supplies what
/// they leave unstated. A following default send runs the sticky config,
/// with no earlier run's options, and no send advances the config revision.
#[tokio::test]
async fn queued_sends_run_their_own_options_over_the_sticky_config_on_sqlite_memory() {
    use lash::config::{ConfigSettlement, ConfigTransaction, ConfigWrite};

    let scenario = Harness::new().await;
    let observed = Arc::new(Observed::default());
    let world = Arc::new(ExternalWorld::default());
    let core = Deployment {
        backend: &scenario.backend,
        definition: None,
        host: HostPluginFactory::default(),
        models: &[MODEL],
        answer: Arc::new(|request, _| text(request, "answered")),
        observed: &observed,
        tools: vec![ext_write(Arc::clone(&world))],
        boot: "isolation",
    }
    .core();
    core.session(session())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            root_spec(),
        ))
        .await
        .expect("the session starts");
    let live = core
        .session(session())
        .open()
        .await
        .expect("open admin access");
    let admin = live.admin().config();
    let revision = admin.revision().await.expect("the created revision");
    let edit = admin
        .submit(
            ConfigWrite::new("standing-audience", revision),
            ConfigTransaction::of(SetAudience {
                audience: "operators".to_owned(),
            }),
        )
        .await
        .expect("the standing edit is submitted");
    assert!(matches!(edit, ConfigSettlement::Pending(_)), "{edit:?}");
    let narrowed = lash::plugins::SessionToolAccess::restricted(Vec::new()).expect("no tools");
    let durable = core
        .session(session())
        .durable()
        .await
        .expect("the session's durable handle");
    for (id, facts, access) in [
        ("first-send", "first facts", Some(narrowed.clone())),
        ("second-send", "second facts", None),
    ] {
        let mut send = durable
            .send(lash::TurnInput::text(id))
            .id(TurnId::try_from(id.to_owned()).expect("a host id"))
            .options(
                HOST_OPTIONS,
                HostRunOptions {
                    facts: facts.to_owned(),
                },
            );
        if let Some(access) = access {
            send = send.tool_access(access);
        }
        send.await.expect("the send is accepted");
    }
    let nodes = nodes(
        &scenario.database,
        &scenario.clock,
        &scenario.backend,
        &core,
        Script::new(),
    );
    nodes.start("a");
    step_until(&nodes, &scenario.clock, "the queued sends", || async {
        observed.requests.lock_recover().len() >= 2 && session_idle(&scenario.database).await
    })
    .await;
    let after_edit = admin.revision().await.expect("the edited revision");
    assert_eq!(after_edit, revision + 1, "the standing edit applied once");
    durable
        .send(lash::TurnInput::text("default-send"))
        .id(TurnId::try_from("default-send".to_owned()).expect("a host id"))
        .await
        .expect("the default send is accepted");
    step_until(&nodes, &scenario.clock, "the default send", || async {
        observed.requests.lock_recover().len() >= 3 && session_idle(&scenario.database).await
    })
    .await;
    nodes.kill("a");
    nodes.quiesce().await;
    assert_eq!(
        admin.revision().await.expect("the final revision"),
        after_edit,
        "no send's options advance the config revision"
    );
    drop(live);
    core.shutdown().await.expect("the core stops");
    assert_eq!(
        observed.host_sections(),
        [
            "audience=operators facts=first facts",
            "audience=operators facts=second facts",
            "audience=operators facts=",
        ],
        "each run reads its own options over the sticky audience; the default run none"
    );
    let offered = observed.offered();
    assert!(
        !offered[0].contains(&TOOL.to_owned()),
        "the first send's narrowed access offers no host tool: {:?}",
        offered[0]
    );
    assert!(offered[1].contains(&TOOL.to_owned()), "{:?}", offered[1]);
    assert!(offered[2].contains(&TOOL.to_owned()), "{:?}", offered[2]);
}
