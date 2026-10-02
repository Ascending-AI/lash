//! Pluggable capability model for subagents.
//!
//! A `Capability` describes how to translate an
//! `agents.spawn({ capability: "name" })` call into the complete child session
//! request. Built-in `explore` / `peer` tiers are tiny `TierCapability`
//! instances; downstream code can register arbitrary additional impls
//! (different model lookup, dynamic inheritance, config-driven surfaces, ...)
//! without touching the spawn pipeline.

use lash_sansio::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::{
    CausalRef, LlmProfileKey, PluginOptions, SessionCreateRequest, SessionPolicy, SessionSnapshot,
    SessionStartPoint, SessionToolAccess, SubagentSessionContext, facade_support::SessionSpec,
};
use lash_rlm_types::RlmTermination;
use serde_json::Value;

const RECURSIVE_SUBAGENT_TOOL: &str = "spawn_agent";

pub fn default_explore_plugin_source() -> ChildPluginSource {
    ChildPluginSource::CurrentHostFresh
}

/// Trusted extension point that authors a child-session request and plugin-source intent.
///
/// Built-in capabilities use [`SubagentSpawnContext::rlm_request`], which copies
/// [`SubagentSpawnContext::base_tool_access`] into the request. A custom
/// implementation may instead replace or ignore that input because the
/// returned [`SessionCreateRequest`] selects the child's authority. Spawn preparation
/// resolves [`Capability::plugin_source`] and installs its complete captured source.
/// Registering a custom
/// capability therefore trusts it to select the child's tool authority.
///
/// The model-facing spawn schema does not accept arbitrary additional
/// arguments, so a model cannot inject a tool-access record into a built-in
/// request. That closed argument shape does not impose transitive confinement:
/// it neither constrains what a custom capability authors nor guarantees that a
/// child's effective catalog is contained by its parent's catalog.
pub trait Capability: Send + Sync {
    fn name(&self) -> &str;
    /// Intent resolved into a complete plugin source by spawn preparation.
    fn plugin_source(&self) -> ChildPluginSource {
        ChildPluginSource::CurrentHostFresh
    }
    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<SessionCreateRequest, String>;
}

/// State exposed to a `Capability` while it resolves a spawn.
pub struct SubagentSpawnContext<'a> {
    pub parent_session_id: &'a SessionId,
    pub parent_snapshot: &'a SessionSnapshot,
    pub session_spec: &'a SessionSpec,
    /// Factory-configured access input used by built-in request helpers.
    ///
    /// This is not the parent's access unless the host factory explicitly
    /// copied [`lash_core::plugin::PluginSessionContext::tool_access`] into
    /// [`crate::SubagentsPluginFactory::with_tool_access`]. Even when copied,
    /// it is an access-input record rather than proof that the child's resolved
    /// tool catalog is a subset of the parent's effective catalog.
    pub base_tool_access: &'a SessionToolAccess,
    pub final_answer_format: lash_rlm_types::RlmFinalAnswerFormat,
    pub output_schema: Option<Value>,
    pub seed: lash_protocol_rlm::RlmSeed,
    pub parent_subagent: Option<&'a SubagentSessionContext>,
    pub caused_by: Option<CausalRef>,
}

impl SubagentSpawnContext<'_> {
    /// The factory's spec over the parent's recorded policy. The child copies
    /// the parent's recorded model; a model key the spec names is not minted
    /// here but carried by the request ([`Self::rlm_request`]) and minted when
    /// the child is created.
    pub fn base_policy(&self) -> Result<SessionPolicy, String> {
        resolve_recorded(self.session_spec, &self.parent_snapshot.policy)
    }

    /// Policy is resolved against the parent snapshot, while tool access is
    /// copied from [`Self::base_tool_access`]. The latter is factory input and
    /// is not derived from the parent's effective tool catalog.
    ///
    /// The child runs its parent's recorded protocol (FIG-4396). The RLM
    /// termination and final-answer format are stated only for a parent whose
    /// recorded protocol is RLM; a child of any other protocol states no RLM
    /// namespace, which its parent's plugin set has no owner for.
    pub fn rlm_request(
        &self,
        capability_name: &str,
        spec: &SessionSpec,
    ) -> Result<SessionCreateRequest, String> {
        let policy = resolve_recorded(spec, &self.base_policy()?)?;
        let model = spec
            .model
            .clone()
            .or_else(|| self.session_spec.model.clone());
        let termination = match self.output_schema.clone() {
            Some(schema) => RlmTermination::FinishRequired {
                schema: Some(schema),
            },
            None => RlmTermination::FinishRequired { schema: None },
        };
        let parent_runs_rlm = self.parent_snapshot.plugin_config.protocol_plugin_id()
            == Some(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
        let plugin_options = if parent_runs_rlm {
            PluginOptions::typed(
                lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash_rlm_types::RlmCreateExtras {
                    termination: Some(termination),
                    final_answer_format: Some(self.final_answer_format.clone()),
                    render: None,
                    // Stating nothing: the child copies its parent's recorded
                    // prompt (FIG-4588).
                    prompt: None,
                },
            )
            .map_err(|err| format!("failed to encode rlm plugin options: {err}"))?
        } else {
            PluginOptions::default()
        };
        // The child's spec states its plugin creation options (FIG-4589):
        // the capability's spec over the factory's, under what the spawn
        // itself states. A spec that states no prompt leaves it to the
        // child's lineage.
        let plugin_options = plugin_options.over(
            spec.plugin_options
                .clone()
                .over(self.session_spec.plugin_options.clone()),
        );

        let initial_nodes = lash_protocol_rlm::rlm_seed_initial_nodes(self.seed.clone());
        let request = SessionCreateRequest::child(
            self.parent_session_id,
            SessionStartPoint::Empty,
            policy,
            plugin_options,
        )
        .with_tool_access(self.base_tool_access.clone())
        .with_initial_nodes(initial_nodes);
        let request = match model {
            Some(key) => request.with_llm_profile(key),
            None => request,
        };
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
        if child_depth > SubagentSessionContext::MAX_DEPTH {
            return Err(format!(
                "subagent recursion depth exceeded: max depth is {}",
                SubagentSessionContext::MAX_DEPTH,
            ));
        }
        let mut tool_access = request.tool_access.clone();
        if child_depth >= SubagentSessionContext::MAX_DEPTH
            && !tool_access.hides(RECURSIVE_SUBAGENT_TOOL)
        {
            tool_access
                .hide_tool(RECURSIVE_SUBAGENT_TOOL)
                .map_err(|error| error.to_string())?;
        }
        Ok(request
            .with_tool_access(tool_access)
            .with_subagent_context(SubagentSessionContext {
                capability: capability_name.to_string(),
                depth: child_depth,
            }))
    }
}

