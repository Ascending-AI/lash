//! Tool-surface vocabulary shared by sessions and processes (FIG-6074).
//!
//! Which tools exist is each tool provider's own configuration. A call is
//! matched once, the match is recorded, and the call executes on exactly
//! that match:
//!
//! - a session run records the [`RecordedSurface`] its model calls are
//!   admitted under, each tool with the provider it is matched to and its
//!   whole definition;
//! - a process env links each effect its document performs to a tool
//!   ([`EffectLink`]), with the dispatch surface the link was checked
//!   against ([`ProcessSurface`]);
//! - a call path no surface or link matches is resolved once by the
//!   surface's deferred provider, and the [`DeferredMatch`] it records is
//!   never resolved again.

use std::collections::BTreeMap;

use lash_core_ids::FormatVersion;
use serde::{Deserialize, Serialize};

use crate::{ToolDefinition, ToolId};

/// The static, author-declared key a tool provider is registered under: the
/// identity of its owning plugin or process component and the key it
/// declared. A provider's existence never depends on config.
pub type ProviderKey = crate::plugin_registrations::ToolProviderHandle;

/// A refused change to a session's registered tool providers or the surface
/// they advertise.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReconfigureError {
    /// A tool namespace root would hide a top-level session binding.
    #[error(
        "tool namespace `{root}` conflicts with top-level binding `{binding}`; rename `{binding}` to `{binding}_` before offering these tools"
    )]
    ToolNamespaceCollision { root: String, binding: String },
    #[error("validation error: {0}")]
    Validation(String),
}

/// One tool of a recorded surface: the tool, the provider it is matched to
/// and its whole definition as the run was offered it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceEntry {
    pub provider: ProviderKey,
    pub definition: ToolDefinition,
}

impl SurfaceEntry {
    /// The tool this entry offers.
    pub fn tool(&self) -> &ToolId {
        &self.definition.manifest.id
    }
}

/// The tool surface a session run recorded: each model-facing tool name
/// with the tool and provider it is matched to, and the deferred provider
/// that resolves call paths the surface does not offer. A content-addressed
/// TurnContent node of its run; no hash of it is stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedSurface {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tools: BTreeMap<String, SurfaceEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deferred: Option<ProviderKey>,
}

/// Two tools a surface would offer under one model-facing name: refused,
/// naming both tools and both providers. The host fixes it by reconfiguring
/// one provider.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[error(
    "tool name `{name}` is advertised by tool `{first_tool}` of provider `{first_provider}` \
     and by tool `{second_tool}` of provider `{second_provider}`; reconfigure one provider"
)]
pub struct SurfaceCollision {
    pub name: String,
    pub first_tool: ToolId,
    pub first_provider: ProviderKey,
    pub second_tool: ToolId,
    pub second_provider: ProviderKey,
}

/// A surface entry whose tool id another entry already offers.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[error(
    "tool id `{tool}` is advertised by provider `{first_provider}` and by provider \
     `{second_provider}`; a tool id is unique within a surface"
)]
pub struct SurfaceDuplicateTool {
    pub tool: ToolId,
    pub first_provider: ProviderKey,
    pub second_provider: ProviderKey,
}

/// Why a surface could not be built.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SurfaceRefusal {
    #[error(transparent)]
    Collision(SurfaceCollision),
    #[error(transparent)]
    DuplicateTool(SurfaceDuplicateTool),
}

