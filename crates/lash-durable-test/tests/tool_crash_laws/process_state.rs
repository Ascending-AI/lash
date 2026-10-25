//! The process-engine half of the plugin-state crash laws.
use super::*;

const STATE_ENGINE: &str = "crash-state-engine";

/// Which steps the process scenario's engine asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Steps {
    /// [`STATE_SET`] `P`, then, once it settled, [`STATE_OBSERVE`] `Q`.
    Sequence,
    /// [`STATE_SET`] `A` and [`STATE_OBSERVE`] `B` at once.
    Pair,
}

/// The first string under `key` in `value`, depth first, looking into
/// strings that encode JSON too.
fn find_text(value: &serde_json::Value, key: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| map.values().find_map(|value| find_text(value, key))),
        serde_json::Value::Array(items) => items.iter().find_map(|item| find_text(item, key)),
        serde_json::Value::String(text) => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .filter(|parsed| parsed.is_object() || parsed.is_array())
            .and_then(|parsed| find_text(&parsed, key)),
        _ => None,
    }
}

/// A step's tool output, as its payload carries it.
fn step_answer(outcome: &SettledOutput) -> serde_json::Value {
    outcome
        .payload()
        .and_then(|payload| serde_json::from_str(payload).ok())
        .unwrap_or_default()
}

/// The engine: its steps per [`Steps`], then a terminal carrying the value
/// the [`STATE_SET`] step's outcome set (`set`) and what the
/// [`STATE_OBSERVE`] step saw (`seen`). Its state is the settled steps'
/// answers so far, by step.
struct StateEngine {
    steps: Steps,
}

fn tool_step(
    step: &str,
    tool: &str,
    label: &str,
) -> lash_execution_contracts::process::StepRequest {
    lash_execution_contracts::process::StepRequest::Tool {
        step: lash_execution_contracts::process::StepName(step.to_owned()),
        use_: operations::use_for(&state_definition(tool)),
        input: serde_json::json!({ "label": label }),
        site: None,
    }
}

fn engine_failure(
    error: impl std::fmt::Display,
) -> lash_execution_contracts::process::ProcessInfraError {
    lash_execution_contracts::process::ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()),
    )
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for StateEngine {
    fn settled_ran(
        &self,
        _kind: &EngineStepKind,
        _output: &SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        _link: lash_vocabulary::LinkId,
        _selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "this scripted engine has no executable document".into(),
            },
        )
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "this scripted engine has no executable document".into(),
            },
        )
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        STATE_ENGINE
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
            kind: STATE_ENGINE.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core_execution::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let mut settled: BTreeMap<String, serde_json::Value> = if state.bytes.is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_slice(&state.bytes).map_err(engine_failure)?
        };
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::Steps {
                steps: match self.steps {
                    Steps::Sequence => vec![tool_step("set", STATE_SET, "P")],
                    Steps::Pair => vec![
                        tool_step("set", STATE_SET, "A"),
                        tool_step("observe", STATE_OBSERVE, "B"),
                    ],
                },
                wake: None,
            },
            EngineEvent::StepSettled { step, outcome, .. } => {
                settled.insert(step.0.clone(), step_answer(&outcome));
                match (self.steps, step.0.as_str()) {
                    (Steps::Sequence, "set") => EngineAction::Steps {
                        steps: vec![tool_step("observe", STATE_OBSERVE, "Q")],
                        wake: None,
                    },
                    _ if settled.len() == 2 => EngineAction::Terminal(
                        lash_execution_contracts::process::ProcessOutcome::from_tool_output(
                            lash_core_execution::ToolCallOutput::success(serde_json::json!({
                                "set": settled.get("set").and_then(|answer| find_text(answer, "value")),
                                "seen": settled.get("observe").and_then(|answer| find_text(answer, "seen")),
                            })),
                        ),
                    ),
                    _ => EngineAction::Idle,
                }
            }
            EngineEvent::Cancelled { origin, .. } => EngineAction::Terminal(
                lash_execution_contracts::process::ProcessOutcome::from_tool_output(
                    lash_core_execution::ToolCallOutput::cancelled(
                        lash_core_execution::ToolCancellation::runtime("cancelled")
                            .with_origin(origin),
                    ),
                ),
            ),
            _ => EngineAction::Idle,
        };
        let state = EngineState {
            staged: Vec::new(),
            format: self.state_format(),
            bytes: serde_json::to_vec(&settled).map_err(engine_failure)?,
            pages: None,
            requirements: Default::default(),
        };
        Ok((state, action))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        Vec<lash_core_execution::ArtifactName>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core_execution::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core_execution::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        Err(
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the state engine stores no artifact `{artifact_ref}`"
            )),
        )
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}

