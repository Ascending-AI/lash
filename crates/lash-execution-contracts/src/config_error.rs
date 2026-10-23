use serde::Serialize;
/// A config registration that cannot stand.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigRegistrationError {
    /// A factory id must be unique, even if its factory registers no config.
    #[error("plugin id `{id}` is registered twice")]
    DuplicatePluginId { id: String },
    #[error("plugin `{plugin_id}` registered a second config owner")]
    DuplicateOwner { plugin_id: String },
    #[error("plugin `{plugin_id}` registered config command `{command}` twice")]
    DuplicateCommand { plugin_id: String, command: String },
    #[error(
        "plugin `{plugin_id}` registered config command `{command}` for an owner type it did not \
         register"
    )]
    ForeignCommand { plugin_id: String, command: String },
    #[error("no plugin may own the reserved config owner id `{owner}`")]
    ReservedOwner { owner: String },
    #[error(
        "plugin `{plugin_id}` registered its config owner under the run options key of `{key}`"
    )]
    ForeignRunOptionsKey { plugin_id: String, key: String },
}

/// Why the core owner refused a candidate.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoreConfigRefusal {
    /// The host's models register no model under the key a [`SetLlmProfile`]
    /// named.
    UnknownLlmProfile { key: crate::LlmProfileKey },
    /// The candidate's reasoning does not fit the capability its recorded
    /// model declares.
    ReasoningRefused {
        key: crate::LlmProfileKey,
        reasoning: crate::ReasoningSelection,
        category: lash_core_llm::provider::LlmProfileEffortValidationCategory,
        message: String,
    },
    /// A charge-safety policy that accepts more unsafe retries than Lash admits.
    UnsafeRetriesAboveCeiling { requested: u8, ceiling: u8 },
    /// The candidate selects a reasoning but records no model to run it
    /// with.
    ReasoningWithoutLlmProfile {
        reasoning: crate::ReasoningSelection,
    },
    /// A [`SetPromptPlan`] states a plan that orders or places a section
    /// twice, or a per-section limit above its total.
    PromptPlanRefused {
        error: crate::prompt_sections::PromptPlanError,
    },
    /// A creation states no policy. Creating a session is explicit: no
    /// other session supplies the turn budget, tool-call limit or model a
    /// request leaves unstated (ADR 0134).
    PolicyUnstated,
    /// The tools the candidate's providers advertise do not build into one
    /// surface (S4): two share a model-facing name, or a tool id.
    ToolSurfaceRefused { refusal: Box<crate::SurfaceRefusal> },
    EnvironmentAssemblyRefused {
        refusal: Box<lash_core_store::environment::AssemblyRefusal>,
    },
}

impl std::fmt::Display for CoreConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLlmProfile { key } => {
                write!(formatter, "the host's models register no model `{key}`")
            }
            Self::ReasoningRefused { message, .. } => formatter.write_str(message),
            Self::ToolSurfaceRefused { refusal } => write!(formatter, "{refusal}"),
            Self::EnvironmentAssemblyRefused { refusal } => write!(formatter, "{refusal}"),
            Self::ReasoningWithoutLlmProfile { reasoning } => write!(
                formatter,
                "reasoning {reasoning:?} needs a model, and the session records none"
            ),
            Self::PromptPlanRefused { error } => write!(formatter, "prompt plan refused: {error}"),
            Self::PolicyUnstated => formatter.write_str(
                "the create request states no policy, and no other session supplies one",
            ),
            Self::UnsafeRetriesAboveCeiling { requested, ceiling } => write!(
                formatter,
                "charge safety accepts {requested} unsafe retries, above the ceiling of {ceiling}"
            ),
        }
    }
}

impl std::error::Error for CoreConfigRefusal {}

impl CoreConfigRefusal {
    pub fn environment_refusal(
        refusal: lash_core_store::environment::AssemblyRefusal,
    ) -> lash_core_store::config_transaction::ConfigRefusal {
        lash_core_store::config_transaction::ConfigRefusal::by_owner(
            lash_core_store::config_transaction::CORE_CONFIG_OWNER,
            lash_core_store::config_transaction::RefusalSite::Candidate,
            &Self::EnvironmentAssemblyRefused {
                refusal: Box::new(refusal),
            },
        )
    }

    /// The core owner's refusal of a candidate whose providers' tools do
    /// not build into one surface.
    pub fn surface_refusal(
        refusal: lash_core_store::tool_surface::SurfaceRefusal,
    ) -> lash_core_store::config_transaction::ConfigRefusal {
        lash_core_store::config_transaction::ConfigRefusal::by_owner(
            lash_core_store::config_transaction::CORE_CONFIG_OWNER,
            lash_core_store::config_transaction::RefusalSite::Candidate,
            &CoreConfigRefusal::ToolSurfaceRefused {
                refusal: Box::new(refusal),
            },
        )
    }
}
