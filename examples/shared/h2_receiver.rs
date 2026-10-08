//! H2's host engine processes: a receiver that runs until it is cancelled, a
//! source that awaits its own pinned key, and a sleeper. The caller lends an
//! admitted handler scope.
use std::sync::Arc;

use anyhow::Result;
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProcessEngine, ProcessEngineContributionContext, ProcessEngineRegistration, ProcessInfraError,
    SessionPlugin,
};
use lash::process::{
    ProcessAwaitOutput, ProcessInput, ProcessOriginator, ProcessStartReceipt, ProcessStartRequest,
};

/// The kind of [`ReceiverEngine`].
pub const RECEIVER_ENGINE_KIND: &str = "h2-receiver";

/// The receiver's input for `session`: what its start declares and what a
/// binding check compares a retained row against.
pub fn receiver_input(session: &lash::SessionId) -> ProcessInput {
    ProcessInput::Engine {
        kind: RECEIVER_ENGINE_KIND.to_owned(),
        payload: serde_json::json!({"fixture":"h2-receiver","session":session}),
    }
}

/// The receiver's engine: its process runs until it is cancelled.
pub struct ReceiverEngine;

#[lash::async_trait]
impl ProcessEngine for ReceiverEngine {
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

    fn creation_config(
        &self,
        _env: &lash::process::ProcessExecutionEnvSpec,
    ) -> std::result::Result<Option<serde_json::Value>, PluginError> {
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
        lash::process::ProcessDefinitionRefusal,
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

    fn creation_config(
        &self,
        _env: &lash::process::ProcessExecutionEnvSpec,
    ) -> std::result::Result<Option<serde_json::Value>, PluginError> {
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
            EngineEvent::KeyPinned { name, .. } => EngineAction::AwaitExternal { name },
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
        lash::process::ProcessDefinitionRefusal,
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

    fn creation_config(
        &self,
        _env: &lash::process::ProcessExecutionEnvSpec,
    ) -> std::result::Result<Option<serde_json::Value>, PluginError> {
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
        lash::process::ProcessDefinitionRefusal,
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

    fn process_engine_contributions(
        &self,
        _context: &ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<Vec<ProcessEngineRegistration>, PluginError> {
        Ok(vec![
            ProcessEngineRegistration::accepting(Arc::new(ReceiverEngine)),
            ProcessEngineRegistration::accepting(Arc::new(SourceEngine)),
            ProcessEngineRegistration::accepting(Arc::new(SleeperEngine)),
        ])
    }

    fn build(
        &self,
        _context: &PluginSessionContext,
    ) -> std::result::Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(ReceiverEnginePlugin))
    }
}

impl lash::plugins::PluginDefinition for ReceiverEnginePlugin {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(RECEIVER_PLUGIN)
    }
}

impl SessionPlugin for ReceiverEnginePlugin {
    fn id(&self) -> &'static str {
        RECEIVER_PLUGIN
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> std::result::Result<(), PluginError> {
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
        .publish_process_env(
            &lash::process::HostArtifactPin::mint(),
            &lash::process::ProcessExecutionEnvSpec::new(
                lash::plugins::AdmittedPluginConfig::default(),
                lash::runtime::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                    lash::NoProgressBudget::bounded(12),
                ),
            ),
        )
        .await?;
    let start = ProcessStartRequest::new(
        receiver_input(session),
        ProcessOriginator::host_scoped(format!("h2:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_env_ref(env_ref)
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
        .publish_process_env(
            &lash::process::HostArtifactPin::mint(),
            &lash::process::ProcessExecutionEnvSpec::new(
                lash::plugins::AdmittedPluginConfig::default(),
                lash::runtime::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                    lash::NoProgressBudget::bounded(12),
                ),
            ),
        )
        .await?;
    let start = ProcessStartRequest::new(
        ProcessInput::Engine {
            kind: SOURCE_ENGINE_KIND.to_owned(),
            payload: source_payload(session, key),
        },
        ProcessOriginator::host_scoped(format!("h2-source:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_env_ref(env_ref)
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
        .publish_process_env(
            &lash::process::HostArtifactPin::mint(),
            &lash::process::ProcessExecutionEnvSpec::new(
                lash::plugins::AdmittedPluginConfig::default(),
                lash::runtime::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                    lash::NoProgressBudget::bounded(12),
                ),
            ),
        )
        .await?;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    let start = ProcessStartRequest::new(
        ProcessInput::Engine {
            kind: SLEEPER_ENGINE_KIND.to_owned(),
            payload: serde_json::json!({"fixture": "h2-sleeper", "until_ms": now + millis}),
        },
        ProcessOriginator::host_scoped(format!("h2-sleeper:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_env_ref(env_ref)
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
