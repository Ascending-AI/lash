use lash_protocol_types::{CellRecord, TerminationMode};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmAttachmentRef {
    /// Attachment occurrence id within the message.
    pub id: String,
    pub reference: lash_protocol_types::AttachmentRef,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmImageRef {
    pub id: String,
    pub media_type: lash_protocol_types::MediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RlmImageRef {
    /// Build the model-visible image metadata from its durable attachment reference.
    pub fn from_attachment(attachment: &lash_protocol_types::AttachmentRef) -> Self {
        let (width, height) = match attachment.type_metadata.as_ref() {
            Some(lash_protocol_types::AttachmentTypeMetadata::Image { width, height }) => {
                (*width, *height)
            }
            None => (None, None),
        };
        Self {
            id: attachment.id.to_string(),
            media_type: attachment.media_type.clone(),
            width,
            height,
            bytes: attachment.byte_len as usize,
            label: attachment.label.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmGlobalsPatchPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub set_default: serde_json::Map<String, serde_json::Value>,
}

impl RlmGlobalsPatchPluginBody {
    pub fn is_empty(&self) -> bool {
        self.set_default.is_empty()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum RlmProtocolEvent {
    /// One executed cell: the record the transcript returns, shared with it.
    RlmTrajectoryEntry(std::sync::Arc<CellRecord>),
    RlmGlobalsPatch(RlmGlobalsPatchPluginBody),
    RlmSeed(RlmSeedPluginBody),
    RlmDiagnostic(RlmDiagnosticEvent),
}

/// The protocol step an [`RlmDiagnosticEvent`] reports on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmDiagnosticPhase {
    /// A program extracted from a text-protocol reply.
    LlmExtraction,
    /// A program extracted from a native tool-call reply.
    NativeExtraction,
    /// The turn's no-progress budget ran out.
    NoProgressBudget,
    /// Bindings that degraded while a projection was rehydrated.
    ProjectionRehydration,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RlmDiagnosticEvent {
    pub phase: RlmDiagnosticPhase,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// RLM protocol session config. Natural turns finish with prose-only model
/// responses or a declared control call (`control.finish`). Programmatic
/// turns can require a control call ([`TerminationMode::TerminalRequired`]).
/// The termination is the session's: it is recorded here, at creation, and
/// no run states it again. The type of a turn's final value is the one its
/// finishing tool declares, never a session setting.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmCreateExtras {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RlmRenderPatch>,
    /// Session-wide termination requirement. Absence is the `Natural` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationMode>,
    /// The JSON Schema `control.finish` takes its value under, so a turn's
    /// final value has this type. Absence is any JSON value. A run may state
    /// another for itself ([`RlmTurnOptions::finish_value_schema`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_value_schema: Option<serde_json::Value>,
}

/// The RLM options a *single turn* may state again (FIG-1979): its render
/// preferences and the schema of its final value.
///
/// There is currently no language choice to state: TypeScript is the only shipped RLM dialect
/// and nothing — a turn bag, a session bag, a create contract — names one.
/// The termination is the session's, fixed at creation. The type of the
/// final value is the finishing tool's: a run that wants another shape for
/// `control.finish` states it here, for that run alone.
///
/// These are the RLM owner's run options: the owner applies each stated
/// field over the session's recorded value, and an unstated one leaves it
/// alone. Nothing else is a field, so a payload that names a session pin
/// does not decode (FIG-4652).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmTurnOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RlmRenderPatch>,
    /// The JSON Schema this run's `control.finish` takes its value under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_value_schema: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmRenderPatch {
    #[serde(
        default,
        skip_serializing_if = "lash_render::RenderParamsPatch::is_empty"
    )]
    pub print: lash_render::RenderParamsPatch,
    #[serde(
        default,
        skip_serializing_if = "lash_render::RenderParamsPatch::is_empty"
    )]
    pub preview: lash_render::RenderParamsPatch,
}

/// The durable RLM facts a session has recorded, read as recorded.
///
/// Every field is `Option`-shaped on purpose: `None` means the session has
/// stated nothing yet, which is a different answer from the value the default
/// resolves to. A host that cannot tell those apart has to peek at raw payload
/// keys to label a fresh session honestly — the hack this type replaces.
///
/// The facts are recorded once, when the session is created (FIG-4379): no
/// config command or run changes them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmSessionConfig {
    pub termination: Option<TerminationMode>,
}

impl RlmSessionConfig {
    /// An empty request, stating nothing.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn termination(mut self, termination: TerminationMode) -> Self {
        self.termination = Some(termination);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.termination.is_none()
    }
}

impl From<&RlmCreateExtras> for RlmSessionConfig {
    fn from(extras: &RlmCreateExtras) -> Self {
        Self {
            termination: extras.termination,
        }
    }
}

impl From<&RlmSessionConfig> for RlmCreateExtras {
    fn from(config: &RlmSessionConfig) -> Self {
        Self {
            render: None,
            termination: config.termination,
            finish_value_schema: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmSeedPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub globals: serde_json::Map<String, serde_json::Value>,
}

impl RlmSeedPluginBody {
    pub fn is_empty(&self) -> bool {
        self.globals.is_empty()
    }
}
