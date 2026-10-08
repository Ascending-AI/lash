//! H2's mutation receiver uses the real process registry and returns its
//! original event records. The caller lends an admitted handler scope. The
//! receiver is a host engine process: it holds the events tools append to it
//! and runs until it is cancelled.
use std::sync::Arc;

use anyhow::{Result, ensure};
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProcessEngine, ProcessEngineContributionContext, ProcessEngineRegistration, ProcessInfraError,
    SessionPlugin,
};
use lash::process::{
    ProcessAwaitOutput, ProcessEvent, ProcessEventPageEvents, ProcessEventPageMore,
    ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventType, ProcessInput,
    ProcessOriginator, ProcessStartReceipt, ProcessStartRequest,
};

use serde::{Deserialize, Serialize};

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

/// The receiver's engine: its process runs until it is cancelled, holding
/// the events tools append to it.
pub struct ReceiverEngine;

#[lash::async_trait]
impl ProcessEngine for ReceiverEngine {
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
            Vec::new(),
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

/// The event a source process appends once its key is pinned: the key the
/// host resolves it by.
pub const SOURCE_PINNED: &str = "h2_source_pinned";

fn source_pinned_type() -> std::result::Result<ProcessEventType, ProcessInfraError> {
    Ok(ProcessEventType {
        name: SOURCE_PINNED.to_owned(),
        payload_schema: lash::schema::JsonSchema::admit(serde_json::json!({
            "type": "object", "required": ["key"],
            "properties": {"key": {"type": "string"}}, "additionalProperties": false,
        }))
        .map_err(|error| ProcessInfraError::new(PluginError::Session(error.to_string())))?,
        semantics: Default::default(),
    })
}

/// A source process: it pins one host-resolvable key, appends the key as
/// [`SOURCE_PINNED`], awaits its resolution and ends with the resolved
/// value. Each transition follows from its event alone.
pub struct SourceEngine;

#[lash::async_trait]
impl ProcessEngine for SourceEngine {
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
        use lash::plugins::{EngineAction, EngineEvent, HostWaitKind, KeyName};
        let name = || KeyName("source".to_owned());
        let output = |output| EngineAction::Terminal(ProcessAwaitOutput::from_tool_output(output));
        let action = match event {
            EngineEvent::Started { .. } => EngineAction::PinKey {
                name: name(),
                kind: HostWaitKind::Custom,
                // The receiver waits for its source as long as it lives.
                bound: lash::tools::ParkBound::UntilScopeEnd,
            },
            EngineEvent::KeyPinned { key, .. } => EngineAction::Emit {
                event_type: source_pinned_type()?,
                payload: serde_json::json!({"key": key.as_str()}),
            },
            EngineEvent::Emitted => EngineAction::AwaitExternal { name: name() },
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
            Vec::new(),
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
            Vec::new(),
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
    event_type: &str,
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
    .with_observers([session.clone()])
    .with_extra_event_types([ProcessEventType {
        name: event_type.to_owned(),
        payload_schema: lash::schema::JsonSchema::admit(
            serde_json::json!({"type":"object","required":["call_id","value"],
            "properties":{"call_id":{"type":"string"},"value":{}},"additionalProperties":false}),
        )?,
        semantics: Default::default(),
    }]);
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
                ),
            ),
        )
        .await?;
    let start = ProcessStartRequest::new(
        ProcessInput::Engine {
            kind: SOURCE_ENGINE_KIND.to_owned(),
            payload: serde_json::json!({"fixture": "h2-source", "session": session}),
        },
        ProcessOriginator::host_scoped(format!("h2-source:{session}")),
        lash::process::Lifetime::Detached,
    )
    .with_env_ref(env_ref)
    .with_host_start_key(format!("h2-source:{session}:{key}"))
    .with_observers([session.clone()])
    .with_extra_event_types([source_pinned_type().map_err(|error| anyhow::anyhow!("{error}"))?]);
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

/// The key source process `process` pinned, once it appended it.
pub async fn source_key(
    core: &lash::LashCore,
    process: &lash::ProcessId,
) -> Result<Option<String>> {
    let events = receiver_events(core, process).await?;
    Ok(events
        .events
        .iter()
        .find(|event| event.event_type == SOURCE_PINNED)
        .and_then(|event| event.payload["key"].as_str().map(ToOwned::to_owned)))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiverEvents {
    pub kind: String,
    pub process_id: lash::ProcessId,
    pub events: Vec<ProcessEvent>,
}

pub async fn receiver_events(
    core: &lash::LashCore,
    process_id: &lash::ProcessId,
) -> Result<ReceiverEvents> {
    let read = core
        .process_registry()
        .event_page_after(
            process_id,
            0,
            std::num::NonZeroUsize::new(1024)
                .ok_or_else(|| anyhow::anyhow!("receiver page must be nonzero"))?,
            ProcessEventQueryMode::Full,
        )
        .await?;
    let ProcessEventReadOutcome::Retained(page) = read else {
        anyhow::bail!("receiver events no longer retained");
    };
    ensure!(
        page.more == ProcessEventPageMore::Complete,
        "receiver evidence page truncated"
    );
    let ProcessEventPageEvents::Full(events) = page.events else {
        anyhow::bail!("receiver evidence is not full");
    };
    Ok(ReceiverEvents {
        kind: "h2_receiver_events".to_owned(),
        process_id: process_id.clone(),
        events,
    })
}
