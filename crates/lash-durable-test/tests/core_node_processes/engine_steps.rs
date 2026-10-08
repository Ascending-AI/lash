//! Engine steps on the core's node: a plugin engine's own body runs through
//! its registration, an engine without the capability is refused before
//! admission, and an engine step retries as its kind declares unless its
//! host overrides it.

use super::*;

/// The engine-step engine: it runs its own `double` body once on its start
/// payload, and ends with what the body answered.
fn engine_step_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { payload } => lash_core::EngineAction::Steps {
            steps: vec![lash_core::StepRequest::Engine {
                step: lash_core::StepName("double".to_owned()),
                kind: lash_core::EngineStepKind::new(DOUBLE),
                input: payload,
            }],
            wake: None,
        },
        lash_core::EngineEvent::StepSettled { outcome, .. } => {
            answer(serde_json::json!({ "step": settled(&outcome) }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// The engine body a plugin's engine declares.
const DOUBLE: &str = "double";

/// A plugin-contributed engine that declares the `double` body.
const PLUGIN_ENGINE: &str = "core-node-plugin-engine";

/// A host engine registered on the backend, which declares no body.
const BARE_ENGINE: &str = "core-node-bare-engine";

/// The `double` body: it answers twice its input, counting its runs.
struct Double {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for Double {
    fn kinds(&self) -> Vec<lash_core::EngineStepKind> {
        vec![lash_core::EngineStepKind::new(DOUBLE)]
    }

    fn execution(&self, _kind: &lash_core::EngineStepKind) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

    fn retry(&self, _kind: &lash_core::EngineStepKind) -> lash_core::ExecutionPolicy {
        lash_core::ExecutionPolicy::Once
    }

    async fn run(
        &self,
        run: lash_core::EngineStepRun,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> lash_core::SettledOutput {
        use lash_core::tool_run::{MaterialOwner, MaterialRole};
        self.runs.fetch_add(1, Ordering::SeqCst);
        let text = (run.input.as_i64().unwrap_or_default() * 2).to_string();
        lash_core::SettledOutput::Completed(lash_core::Material::journal_local(
            MaterialOwner::Process {
                process_id: run.process,
            },
            MaterialRole::AttemptOutput,
            text,
        ))
    }
}

/// A plugin that contributes [`PLUGIN_ENGINE`] with its `double` body, as a
/// host's engine plugin does.
struct EnginePlugin {
    runs: Arc<AtomicUsize>,
}

struct NoSessionPlugin;

impl lash_core::plugin::SessionPlugin for NoSessionPlugin {
    fn id(&self) -> &'static str {
        "core-node-engine-plugin"
    }

    fn register(
        &self,
        _registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for EnginePlugin {
    fn id(&self) -> &'static str {
        "core-node-engine-plugin"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![
            lash_core::ProcessEngineRegistration::accepting(Arc::new(ScriptEngine {
                kind: PLUGIN_ENGINE,
                advance: engine_step_advance,
            }))
            .with_engine_steps(Arc::new(Double {
                runs: Arc::clone(&self.runs),
            })),
        ])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoSessionPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for EnginePlugin {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("core-node-engine-plugin")
    }
}

/// An engine a plugin contributes advances on the core's node as a host's
/// does, and its engine step runs through the `EngineSteps` its
/// registration declares, once.
async fn plugin_engine_step_runs_through_its_registration(tier: Tier) {
    let runs = Arc::new(AtomicUsize::new(0));
    let plugin = Arc::new(EnginePlugin {
        runs: Arc::clone(&runs),
    });
    let deployment = deploy(tier, Vec::new(), |builder| builder.plugin(plugin)).await;
    let process = start(&deployment.core, PLUGIN_ENGINE, serde_json::json!(21)).await;
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "step": 42 }));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the body ran once");
}

on_every_tier!(plugin_engine_step_runs_through_its_registration);

/// An engine step of an engine whose registration declares no engine
/// steps is refused before its admission: the process ends `Failed` with
/// the typed refusal, and no step was admitted or run.
async fn engine_step_without_the_capability_is_refused_before_admission(tier: Tier) {
    let deployment = deploy(
        tier,
        vec![Arc::new(ScriptEngine {
            kind: BARE_ENGINE,
            advance: engine_step_advance,
        })],
        |builder| builder,
    )
    .await;
    let process = start(&deployment.core, BARE_ENGINE, serde_json::json!(21)).await;
    let output = ended(&deployment.core, &process).await;
    assert!(!output.is_success(), "the process failed: {output:?}");
    let refusal = lash_core::EngineStepRefusal::NoEngineSteps {
        engine: BARE_ENGINE.to_owned(),
    }
    .to_string();
    assert!(
        format!("{output:?}").contains(&refusal),
        "the process ends with the typed refusal `{refusal}`: {output:?}"
    );
    let rows = deployment
        .backend
        .durable()
        .run_records(&lash_durable::domain::OwnerKey::Process(process))
        .await
        .expect("the process's run records are read");
    assert!(rows.is_empty(), "no step was admitted: {rows:?}");
}

on_every_tier!(engine_step_without_the_capability_is_refused_before_admission);

/// The engine body that always fails, retryably: it counts its runs.
const FLAKY: &str = "flaky";

/// A plugin-contributed engine that runs its `flaky` body once and ends
/// with what the body answered.
const RETRY_ENGINE: &str = "core-node-retry-engine";

/// The `flaky` body, declared with `retry` as its engine author's policy.
struct Flaky {
    runs: Arc<AtomicUsize>,
    retry: lash_core::ExecutionPolicy,
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for Flaky {
    fn kinds(&self) -> Vec<lash_core::EngineStepKind> {
        vec![lash_core::EngineStepKind::new(FLAKY)]
    }

    fn execution(&self, _kind: &lash_core::EngineStepKind) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

    fn retry(&self, _kind: &lash_core::EngineStepKind) -> lash_core::ExecutionPolicy {
        self.retry
    }

    async fn run(
        &self,
        run: lash_core::EngineStepRun,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> lash_core::SettledOutput {
        use lash_core::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
        self.runs.fetch_add(1, Ordering::SeqCst);
        let output = lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::Internal,
            "flaky",
            "the flaky body failed",
        ));
        lash_core::SettledOutput::Failed(
            lash_core::Material::journal_local(
                MaterialOwner::Process {
                    process_id: run.process,
                },
                MaterialRole::AttemptOutput,
                serde_json::to_string(&output).expect("a tool output encodes"),
            )
            .failure(KnownFailureReason::Reported, None),
        )
    }
}

fn flaky_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { .. } => lash_core::EngineAction::Steps {
            steps: vec![lash_core::StepRequest::Engine {
                step: lash_core::StepName("flaky".to_owned()),
                kind: lash_core::EngineStepKind::new(FLAKY),
                input: serde_json::Value::Null,
            }],
            wake: None,
        },
        lash_core::EngineEvent::StepSettled { outcome, .. } => {
            answer(serde_json::json!({ "step": settled(&outcome) }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// A plugin that contributes [`RETRY_ENGINE`] with its `flaky` body under
/// `declared`, and registers it with the host's `overridden` policy when
/// there is one.
struct RetryPlugin {
    runs: Arc<AtomicUsize>,
    declared: lash_core::ExecutionPolicy,
    overridden: Option<lash_core::ExecutionPolicy>,
}

struct NoRetrySessionPlugin;

impl lash_core::plugin::SessionPlugin for NoRetrySessionPlugin {
    fn id(&self) -> &'static str {
        "core-node-retry-plugin"
    }

    fn register(
        &self,
        _registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for RetryPlugin {
    fn id(&self) -> &'static str {
        "core-node-retry-plugin"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        let registration =
            lash_core::ProcessEngineRegistration::accepting(Arc::new(ScriptEngine {
                kind: RETRY_ENGINE,
                advance: flaky_advance,
            }))
            .with_engine_steps(Arc::new(Flaky {
                runs: Arc::clone(&self.runs),
                retry: self.declared,
            }));
        Ok(vec![match self.overridden {
            Some(policy) => {
                registration.with_engine_step_retry(lash_core::EngineStepKind::new(FLAKY), policy)
            }
            None => registration,
        }])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NoRetrySessionPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for RetryPlugin {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("core-node-retry-plugin")
    }
}

/// How many times the `flaky` body ran before its process ended, under the
/// engine's `declared` policy and the host's `overridden` one.
async fn flaky_runs(
    tier: Tier,
    declared: lash_core::ExecutionPolicy,
    overridden: Option<lash_core::ExecutionPolicy>,
) -> usize {
    let runs = Arc::new(AtomicUsize::new(0));
    let plugin = Arc::new(RetryPlugin {
        runs: Arc::clone(&runs),
        declared,
        overridden,
    });
    let deployment = deploy(tier, Vec::new(), |builder| builder.plugin(plugin)).await;
    let process = start(&deployment.core, RETRY_ENGINE, serde_json::Value::Null).await;
    let output = ended(&deployment.core, &process).await;
    assert!(
        output.is_success(),
        "the engine answers with the step's failure: {output:?}"
    );
    runs.load(Ordering::SeqCst)
}

/// An engine step runs under the retry policy its kind declares, which the
/// host may override when it registers the engine (D-STEPRETRY): a kind
/// declared `Once` runs its failing body once, a host's `Repeatable`
/// override retries it to its attempts, and a host's `Once` override stops
/// an engine's declared retries.
async fn an_engine_step_retries_as_its_kind_declares_unless_its_host_overrides(tier: Tier) {
    let three =
        lash_core::ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN.saturating_add(2), 0, 0);
    assert_eq!(
        flaky_runs(tier, lash_core::ExecutionPolicy::Once, None).await,
        1,
        "a step kind declared `Once` is not retried after its failure"
    );
    assert_eq!(
        flaky_runs(tier, lash_core::ExecutionPolicy::Once, Some(three)).await,
        3,
        "the host's `Repeatable` override retries it"
    );
    assert_eq!(
        flaky_runs(tier, three, Some(lash_core::ExecutionPolicy::Once)).await,
        1,
        "the host's `Once` override stops the engine's declared retries"
    );
}

on_every_tier!(an_engine_step_retries_as_its_kind_declares_unless_its_host_overrides);
