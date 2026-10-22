use super::ProcessEnvRef;
use crate::ProcessDefinitionId;
use serde::{Deserialize, Serialize};
/// Engine-owned settings a starter states for an engine start, checked by
/// the engine before the start is durable
/// ([`ProcessEngine::check_settings`](super::ProcessEngine::check_settings))
/// and read back by every run of the process (D4). Lash never chooses them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EngineSettings(serde_json::Value);

impl EngineSettings {
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    pub fn value(&self) -> &serde_json::Value {
        &self.0
    }

    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

/// What an engine process runs under, sealed with its start: the
/// environment its starter holds and the engine settings it states.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineLaunch {
    pub env: ProcessEnvRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<EngineSettings>,
    /// Explicit host pins, validated atomically with start publication.
    pub initial_resolutions: Vec<lash_core_store::resolutions::ResolutionSeed>,
}

impl EngineLaunch {
    pub fn new(env: ProcessEnvRef, settings: Option<EngineSettings>) -> Self {
        Self {
            env,
            settings,
            initial_resolutions: Vec::new(),
        }
    }
    pub fn with_initial_resolutions(
        mut self,
        seeds: Vec<lash_core_store::resolutions::ResolutionSeed>,
    ) -> Self {
        self.initial_resolutions = seeds;
        self
    }
}

/// A launch a declaring attempt holds: only
/// [`AttemptContext::declare_launch`](crate::AttemptContext::declare_launch)
/// mints one, after it held the environment under the attempt's execution
/// claim and the engine checked the settings, so no tool declares an engine
/// start whose environment it has not held (D10).
#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredLaunch(EngineLaunch);

impl DeclaredLaunch {
    pub fn launch(&self) -> &EngineLaunch {
        &self.0
    }

    pub(crate) fn into_launch(self) -> EngineLaunch {
        self.0
    }
}

/// A launch held for a start of one definition: the definition its env was
/// held and checked for is bound into it (P1), so a declaration made with it
/// starts exactly that definition.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredDefinitionLaunch {
    definition: ProcessDefinitionId,
    launch: DeclaredLaunch,
}

impl DeclaredDefinitionLaunch {
    /// The definition the launch was held for.
    pub fn definition(&self) -> &ProcessDefinitionId {
        &self.definition
    }

    pub fn launch(&self) -> &EngineLaunch {
        self.launch.launch()
    }

    pub(crate) fn into_parts(self) -> (ProcessDefinitionId, EngineLaunch) {
        (self.definition, self.launch.into_launch())
    }
}

/// A launch held for a start of one inline function: the engine and the
/// payload its env was held and checked for are bound into it (P1), so a
/// declaration made with it starts exactly that function.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredInlineLaunch {
    engine_kind: String,
    request: crate::process::engine::InlineRequest,
    launch: DeclaredLaunch,
}

impl DeclaredInlineLaunch {
    /// The engine the function is started on.
    pub fn engine_kind(&self) -> &str {
        &self.engine_kind
    }

    /// The inline payload and retained environment supplied by its starter,
    /// recorded when the start was declared (FIG-6993).
    pub fn request(&self) -> &crate::process::engine::InlineRequest {
        &self.request
    }

    pub fn launch(&self) -> &EngineLaunch {
        self.launch.launch()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (String, crate::process::engine::InlineRequest, EngineLaunch) {
        (self.engine_kind, self.request, self.launch.into_launch())
    }
}

/// Runtime construction after the launch has been held.
#[doc(hidden)]
pub fn held_launch(launch: EngineLaunch) -> DeclaredLaunch {
    DeclaredLaunch(launch)
}

/// Runtime construction after the launch has been held.
#[doc(hidden)]
pub fn held_definition_launch(
    definition: ProcessDefinitionId,
    launch: DeclaredLaunch,
) -> DeclaredDefinitionLaunch {
    DeclaredDefinitionLaunch { definition, launch }
}

/// Runtime construction after the launch has been held.
#[doc(hidden)]
pub fn held_inline_launch(
    engine_kind: String,
    request: crate::process::engine::InlineRequest,
    launch: DeclaredLaunch,
) -> DeclaredInlineLaunch {
    DeclaredInlineLaunch {
        engine_kind,
        request,
        launch,
    }
}
