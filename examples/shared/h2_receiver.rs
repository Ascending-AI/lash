//! H2's host engine processes: a receiver that runs until it is cancelled, a
//! source that awaits its own pinned key, and a sleeper. The caller lends an
//! admitted handler scope.
use lash::plugins::PluginRecipe;
use std::sync::Arc;

use anyhow::Result;
use lash::persistence::ProcessStartReceipt;
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProcessEngine, ProcessEngineContributionContext, ProcessEngineRegistration, ProcessInfraError,
    SessionPlugin,
};
use lash::process::{ProcessAwaitOutput, ProcessOriginator, ProcessStartRequest};

/// The kind of [`ReceiverEngine`].
pub const RECEIVER_ENGINE_KIND: &str = "h2-receiver";

/// The receiver's start payload for `session`.
fn receiver_payload(session: &lash::SessionId) -> serde_json::Value {
    serde_json::json!({"fixture":"h2-receiver","session":session})
}

/// The environment every h2 fixture process runs under: no tools and no
/// components, since its engines call none.
fn environment() -> Result<lash::process::ProcessEnv> {
    Ok(lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024)).build()?)
}

/// The receiver's engine: its process runs until it is cancelled.
pub struct ReceiverEngine;

#[lash::async_trait]
impl ProcessEngine for ReceiverEngine {
    fn settled_ran(
        &self,
        _kind: &lash::plugins::EngineStepKind,
        _output: &lash::plugins::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
    }