/// Fixed capability for callers that already know the exact child authority
/// they want and do not need provider-tier lookup.
pub struct StaticCapability {
    name: String,
    spec: SessionSpec,
    plugin_source: ChildPluginSource,
}

impl StaticCapability {
    pub fn new(name: impl Into<String>, spec: SessionSpec) -> Self {
        Self {
            name: name.into(),
            spec,
            plugin_source: ChildPluginSource::CurrentHostFresh,
        }
    }

    pub fn with_plugin_source(mut self, plugin_source: ChildPluginSource) -> Self {
        self.plugin_source = plugin_source;
        self
    }
}

impl Capability for StaticCapability {
    fn name(&self) -> &str {
        &self.name
    }

    fn plugin_source(&self) -> ChildPluginSource {
        self.plugin_source
    }

    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<SessionCreateRequest, String> {
        ctx.rlm_request(&self.name, &self.spec)
    }
}

/// How a capability picks plugin instances before spawn captures any parent state.
#[derive(Clone, Copy, Debug)]
pub enum ChildPluginSource {
    CurrentHostFresh,
    ParentFork,
}

/// Built-in capability that maps a tier name to: an optional explicit
/// model key, plugin-source policy, and the conventional `explore` / `peer`
/// authority split. Reproduces the historic tiered model behaviour when
/// registered through [`default_registry`].
pub struct TierCapability {
    name: String,
    model: Option<LlmProfileKey>,
    plugin_source: ChildPluginSource,
}

impl TierCapability {
    pub fn new(
        name: impl Into<String>,
        model: Option<LlmProfileKey>,
        plugin_source: ChildPluginSource,
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

    fn plugin_source(&self) -> ChildPluginSource {
        self.plugin_source
    }

    fn build_session_request(
        &self,
        ctx: SubagentSpawnContext<'_>,
    ) -> Result<SessionCreateRequest, String> {
        // A tier without a key of its own inherits the parent's recorded
        // model; one with a key has it minted when the child is created.
        let spec = match &self.model {
            Some(key) => SessionSpec::inherit().model(key.clone()),
            None => SessionSpec::inherit(),
        };
        ctx.rlm_request(&self.name, &spec)
    }
}

/// `spec` over `base` with its model key left out: the key is minted when
/// the child is created, so nothing here consults a catalog. A reasoning
/// selection stated beside a key belongs to that key's model: it is carried
/// on the policy unjudged, and judged against the minted capability when the
/// child is created. Without a key it is judged here, against the model the
/// child copies.
fn resolve_recorded(spec: &SessionSpec, base: &SessionPolicy) -> Result<SessionPolicy, String> {
    let mut spec = spec.clone();
    let keyed_reasoning = match spec.model.take() {
        Some(_) => spec.reasoning.take(),
        None => None,
    };
    let mut policy = spec
        .resolve_against(base, &lash_core::EmptyLlmProfiles)
        .map_err(|error| format!("subagent session spec does not resolve: {error}"))?;
    if let Some(reasoning) = keyed_reasoning
        && let Some(model) = policy.model.as_mut()
    {
        model.reasoning = reasoning;
    }
    Ok(policy)
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

/// `tier_llm_profiles` supplies optional explicit model keys by tier name; an absent tier runs the
/// parent session's recorded model.
/// The built-in `explore` tier uses [`default_explore_plugin_source`] while `peer` forks the
/// current session's plugin instances.
///
/// The `explore` tier is read-only and cannot recurse: investigative
/// subagents that scan, summarise, or verify without mutating state. The
/// `peer` tier is a parallel-self with the parent's full affordances:
/// edits, recursion, anything the parent can do, in a fresh window.
pub fn default_registry(tier_llm_profiles: &BTreeMap<String, LlmProfileKey>) -> CapabilityRegistry {
    let model_for = |name: &str| tier_llm_profiles.get(name).cloned();
    let mut registry = CapabilityRegistry::new();
    registry.add(Arc::new(TierCapability::new(
        "explore",
        model_for("explore"),
        default_explore_plugin_source(),
    )));
    registry.add(Arc::new(TierCapability::new(
        "peer",
        model_for("peer"),
        ChildPluginSource::ParentFork,
    )));
    registry
}