impl RecordedSurface {
    /// The surface of `entries`, each advertised by its provider, with
    /// `deferred` as its deferred provider.
    ///
    /// # Errors
    ///
    /// Two entries share a model-facing name, or a tool id.
    #[expect(
        clippy::result_large_err,
        reason = "a refusal is built once, at surface assembly; it is boxed where it is carried"
    )]
    pub fn new(
        entries: impl IntoIterator<Item = SurfaceEntry>,
        deferred: Option<ProviderKey>,
    ) -> Result<Self, SurfaceRefusal> {
        let mut tools = BTreeMap::<String, SurfaceEntry>::new();
        let mut ids = BTreeMap::<ToolId, ProviderKey>::new();
        for entry in entries {
            if let Some(first) = ids.get(entry.tool()) {
                return Err(SurfaceRefusal::DuplicateTool(SurfaceDuplicateTool {
                    tool: entry.tool().clone(),
                    first_provider: first.clone(),
                    second_provider: entry.provider.clone(),
                }));
            }
            let name = entry.definition.manifest.name.clone();
            if let Some(first) = tools.get(&name) {
                return Err(SurfaceRefusal::Collision(SurfaceCollision {
                    name,
                    first_tool: first.tool().clone(),
                    first_provider: first.provider.clone(),
                    second_tool: entry.tool().clone(),
                    second_provider: entry.provider.clone(),
                }));
            }
            ids.insert(entry.tool().clone(), entry.provider.clone());
            tools.insert(name, entry);
        }
        Ok(Self { tools, deferred })
    }

    /// The offered tools, by model-facing name.
    pub fn tools(&self) -> &BTreeMap<String, SurfaceEntry> {
        &self.tools
    }

    /// The entry offered under `name`.
    pub fn entry(&self, name: &str) -> Option<&SurfaceEntry> {
        self.tools.get(name)
    }

    /// The entry that offers `tool`.
    pub fn entry_for(&self, tool: &ToolId) -> Option<&SurfaceEntry> {
        self.tools.values().find(|entry| entry.tool() == tool)
    }

    /// The surface's deferred provider, which resolves call paths the
    /// surface does not offer.
    pub fn deferred(&self) -> Option<&ProviderKey> {
        self.deferred.as_ref()
    }

    /// The offered definitions, in model-facing name order.
    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values().map(|entry| &entry.definition)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.deferred.is_none()
    }
}

/// The part of a tool's dispatch surface a process link retains: what
/// decides how a call is admitted and executed, without the session-only
/// model-facing name, binding path or presentation. A linked call runs only
/// on a node whose live tool projects to exactly this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSurface {
    pub id: ToolId,
    pub input_schema: lash_sansio::SchemaContract,
    pub output_schema: lash_sansio::SchemaContract,
    pub output_contract: lash_sansio::ToolOutputContract,
    pub declaration: lash_sansio::ToolDeclaration,
    pub execution_policy: lash_sansio::ExecutionPolicy,
    pub argument_projection: lash_sansio::ToolArgumentProjectionPolicy,
}

impl ProcessSurface {
    /// The process projection of `definition`.
    pub fn of(definition: &ToolDefinition) -> Self {
        Self::of_parts(&definition.manifest, &definition.contract)
    }

    /// The process projection of the tool `manifest` and `contract` describe.
    pub fn of_parts(manifest: &crate::ToolManifest, contract: &lash_sansio::ToolContract) -> Self {
        Self {
            id: manifest.id.clone(),
            input_schema: contract.input_schema.clone(),
            output_schema: contract.output_schema.clone(),
            output_contract: contract.output_contract.clone(),
            declaration: manifest.declaration().clone(),
            execution_policy: manifest.execution_policy,
            argument_projection: manifest.argument_projection.clone(),
        }
    }
}

/// The effect path a code document performs `manifest`'s tool under: its
/// binding's module path and operation, dotted. `None` for a tool with no
/// usable binding, which no document can perform.
pub fn effect_path_of(manifest: &crate::ToolManifest) -> Option<String> {
    let binding: lash_sansio::ToolBinding = serde_json::from_value(
        manifest
            .bindings
            .get(lash_sansio::core_support::TOOL_BINDING_KEY)?
            .clone(),
    )
    .ok()?;
    let operation = binding.operation?;
    if binding.module_path.is_empty() {
        return None;
    }
    Some(format!("{}.{operation}", binding.module_path.join(".")))
}