    fn trace_class(&self) -> lash::plugins::ProcessTraceClass {
        lash::plugins::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash::process::PerformedEffects, lash::process::ProcessEnvRefusal> {
        Ok(lash::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash::process::ArgsMode,
    ) -> std::result::Result<(), lash::process::ArgsMismatch> {
        Err(lash::process::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        RECEIVER_ENGINE_KIND
    }

    fn state_format(&self) -> lash::plugins::EngineStateFormat {
        lash::plugins::EngineStateFormat {
            kind: RECEIVER_ENGINE_KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash::plugins::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash::process::EngineSettings>,
    ) -> std::result::Result<(), PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(PluginError::Registration(format!(
                "process engine `{}` takes no settings",
                self.kind()
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Option<lash::process::EngineSettings>, PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash::plugins::EngineState,
        event: lash::plugins::EngineEvent,
    ) -> std::result::Result<
        (lash::plugins::EngineState, lash::plugins::EngineAction),
        ProcessInfraError,
    > {
        // The receiver runs until it is cancelled, and then answers its
        // cancellation.
        let action = match event {
            lash::plugins::EngineEvent::Cancelled { .. } => lash::plugins::EngineAction::Terminal(
                ProcessAwaitOutput::from_tool_output(lash::tools::ToolCallOutput::cancelled(
                    lash::tools::ToolCancellation::runtime("the receiver was cancelled"),
                )),
            ),
            _ => lash::plugins::EngineAction::Idle,
        };
        Ok((state, action))
    }

    async fn resolve(
        &self,
        _reference: &lash::process::ProcessDefinitionRef,
    ) -> std::result::Result<
        lash::process::ProcessDefinitionResolution,
        lash::process::ProcessDefinitionError,
    > {
        Ok(lash::process::ProcessDefinitionResolution::new(
            lash::process::ProcessSignature::Unknown,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash::persistence::ArtifactName>, PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash::persistence::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash::persistence::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash::persistence::ReferrerClaim,
        artifact_ref: &str,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::Session(format!(
            "the receiver engine stores no artifact `{artifact_ref}`"
        )))
    }
}

/// The kind of [`SourceEngine`].
pub const SOURCE_ENGINE_KIND: &str = "h2-source";

/// The name a source process pins its key under.
const SOURCE_KEY_NAME: &str = "source";

/// A source process: it pins one host-resolvable key, awaits its resolution
/// and ends with the resolved value. Its host finds the key among the
/// process's pending waits ([`source_key`]).
pub struct SourceEngine;

#[lash::async_trait]
impl ProcessEngine for SourceEngine {
    fn settled_ran(
        &self,
        _kind: &lash::plugins::EngineStepKind,
        _output: &lash::plugins::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
    }

    fn trace_class(&self) -> lash::plugins::ProcessTraceClass {
        lash::plugins::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash::process::PerformedEffects, lash::process::ProcessEnvRefusal> {
        Ok(lash::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash::process::ArgsMode,
    ) -> std::result::Result<(), lash::process::ArgsMismatch> {
        Err(lash::process::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        SOURCE_ENGINE_KIND
    }

    fn state_format(&self) -> lash::plugins::EngineStateFormat {
        lash::plugins::EngineStateFormat {
            kind: SOURCE_ENGINE_KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash::plugins::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash::process::EngineSettings>,
    ) -> std::result::Result<(), PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(PluginError::Registration(format!(
                "process engine `{}` takes no settings",
                self.kind()
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Option<lash::process::EngineSettings>, PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash::plugins::EngineState,
        event: lash::plugins::EngineEvent,
    ) -> std::result::Result<
        (lash::plugins::EngineState, lash::plugins::EngineAction),
        ProcessInfraError,
    > {
        use lash::plugins::{EngineAction, EngineEvent, KeyName};
        let name = || KeyName(SOURCE_KEY_NAME.to_owned());
        let output = |output| EngineAction::Terminal(ProcessAwaitOutput::from_tool_output(output));
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::PinKey {
                name: name(),
                // The receiver waits for its source as long as it lives.
                bound: lash::tools::ParkBound::UntilScopeEnd,
            },
            EngineEvent::KeyPinned { name, .. } => EngineAction::AwaitExternal { name, site: None },
            EngineEvent::ExternalResolved { resolution, .. } => match resolution {
                lash::Resolution::Ok(value) => output(lash::tools::ToolCallOutput::success(value)),
                other => output(lash::tools::ToolCallOutput::cancelled(
                    lash::tools::ToolCancellation::runtime(format!(
                        "the source resolved {other:?}"
                    )),
                )),
            },
            EngineEvent::Cancelled { .. } => output(lash::tools::ToolCallOutput::cancelled(
                lash::tools::ToolCancellation::runtime("the source was cancelled"),
            )),
            _ => EngineAction::Idle,
        };
        Ok((state, action))
    }

    async fn resolve(
        &self,
        _reference: &lash::process::ProcessDefinitionRef,
    ) -> std::result::Result<
        lash::process::ProcessDefinitionResolution,
        lash::process::ProcessDefinitionError,
    > {
        Ok(lash::process::ProcessDefinitionResolution::new(
            lash::process::ProcessSignature::Unknown,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash::persistence::ArtifactName>, PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash::persistence::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash::persistence::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash::persistence::ReferrerClaim,
        artifact_ref: &str,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::Session(format!(
            "the source engine stores no artifact `{artifact_ref}`"
        )))
    }
}

/// The kind of [`SleeperEngine`].
pub const SLEEPER_ENGINE_KIND: &str = "h2-sleeper";

/// A sleeper process: it sleeps until its payload's `until_ms`, a durable
/// timer row, then ends with `{"slept": true}`; a cancel ends it cancelled.
pub struct SleeperEngine;

#[lash::async_trait]
impl ProcessEngine for SleeperEngine {
    fn settled_ran(
        &self,
        _kind: &lash::plugins::EngineStepKind,
        _output: &lash::plugins::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
    }

    fn trace_class(&self) -> lash::plugins::ProcessTraceClass {
        lash::plugins::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash::process::PerformedEffects, lash::process::ProcessEnvRefusal> {
        Ok(lash::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash::process::ArgsMode,
    ) -> std::result::Result<(), lash::process::ArgsMismatch> {
        Err(lash::process::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        SLEEPER_ENGINE_KIND
    }

    fn state_format(&self) -> lash::plugins::EngineStateFormat {
        lash::plugins::EngineStateFormat {
            kind: SLEEPER_ENGINE_KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash::plugins::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash::process::EngineSettings>,
    ) -> std::result::Result<(), PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(PluginError::Registration(format!(
                "process engine `{}` takes no settings",
                self.kind()
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Option<lash::process::EngineSettings>, PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash::plugins::EngineState,
        event: lash::plugins::EngineEvent,
    ) -> std::result::Result<
        (lash::plugins::EngineState, lash::plugins::EngineAction),
        ProcessInfraError,
    > {
        use lash::plugins::{EngineAction, EngineEvent};
        let output = |output| EngineAction::Terminal(ProcessAwaitOutput::from_tool_output(output));
        let action = match event {
            EngineEvent::Started { payload } => EngineAction::Sleep {
                until: lash::durable::DurableInstant(
                    payload["until_ms"].as_i64().unwrap_or_default(),
                ),
                site: None,
            },
            EngineEvent::Woke => output(lash::tools::ToolCallOutput::success(
                serde_json::json!({"slept": true}),
            )),
            EngineEvent::Cancelled { .. } => output(lash::tools::ToolCallOutput::cancelled(
                lash::tools::ToolCancellation::runtime("the sleeper was cancelled"),
            )),
            _ => EngineAction::Idle,
        };
        Ok((state, action))
    }

    async fn resolve(
        &self,
        _reference: &lash::process::ProcessDefinitionRef,
    ) -> std::result::Result<
        lash::process::ProcessDefinitionResolution,
        lash::process::ProcessDefinitionError,
    > {
        Ok(lash::process::ProcessDefinitionResolution::new(
            lash::process::ProcessSignature::Unknown,
        ))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash::persistence::ArtifactName>, PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash::persistence::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash::persistence::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash::persistence::ReferrerClaim,
        artifact_ref: &str,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::Session(format!(
            "the sleeper engine stores no artifact `{artifact_ref}`"
        )))
    }
}

/// The plugin that contributes [`ReceiverEngine`], [`SourceEngine`] and
/// [`SleeperEngine`] to a host's core.
pub struct ReceiverEnginePlugin;

const RECEIVER_PLUGIN: &str = "h2-receiver-engine";

impl PluginFactory for ReceiverEnginePlugin {
    fn id(&self) -> &'static str {
        RECEIVER_PLUGIN
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> = {
            let mut reg = lash::plugins::PluginRegistrar::<ReceiverEnginePlugin>::new(
                lash::plugins::PluginFactory::id(self.as_ref()),
            );
            ReceiverEnginePlugin::declare(&mut reg)?;
            let id = lash::plugins::PluginFactory::id(self.as_ref());
            Ok(lash::plugins::PluginRecipe::new(
                id,
                reg,
                move |_context, _state| Ok(Arc::new(ReceiverEnginePlugin)),
            ))
        };
        let mut recipe = recipe?;
        recipe = recipe.with_process_engine(RECEIVER_ENGINE_KIND, |_| {
            Ok(ProcessEngineRegistration::accepting(Arc::new(
                ReceiverEngine,
            )))
        });
        recipe = recipe.with_process_engine(SOURCE_ENGINE_KIND, |_| {
            Ok(ProcessEngineRegistration::accepting(Arc::new(SourceEngine)))
        });
        recipe = recipe.with_process_engine(SLEEPER_ENGINE_KIND, |_| {
            Ok(ProcessEngineRegistration::accepting(Arc::new(
                SleeperEngine,
            )))
        });
        Ok(recipe)
    }
}

impl ReceiverEnginePlugin {
    pub fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(RECEIVER_PLUGIN)
    }
}
impl lash::plugins::PluginMetadata for ReceiverEnginePlugin {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        Self::declaration()
    }
}

impl SessionPlugin for ReceiverEnginePlugin {
    fn id(&self) -> &'static str {
        RECEIVER_PLUGIN
    }
}
impl ReceiverEnginePlugin {
    fn declare(
        reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        Ok(())
    }
}

pub async fn register_receiver(
    core: &lash::LashCore,
    session: &lash::SessionId,
    scoped: lash::runtime::ActorContext,
) -> Result<ProcessStartReceipt> {
    // The receiver runs under an execution environment the host publishes; a
    // host pin keeps it alive for the process (ADR 0113).
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash::process::HostArtifactPin::mint(), &environment()?)
        .await?;
    let start = ProcessStartRequest::engine(
        RECEIVER_ENGINE_KIND,
        receiver_payload(session),
        lash::process::EngineLaunch::new(env_ref, None),
        ProcessOriginator::host_scoped(format!("h2:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_host_start_key(format!("h2-receiver:{session}"))
    .with_observers([session.clone()]);
    Ok(core.processes().start(start, scoped).await?)
}

/// Start a source process observed by `session`, under `key`.
pub async fn start_source(
    core: &lash::LashCore,
    session: &lash::SessionId,
    key: &str,
    scoped: lash::runtime::ActorContext,
) -> Result<ProcessStartReceipt> {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash::process::HostArtifactPin::mint(), &environment()?)
        .await?;
    let start = ProcessStartRequest::engine(
        SOURCE_ENGINE_KIND.to_owned(),
        source_payload(session, key),
        lash::process::EngineLaunch::new(env_ref, None),
        ProcessOriginator::host_scoped(format!("h2-source:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_host_start_key(format!("h2-source:{session}:{key}"))
    .with_observers([session.clone()]);
    Ok(core.processes().start(start, scoped).await?)
}

/// Start a sleeper process observed by `session`, sleeping for `millis`.
pub async fn start_sleeper(
    core: &lash::LashCore,
    session: &lash::SessionId,
    millis: i64,
    scoped: lash::runtime::ActorContext,
) -> Result<ProcessStartReceipt> {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash::process::HostArtifactPin::mint(), &environment()?)
        .await?;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    let start = ProcessStartRequest::engine(
        SLEEPER_ENGINE_KIND.to_owned(),
        serde_json::json!({"fixture": "h2-sleeper", "until_ms": now + millis}),
        lash::process::EngineLaunch::new(env_ref, None),
        ProcessOriginator::host_scoped(format!("h2-sleeper:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_host_start_key(format!("h2-sleeper:{session}"))
    .with_observers([session.clone()]);
    Ok(core.processes().start(start, scoped).await?)
}

/// The start payload of `session`'s source `key`: what names the source to
/// its host.
fn source_payload(session: &lash::SessionId, key: &str) -> serde_json::Value {
    serde_json::json!({"fixture": "h2-source", "session": session, "source": key})
}

/// The key source process `process` pinned, once it pinned it: read from the
/// process's pending waits, so any node answers, also after a restart.
pub async fn source_key(
    core: &lash::LashCore,
    process: &lash::ProcessId,
) -> Result<Option<String>> {
    Ok(core
        .completions()
        .pinned_keys(process)
        .await?
        .into_iter()
        .find(|pinned| pinned.name.0 == SOURCE_KEY_NAME)
        .map(|pinned| pinned.key.as_str().to_owned()))
}