/// The environment a host start states in these laws: the state
/// component, its two tools linked.
fn process_environment() -> lash_core::ProcessEnv {
    lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(16))
        .component(STATE_COMPONENT, &())
        .execution(operations::environment(
            STATE_COMPONENT.tool_provider(lash::tool_provider_key!("state")),
            &[state_definition(STATE_SET), state_definition(STATE_OBSERVE)],
        ))
        .build()
        .expect("the process environment builds")
}

/// The process scenario on one dialect, fresh for every matrix cell: a
/// host starts one process of [`StateEngine`] through the core's process
/// API, and the simulated nodes run what the core's node runs.
pub(super) struct ProcessCrash {
    steps: Steps,
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<World>,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    process: Mutex<Option<lash_vocabulary::identity::ProcessId>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl ProcessCrash {
    pub(super) fn new(steps: Steps, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            steps,
            dialect,
            postgres_url,
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            process: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core: it serves no node of its own, the simulated
    /// nodes run its actors.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .process_component(
                        Arc::new(StateComponent {
                            world: Arc::clone(&self.world),
                        }),
                        STATE_COMPONENT,
                    )
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("process-state-deployment"),
                        lash::persistence::LeaseIncarnationId::new("process-state-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    fn process(&self) -> Option<lash_vocabulary::identity::ProcessId> {
        self.process.lock_recover().clone()
    }

    fn actor(process: &lash_vocabulary::identity::ProcessId) -> ActorKey {
        ActorKey::process(process.as_str()).expect("a process actor key")
    }

    /// The process's answer: what its [`STATE_SET`] step set and what its
    /// [`STATE_OBSERVE`] step saw.
    async fn answer(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Result<(Option<String>, Option<String>), String> {
        let terminal = self
            .backend()
            .process_registry()
            .get_process(process)
            .await
            .map_err(|error| format!("read the process: {error}"))?
            .and_then(|record| record.terminal().cloned())
            .ok_or("the process holds no terminal")?;
        let answer =
            serde_json::to_value(lash_protocol_types::Durable(&terminal.into_await_output()))
                .map_err(|error| format!("the terminal encodes: {error}"))?;
        Ok((find_text(&answer, "set"), find_text(&answer, "seen")))
    }
}

#[async_trait::async_trait]
impl Scenario for ProcessCrash {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(served::backend_with(
            stores,
            vec![Arc::new(StateEngine { steps: self.steps })],
        ));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            serves: lash::testing::node_serves(&self.core()),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        // What the core's own node runs: its sessions, and its processes on
        // its durable process worker.
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let _ = self.world.nodes.set(Arc::downgrade(nodes));
        // The host is outside the deployment under test: its start is uncut.
        let core = self.core();
        let pin = lash_core::HostArtifactPin::mint();
        operations::retain(
            core.backend(),
            &pin,
            &[state_definition(STATE_SET), state_definition(STATE_OBSERVE)],
        )
        .await;
        let env_ref = core
            .host_artifacts()
            .publish_process_env(&pin, &process_environment())
            .await
            .map_err(|error| format!("publish the environment: {error}"))?;
        let request = lash_execution_contracts::process::ProcessStartRequest::engine(
            STATE_ENGINE.to_owned(),
            serde_json::Value::Null,
            lash_execution_contracts::process::EngineLaunch::new(env_ref, None),
            lash_execution_contracts::process::ProcessOriginator::host(),
            lash_execution_contracts::process::LifetimeDecision::Detached,
        );
        let process = core
            .processes()
            .start(request, core.effect_host())
            .await
            .map(|receipt| receipt.process_id)
            .map_err(|error| format!("start the process: {error}"))?;
        *self.process.lock_recover() = Some(process);
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        self.process()
            .as_ref()
            .map(Self::actor)
            .into_iter()
            .collect()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let Some(process) = self.process() else {
            return false;
        };
        matches!(
            nodes.database().actor(&Self::actor(&process)).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let Some(process) = self.process() else {
            return vec!["nothing was started".to_owned()];
        };
        let trace = nodes.script().trace();
        let mut violations = observation_laws(&self.world);
        let terminals = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::PROCESS_TERMINAL && write.committed())
            .count();
        if terminals != 1 {
            violations.push(format!("the process committed {terminals} terminals"));
        }
        match self.answer(&process).await {
            Err(error) => violations.push(error),
            Ok((None, _)) => violations.push("the set step's outcome set nothing".to_owned()),
            // The next step sees exactly the value the set step's committed
            // outcome carries.
            Ok((set, seen)) if self.steps == Steps::Sequence && seen != set => violations.push(
                format!("the next step saw {seen:?}, not the committed value {set:?}"),
            ),
            // A concurrent step sees the committed value or nothing.
            Ok((set, Some(seen))) if self.steps == Steps::Pair && Some(&seen) != set.as_ref() => {
                violations.push(format!(
                    "the concurrent step saw {seen:?}, not the committed value {set:?}"
                ));
            }
            Ok(_) => {}
        }
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        if !violations.is_empty() {
            violations.push(format!("body entries: {:?}", self.world.entries()));
        }
        violations
    }
}
