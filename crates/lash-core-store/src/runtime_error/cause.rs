//! The typed causes a runtime error carries beside its code.

use super::{IngressReservedSourceKeyRefusal, RuntimeEffectControllerError, RuntimeErrorCode};
use crate::SessionId;

/// Typed cause retained when a controller-owned runtime effect must abort
/// through the generic runtime error boundary.
///
/// Attachment retention and worker causes keep their source's retry class.
/// Other causes are terminal except for an unavailable recorded model.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuntimeErrorCause {
    /// A source's authority or lifecycle refused an arm, subscription or seal.
    SourceRefused {
        refusal: Box<crate::tool_run::SourceRefusal>,
    },
    /// A physical boundary or successor refused the logical Run's transfer.
    RunContinuationRefused {
        refusal: Box<crate::tool_run::ContinuationRefusal>,
    },
    /// Recorded material could not be read. This cause grants no body retry.
    MaterialRefused {
        refusal: Box<crate::tool_run::MaterialRefusal>,
    },
    /// A callback returned state commands where no recorded body carries
    /// their resolution.
    PluginStateUnrecorded {
        plugin: String,
    },
    /// A recorded callback's published state belongs to another owner.
    PluginStateEffectOwnerMismatch,
    /// A recorded state resolution does not follow its namespace's applied
    /// frontier.
    PluginStateFrontier {
        refusal: Box<crate::tool_run::NamespaceFrontierRefusal>,
    },
    /// A namespace holds a reduced publication of unknown durability; it
    /// publishes nothing more until the owner is rebuilt from durable state.
    /// The attempt is not recorded: the invocation's recovery owns it.
    PluginStatePublicationFenced {
        plugin: String,
    },
    /// A completed provider call's refusal, retained through plugin and host errors.
    ProviderFailure {
        failure_kind: lash_sansio::llm::types::ProviderFailureKind,
        code: Option<lash_sansio::FailureCode>,
        retryable: bool,
        terminal_reason: lash_sansio::llm::types::LlmTerminalReason,
    },
    PluginOperation {
        failure: Box<lash_sansio::PluginOperationFailure>,
    },
    PluginHooks {
        causes: Vec<lash_sansio::PluginHookFailure>,
    },
    /// A typed worker failure retained through protocol and host errors.
    VmWorker {
        outcome: Box<lash_vm_protocol::InfrastructureOutcome>,
    },
    PluginExecution {
        refusal: Box<crate::store::plugin_writers::PluginExecutionRefusal>,
    },
    PluginFormat {
        refusal: Box<crate::plugin_state::FormatRefusal>,
    },
    /// A process start names a scope whose lifetime has closed.
    ProcessParentEnded {
        start_key: Option<crate::process_identity::StartKey>,
        parent: Box<crate::scope_identity::ScopeId>,
    },
    /// A host start key already names a different start.
    ProcessStartKeyConflict {
        start_key: crate::process_identity::StartKey,
    },
    /// The attachment-store family and structured source of a required retention failure.
    AttachmentRetention {
        failure: Box<super::AttachmentRetentionFailure>,
    },
    ModuleArtifactRefused {
        refusal: Box<lash_sansio::module_artifact_refusal::ModuleArtifactRefusal>,
    },
    SchemaRefused {
        source: Box<lash_sansio::SchemaAdmissionError>,
    },
    ToolSchemaRefused {
        source: Box<lash_sansio::ToolCatalogBuildError>,
    },
    ValueMismatch {
        context: Box<str>,
        source: Box<lash_sansio::ValueMismatch>,
    },
    IngressReservedSourceKey {
        #[serde(flatten)]
        refusal: Box<IngressReservedSourceKeyRefusal>,
    },
    StoreRefusal {
        refusal: Box<crate::store::StoreRefusal>,
    },
    /// A durable record cannot be decoded. Retrying cannot repair its bytes.
    StoredDataCorrupt {
        #[serde(flatten)]
        corruption: Box<StoredDataCorruption>,
    },
    /// A process row lacks the configuration its engine requires to execute.
    MissingRecordedProcessConfig {
        engine_kind: String,
    },
    /// A Lash object's compatibility record, or its absence, refuses this
    /// build (ADR 0115 §3.2): the typed half of
    /// [`RuntimeErrorCode::EngineObjectStateFormatUnsupported`] when the
    /// refusal is the object's, not one stored value's.
    Compat {
        refusal: Box<crate::compat::CompatRefusal>,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    /// An artifact publish or acquire named a referrer that has a fence
    /// (ADR 0113 §2.7): the typed half of
    /// [`RuntimeErrorCode::ArtifactReferrerEnded`].
    ArtifactReferrerEnded {
        referrer: Box<crate::artifact_referrer::ArtifactReferrer>,
    },
    /// The typed half of [`RuntimeErrorCode::RuntimeEffectGroupChildUnroutable`].
    EffectGroupChildUnroutable {
        missing: GroupChildCapability,
    },
    /// The recorded model key this worker could not bind (FIG-4404).
    /// A deployment serving the key repairs this retryable cause.
    LlmProfileUnavailable {
        profile_key: Box<crate::LlmProfileKey>,
    },
    /// A tool call passed the session's recorded `max_tool_calls`
    /// (FIG-4546): the typed half of
    /// [`RuntimeErrorCode::MaxToolCallsExceeded`].
    MaxToolCallsExceeded {
        exceeded: Box<crate::ToolCallLimitExceeded>,
    },
    /// Why a run's shape was refused (FIG-4652): the typed half of
    /// [`RuntimeErrorCode::RunShapeRefused`] and, for a refused reasoning,
    /// of [`RuntimeErrorCode::ReasoningRefused`].
    RunShapeRefused {
        refusal: Box<crate::run_spec::RunShapeRefusal>,
    },
    /// Why the config a session's creation stated was refused (FIG-4652):
    /// the typed half of [`RuntimeErrorCode::SessionConfigRefused`].
    ConfigRefused {
        refusal: Box<crate::config_transaction::ConfigRefusal>,
    },
}