/// The effect path a process performs the `call_dynamic` endpoint
/// under (P6, R9).
pub const CALL_DYNAMIC_EFFECT: &str = "lash.call_dynamic";

/// The host's execution and retry contract for dynamic discovery, recorded
/// in the process environment before its first resolution. Resolution and
/// the matched tool have independent bounds. As with ordinary tool bounds,
/// there is no runtime-selected ceiling; enclosing deadlines still clip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredResolutionPolicy {
    pub execution: std::time::Duration,
    pub execution_policy: lash_sansio::ExecutionPolicy,
}

impl Default for DeferredResolutionPolicy {
    /// Sixty seconds, with three repeatable attempts and exponential backoff
    /// from 100 ms up to 1,000 ms.
    fn default() -> Self {
        Self {
            execution: std::time::Duration::from_secs(60),
            execution_policy: lash_sansio::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::MIN.saturating_add(2),
                100,
                1_000,
            ),
        }
    }
}

/// The runtime-owned `call_dynamic` signature under `policy`: an endpoint that
/// takes the call path to resolve and the target's arguments
/// ([`DynamicCall`]) and answers whatever the target answers. No provider
/// advertises it; an env links it to its deferred provider
/// ([`LinkKind::Dynamic`]). A call of it is two committed steps: its
/// resolution, whose committed outcome is the [`DeferredMatch`], then the
/// call of the matched tool. Discovery is effect-free; its admitted policy
/// controls retries and recovery.
#[expect(
    clippy::expect_used,
    reason = "the endpoint's schemas are fixed literals the schema admission accepts"
)]
pub fn call_dynamic_endpoint(policy: DeferredResolutionPolicy) -> ToolDefinition {
    use lash_sansio::ToolDefinitionBindingExt as _;
    ToolDefinition::raw(
        "lash:call_dynamic",
        "call_dynamic",
        "Calls the tool the process env's deferred provider resolves `path` to, with `args`.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "args": {},
            },
        }),
        serde_json::json!({}),
    )
    .expect("the call_dynamic endpoint's schemas admit")
    .with_tool_binding(lash_sansio::ToolBinding::new(["lash"], "call_dynamic"))
    .with_execution(policy.execution)
    .with_execution_policy(policy.execution_policy)
}

/// The input of a `call_dynamic` call: the call path to resolve, and the
/// arguments the matched tool is called with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicCall {
    pub path: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// What a linked effect is linked to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// An ordinary tool: one the link's provider advertises.
    Tool,
    /// The `call_dynamic` endpoint and its recorded resolution policy: a
    /// call it makes is resolved once, then runs on the recorded match.
    Dynamic(DeferredResolutionPolicy),
    /// A tool the declaring cell's deferred provider resolved, as its
    /// committed match recorded it: served from this retained definition
    /// and binding by the link's provider, which need not advertise it, and
    /// never resolved again.
    Resolved(Box<ResolvedLink>),
}

/// The committed resolution a [`LinkKind::Resolved`] link retains: the
/// matched tool's definition, whose process surface is the link's, and its
/// execution binding, routed to the codec's process twin with its format
/// and payload as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedLink {
    pub definition: ToolDefinition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<ExecutionBinding>,
}

/// How one effect a process performs is implemented: the tool it is linked
/// to, the provider (an env component's) that serves it, and the dispatch
/// surface the link was checked against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectLink {
    pub provider: ProviderKey,
    pub surface: ProcessSurface,
    pub kind: LinkKind,
}

impl EffectLink {
    /// The tool this effect is linked to.
    pub fn tool(&self) -> &ToolId {
        &self.surface.id
    }

    /// The link of the `call_dynamic` endpoint to the deferred provider an
    /// env component declares as `deferred`.
    pub fn dynamic(deferred: ProviderKey, policy: DeferredResolutionPolicy) -> Self {
        Self {
            provider: deferred,
            surface: ProcessSurface::of(&call_dynamic_endpoint(policy)),
            kind: LinkKind::Dynamic(policy),
        }
    }

