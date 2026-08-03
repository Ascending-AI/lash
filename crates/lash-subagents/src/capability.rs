//! Pluggable capability model for subagents.
//!
//! A `Capability` describes how to translate an
//! `agents.spawn({ capability: "name" })` call into the complete child session
//! request. Built-in `explore` / `peer` tiers are tiny `TierCapability`
//! instances; downstream code can register arbitrary additional impls
//! (different model lookup, dynamic inheritance, config-driven surfaces, ...)
//! without touching the spawn pipeline.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::{
    CausalRef, ModelSpec, PluginOptions, SessionCreateRequest, SessionPluginSource, SessionPolicy,
    SessionSnapshot, SessionStartPoint, SessionToolAccess, SubagentSessionContext,
    facade_support::SessionSpec,
};
use lash_rlm_types::RlmTermination;
use serde_json::Value;

const MAX_SUBAGENT_DEPTH: u8 = 5;
const RECURSIVE_SUBAGENT_TOOL: &str = "spawn_agent";

pub fn default_explore_plugin_source() -> TierPluginSource {
    TierPluginSource::CurrentHostFresh
}

pub trait Capability: Send + Sync {
    fn name(&self) -> &str;
    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<SessionCreateRequest, String>;
}

/// State exposed to a `Capability` while it resolves a spawn.
pub struct SubagentSpawnContext<'a> {
    pub parent_session_id: &'a str,
    pub parent_snapshot: &'a SessionSnapshot,
    pub session_spec: &'a SessionSpec,
    pub base_tool_access: &'a SessionToolAccess,
    pub final_answer_format: lash_rlm_types::RlmFinalAnswerFormat,
    pub output_schema: Option<Value>,
    pub seed: lash_protocol_rlm::RlmSeed,
    pub parent_subagent: Option<&'a SubagentSessionContext>,
    pub caused_by: Option<CausalRef>,
}

impl SubagentSpawnContext<'_> {
    pub fn base_policy(&self) -> SessionPolicy {
        self.session_spec
            .resolve_against(&self.parent_snapshot.policy)
    }

    pub fn rlm_request(
        &self,
        capability_name: &str,
        spec: &SessionSpec,
        plugin_source: SessionPluginSource,
    ) -> Result<SessionCreateRequest, String> {
        let mut policy = self.base_policy();
        policy = spec.resolve_against(&policy);
        let termination = match self.output_schema.clone() {
            Some(schema) => RlmTermination::FinishRequired {
                schema: Some(schema),
            },
            None => RlmTermination::FinishRequired { schema: None },
        };
        let plugin_options = PluginOptions::typed(
            lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
            lash_rlm_types::RlmCreateExtras {
                termination,
                final_answer_format: Some(self.final_answer_format.clone()),
            },
        )
        .map_err(|err| format!("failed to encode rlm plugin options: {err}"))?;

        let initial_nodes = lash_protocol_rlm::rlm_seed_initial_nodes(self.seed.clone());
        let request = SessionCreateRequest::child(
            self.parent_session_id,
            SessionStartPoint::Empty,
            policy,
            plugin_options,
            "subagent",
        )
        .with_plugin_source(plugin_source)
        .with_tool_access(self.base_tool_access.clone())
        .with_initial_nodes(initial_nodes);
        self.finalize_request(request, capability_name)
    }

    pub fn finalize_request(
        &self,
        mut request: SessionCreateRequest,
        capability_name: &str,
    ) -> Result<SessionCreateRequest, String> {
        if let Some(caused_by) = self.caused_by.clone() {
            request = request.with_caused_by(caused_by);
        }
        let child_depth = self
            .parent_subagent
            .map(|parent| parent.depth.saturating_add(1))
            .unwrap_or(1);
        if child_depth > MAX_SUBAGENT_DEPTH {
            return Err(format!(
                "subagent recursion depth exceeded: max depth is {MAX_SUBAGENT_DEPTH}"
            ));
        }
        let mut tool_access = request.tool_access.clone();
        if child_depth >= MAX_SUBAGENT_DEPTH {
            tool_access
                .hidden_tools
                .insert(RECURSIVE_SUBAGENT_TOOL.to_string());
        }
        Ok(request
            .with_tool_access(tool_access)
            .with_subagent_context(SubagentSessionContext {
                parent_session_id: self.parent_session_id.to_string(),
                capability: capability_name.to_string(),
                depth: child_depth,
                max_depth: MAX_SUBAGENT_DEPTH,
            }))
    }
}