/// The record kind and diagnostic retained when durable data cannot be decoded.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct StoredDataCorruption {
    pub record_kind: String,
    pub message: String,
}

impl RuntimeErrorCause {
    /// The fenced referrer `cause` names, if it is an ended-referrer refusal.
    #[must_use]
    pub fn ended_referrer(
        cause: Option<&Self>,
    ) -> Option<&crate::artifact_referrer::ArtifactReferrer> {
        match cause {
            Some(Self::ArtifactReferrerEnded { referrer }) => Some(referrer),
            _ => None,
        }
    }
}

/// A capability a deployment needs to execute an effect-group child, and
/// whose absence is a fact of the deployment's wiring rather than of one
/// attempt (FIG-4550).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GroupChildCapability {
    /// The deployment's builder of a tool child's dispatch context when the
    /// child's opener is not live where it runs.
    ToolChildContextSource,
    /// The resolver that maps a recorded group child to the code that runs
    /// it: the host serves the lane with none registered.
    GroupExecutors,
}

impl GroupChildCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolChildContextSource => "tool_child_context_source",
            Self::GroupExecutors => "group_executors",
        }
    }
}

impl std::fmt::Display for GroupChildCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl RuntimeEffectControllerError {
    /// The typed refusal of a Lash object whose compatibility record refuses
    /// this build: terminal by its cause on every path that carries it.
    #[must_use]
    pub fn compat_refused(refusal: crate::compat::CompatRefusal) -> Self {
        let mut error = Self::new(
            RuntimeErrorCode::EngineObjectStateFormatUnsupported,
            refusal.to_string(),
        );
        error.cause = Some(RuntimeErrorCause::Compat {
            refusal: Box::new(refusal),
        });
        error
    }

    /// The compatibility refusal this error carries, if it is one.
    #[must_use]
    pub fn compat_refusal(&self) -> Option<&crate::compat::CompatRefusal> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::Compat { refusal } => Some(refusal),
            _ => None,
        }
    }

    /// The typed refusal of an effect-group child that the deployment serving
    /// its lane can never execute, naming the capability it lacks (FIG-4550).
    pub fn group_child_unroutable(
        missing: GroupChildCapability,
        message: impl Into<String>,
    ) -> Self {
        let mut error = Self::new(RuntimeErrorCode::RuntimeEffectGroupChildUnroutable, message);
        error.cause = Some(RuntimeErrorCause::EffectGroupChildUnroutable { missing });
        error
    }
}

impl RuntimeErrorCause {
    pub fn plugin_failure_class(&self) -> Option<lash_sansio::PluginFailureClass> {
        match self {
            Self::PluginOperation { failure } => Some(failure.class),
            Self::PluginHooks { causes } => Some(
                causes
                    .iter()
                    .map(|cause| cause.failure.class)
                    .min_by_key(|class| class.precedence())
                    .unwrap_or(lash_sansio::PluginFailureClass::Terminal),
            ),
            _ => None,
        }
    }
}

impl From<super::RuntimeError> for lash_sansio::PluginOperationFailure {
    fn from(error: super::RuntimeError) -> Self {
        if let Some(RuntimeErrorCause::PluginOperation { failure }) = &error.cause {
            return *failure.clone();
        }
        let class = if error.turn_failure_cause() == super::TurnFailureCause::Parked {
            lash_sansio::PluginFailureClass::Parked
        } else if error.is_retryable() {
            lash_sansio::PluginFailureClass::Retryable
        } else if error.is_terminal() {
            lash_sansio::PluginFailureClass::Terminal
        } else {
            lash_sansio::PluginFailureClass::Redrivable
        };
        match serde_json::to_value(&error) {
            Ok(payload) => Self {
                error_type: "lash.runtime".into(),
                error_version: std::num::NonZeroU32::MIN,
                payload,
                class,
                code: lash_sansio::FailureCode::from(&error.code),
                message: error.message,
                origin: None,
            },
            Err(source) => Self {
                error_type: "lash.encoding".into(),
                error_version: std::num::NonZeroU32::MIN,
                payload: serde_json::json!({"message": source.to_string()}),
                class: lash_sansio::PluginFailureClass::Terminal,
                code: lash_sansio::FailureCode::from(
                    &super::RuntimeErrorCode::RecordEncodingFailed,
                ),
                message: source.to_string(),
                origin: None,
            },
        }
    }
}