    /// The resolution endpoint under the policy this link recorded. Only
    /// a dynamic link retaining the endpoint's dispatch surface serves it;
    /// no current provider policy participates in this check.
    pub fn dynamic_endpoint(&self) -> Option<ToolDefinition> {
        let LinkKind::Dynamic(policy) = self.kind else {
            return None;
        };
        let endpoint = call_dynamic_endpoint(policy);
        (self.surface == ProcessSurface::of(&endpoint)).then_some(endpoint)
    }

    /// The link of the tool a committed deferred match resolved to
    /// `definition` under `binding`, served by the env component provider
    /// declared as `provider`.
    pub fn resolved(
        provider: ProviderKey,
        definition: ToolDefinition,
        binding: Option<ExecutionBinding>,
    ) -> Self {
        Self {
            provider,
            surface: ProcessSurface::of(&definition),
            kind: LinkKind::Resolved(Box::new(ResolvedLink {
                definition,
                binding,
            })),
        }
    }

    /// The retained resolution of a [`LinkKind::Resolved`] link.
    pub fn resolution(&self) -> Option<&ResolvedLink> {
        match &self.kind {
            LinkKind::Resolved(resolved) => Some(resolved),
            LinkKind::Tool | LinkKind::Dynamic(_) => None,
        }
    }
}

/// A host-owned routing payload a resolved deferred match carries to its
/// provider: the provider whose declared formats read it, the format it is
/// written in and the payload. Readability is checked before the binding is
/// installed or a call is prepared, never by the provider's reader first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionBinding {
    pub codec: ProviderKey,
    pub format: FormatVersion,
    pub payload: serde_json::Value,
}

impl ExecutionBinding {
    /// Whether a codec that reads `readable` reads this binding: checked
    /// before the binding is installed or a call prepared (S8), so an
    /// unreadable binding never reaches the provider's reader.
    ///
    /// # Errors
    ///
    /// [`ExecutionBindingUnreadable`] naming the codec, the format and what
    /// it reads.
    pub fn check_readable(
        &self,
        readable: &[FormatVersion],
    ) -> Result<(), ExecutionBindingUnreadable> {
        if readable.contains(&self.format) {
            Ok(())
        } else {
            Err(ExecutionBindingUnreadable {
                codec: self.codec.clone(),
                format: self.format,
                readable: readable.to_vec(),
            })
        }
    }
}

/// A recorded execution binding its codec cannot read on this node.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error(
    "execution binding of provider `{codec}` is in format {format}, which this node's provider \
     does not read (it reads {readable:?})"
)]
pub struct ExecutionBindingUnreadable {
    pub codec: ProviderKey,
    pub format: FormatVersion,
    pub readable: Vec<FormatVersion>,
}

/// The one recorded resolution of a call path no surface or link offers.
/// It is committed before any call it admits, and never resolved again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredMatch {
    /// The call path that was resolved.
    pub requested: String,
    pub outcome: DeferredOutcome,
}

/// What a deferred resolution decided.
#[expect(
    clippy::large_enum_variant,
    reason = "a match is decoded once per resolution, and the resolved variant is the common one"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeferredOutcome {
    /// The path resolved to a tool its provider executes.
    Resolved {
        provider: ProviderKey,
        definition: ToolDefinition,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<ExecutionBinding>,
    },
    /// The resolver refused the path.
    Refused { reason: String },
    /// No tool is available for the path, with an optional host note.
    Unavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl DeferredMatch {
    /// The resolved tool's definition, when the path resolved.
    pub fn resolved(&self) -> Option<(&ProviderKey, &ToolDefinition, Option<&ExecutionBinding>)> {
        match &self.outcome {
            DeferredOutcome::Resolved {
                provider,
                definition,
                binding,
            } => Some((provider, definition, binding.as_ref())),
            DeferredOutcome::Refused { .. } | DeferredOutcome::Unavailable { .. } => None,
        }
    }
}