/// Fixed capability for callers that already know the exact child authority
/// they want and do not need provider-tier lookup.
pub struct StaticCapability {
    name: String,
    spec: SessionSpec,
    plugin_source: SessionPluginSource,
}

impl StaticCapability {
    pub fn new(name: impl Into<String>, spec: SessionSpec) -> Self {
        Self {
            name: name.into(),
            spec,
            plugin_source: SessionPluginSource::CurrentHostFresh,
        }
    }

    pub fn with_plugin_source(mut self, plugin_source: SessionPluginSource) -> Self {
        self.plugin_source = plugin_source;
        self
    }
}

impl Capability for StaticCapability {
    fn name(&self) -> &str {
        &self.name
    }

    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<SessionCreateRequest, String> {
        ctx.rlm_request(&self.name, &self.spec, self.plugin_source)
    }
}

/// How a tier picks plugin instances relative to the parent session.
#[derive(Clone, Copy, Debug)]
pub enum TierPluginSource {
    CurrentHostFresh,
    CurrentSessionFork,
}

/// Built-in capability that maps a tier name to: an optional explicit
/// model, plugin-source policy, and the conventional `explore` / `peer`
/// authority split. Reproduces the historic tiered model behaviour when
/// registered through [`default_registry`].
pub struct TierCapability {
    name: String,
    model: Option<ModelSpec>,
    plugin_source: TierPluginSource,
}

impl TierCapability {
    pub fn new(
        name: impl Into<String>,
        model: Option<ModelSpec>,
        plugin_source: TierPluginSource,
    ) -> Self {
        Self {
            name: name.into(),
            model,
            plugin_source,
        }
    }
}

impl Capability for TierCapability {
    fn name(&self) -> &str {
        &self.name
    }

    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<SessionCreateRequest, String> {
        let policy = ctx.base_policy();
        let model = pick_tier_model(self, &policy);
        let spec = SessionSpec::inherit().model(model);
        ctx.rlm_request(&self.name, &spec, self.plugin_source.into())
    }
}

impl From<TierPluginSource> for SessionPluginSource {
    fn from(source: TierPluginSource) -> Self {
        match source {
            TierPluginSource::CurrentHostFresh => Self::CurrentHostFresh,
            TierPluginSource::CurrentSessionFork => Self::CurrentSessionFork,
        }
    }
}

fn pick_tier_model(tier: &TierCapability, policy: &SessionPolicy) -> ModelSpec {
    if let Some(model) = &tier.model {
        return model.clone();
    }
    policy.model.clone()
}

/// Registry of named capabilities. Order is preserved so that the JSON
/// schema enum and tool-list documentation list capabilities in the order
/// they were registered.
#[derive(Default)]
pub struct CapabilityRegistry {
    capabilities: Vec<Arc<dyn Capability>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, capability: Arc<dyn Capability>) -> Self {
        self.add(capability);
        self
    }

    pub fn add(&mut self, capability: Arc<dyn Capability>) {
        // Replace if a capability with the same name is already registered,
        // so a downstream caller can override a built-in tier without
        // duplicating it.
        let name = capability.name().to_string();
        if let Some(slot) = self
            .capabilities
            .iter_mut()
            .find(|existing| existing.name() == name)
        {
            *slot = capability;
        } else {
            self.capabilities.push(capability);
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Capability>> {
        self.capabilities.iter().find(|c| c.name() == name)
    }

    pub fn names(&self) -> Vec<String> {
        self.capabilities
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }
}

/// Build the default `explore` / `peer` registry. `tier_models` supplies
/// optional explicit model overrides keyed by tier name; absent tiers fall
/// back to the provider's default agent model and then to the parent
/// session's model. The built-in `explore` tier uses
/// [`default_explore_plugin_source`] while `peer` forks the current session's
/// plugin instances.
///
/// The `explore` tier is read-only and cannot recurse: investigative
/// subagents that scan, summarise, or verify without mutating state. The
/// `peer` tier is a parallel-self with the parent's full affordances:
/// edits, recursion, anything the parent can do, in a fresh window.
pub fn default_registry(tier_models: &BTreeMap<String, ModelSpec>) -> CapabilityRegistry {
    let model_for = |name: &str| tier_models.get(name).cloned();
    let mut registry = CapabilityRegistry::new();
    registry.add(Arc::new(TierCapability::new(
        "explore",
        model_for("explore"),
        default_explore_plugin_source(),
    )));
    registry.add(Arc::new(TierCapability::new(
        "peer",
        model_for("peer"),
        TierPluginSource::CurrentSessionFork,
    )));
    registry
}
