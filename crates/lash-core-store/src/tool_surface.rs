//! Tool-surface vocabulary shared by sessions and processes (FIG-6074).
//!
//! Which tools exist is each tool provider's own configuration. A call is
//! matched once, the match is recorded, and the call executes on exactly
//! that match:
//!
//! - a session run records the [`RecordedSurface`] its model calls are
//!   admitted under, each tool with its [`OperationLink`] and its whole
//!   definition as the model was shown it;
//! - a process env links each effect its document performs to a tool by
//!   one [`OperationLink`];
//! - a call path no surface or link matches is resolved once by the
//!   surface's deferred provider, and the [`DeferredMatch`] it records, an
//!   [`OperationLink`] retaining the matched definition, is never resolved
//!   again.
//!
//! Every pinned call is judged by one [`judge`]: the live tool's contract id
//! must equal the pinned one, and nothing else about the live tool is
//! compared (OP-LINK-ONE).

use std::collections::BTreeMap;

use lash_core_ids::FormatVersion;
use serde::{Deserialize, Serialize};

use crate::{ToolDefinition, ToolId};
use lash_protocol_types::QualifiedOperation;

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
    #[error("validation error: {0}")]
    Validation(String),
}

/// One tool of a recorded surface: its link, and its whole definition as
/// the model was shown it, kept for prompt replay and as a round's pin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceEntry {
    pub link: OperationLink,
    pub definition: ToolDefinition,
}

impl SurfaceEntry {
    /// The entry of `definition`, advertised by `provider` with the codec
    /// `payload` it states for the tool.
    pub fn advertised(
        provider: ProviderKey,
        definition: ToolDefinition,
        payload: Option<ExecutionBinding>,
    ) -> Self {
        Self {
            link: OperationLink::advertised(provider, &definition, payload),
            definition,
        }
    }

    /// The tool this entry offers.
    pub fn tool(&self) -> &ToolId {
        &self.link.tool
    }

    /// The provider the entry's tool is matched to.
    pub fn provider(&self) -> &ProviderKey {
        &self.link.provider
    }
}

/// The tool surface a session run recorded: each tool, by its operation,
/// with the provider it is matched to, and the deferred provider that
/// resolves call paths the surface does not offer. A tool's model-facing
/// name is derived from its operation. A content-addressed TurnContent node
/// of its run; no hash of it is stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedSurface {
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "lash_protocol_types::operation_keys"
    )]
    tools: BTreeMap<QualifiedOperation, SurfaceEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deferred: Option<ProviderKey>,
}

/// Two tools a surface would offer as one operation: refused, naming both
/// tools and both providers. The host fixes it by reconfiguring one
/// provider.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[error(
    "operation `{operation}` is advertised by tool `{first_tool}` of provider `{first_provider}` \
     and by tool `{second_tool}` of provider `{second_provider}`; reconfigure one provider"
)]
pub struct SurfaceCollision {
    pub operation: String,
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
    /// Two entries share an operation, or a tool id.
    #[expect(
        clippy::result_large_err,
        reason = "a refusal is built once, at surface assembly; it is boxed where it is carried"
    )]
    pub fn new(
        entries: impl IntoIterator<Item = SurfaceEntry>,
        deferred: Option<ProviderKey>,
    ) -> Result<Self, SurfaceRefusal> {
        let mut tools = BTreeMap::<QualifiedOperation, SurfaceEntry>::new();
        let mut ids = BTreeMap::<ToolId, ProviderKey>::new();
        for entry in entries {
            if let Some(first) = ids.get(entry.tool()) {
                return Err(SurfaceRefusal::DuplicateTool(SurfaceDuplicateTool {
                    tool: entry.tool().clone(),
                    first_provider: first.clone(),
                    second_provider: entry.provider().clone(),
                }));
            }
            let operation = entry.definition.manifest.operation.clone();
            if let Some(first) = tools.get(&operation) {
                return Err(SurfaceRefusal::Collision(SurfaceCollision {
                    operation: operation.effect_name().to_string(),
                    first_tool: first.tool().clone(),
                    first_provider: first.provider().clone(),
                    second_tool: entry.tool().clone(),
                    second_provider: entry.provider().clone(),
                }));
            }
            ids.insert(entry.tool().clone(), entry.provider().clone());
            tools.insert(operation, entry);
        }
        Ok(Self { tools, deferred })
    }

    /// The offered tools, by operation.
    pub fn tools(&self) -> &BTreeMap<QualifiedOperation, SurfaceEntry> {
        &self.tools
    }

    /// The entry offered as `operation`.
    pub fn entry(&self, operation: &QualifiedOperation) -> Option<&SurfaceEntry> {
        self.tools.get(operation)
    }

    /// The entry a Standard-mode model calls as `name`, its derived
    /// model-facing name ([`lash_protocol_types::ToolManifest::standard_name`]).
    pub fn entry_named(&self, name: &str) -> Option<&SurfaceEntry> {
        self.tools
            .values()
            .find(|entry| entry.definition.manifest.standard_name() == name)
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

    /// The offered definitions, in operation order.
    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values().map(|entry| &entry.definition)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.deferred.is_none()
    }
}

/// The host's execution and retry contract for a dispatch grant's
/// discovery, recorded with the grant in the process environment before its
/// first resolution. Resolution and the matched tool have independent
/// bounds. As with ordinary tool bounds,
/// there is no runtime-selected ceiling; enclosing deadlines still clip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredResolutionPolicy {
    pub execution: std::time::Duration,
    pub execution_policy: lash_protocol_types::ExecutionPolicy,
}

impl Default for DeferredResolutionPolicy {
    /// Sixty seconds, with three repeatable attempts and exponential backoff
    /// from 100 ms up to 1,000 ms.
    fn default() -> Self {
        Self {
            execution: std::time::Duration::from_secs(60),
            execution_policy: lash_protocol_types::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::MIN.saturating_add(2),
                100,
                1_000,
            ),
        }
    }
}

/// Where a linked operation's tool comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkSource {
    /// The link's provider advertises the tool.
    Advertised,
    /// A committed deferred match: the tool is served from this retained
    /// definition by the link's provider, which need not advertise it, and
    /// is never resolved again.
    Resolved { definition: Box<ToolDefinition> },
    /// A dispatch grant: the host-named tool `definition`, which no
    /// provider serves. Its call is resolved once through the link's
    /// provider, the grant's deferred provider, under the link's policy, at
    /// the call path its host's transform names, then runs on the recorded
    /// match.
    Dispatch { definition: Box<ToolDefinition> },
}

/// The one pinned record of how a run's operation is served: in session
/// surfaces, deferred matches and grants, and process envs alike.
///
/// It holds no contract id: the pin is the performing code's requirement,
/// or for a model round its recorded definition's ([`judge`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationLink {
    pub tool: ToolId,
    pub provider: ProviderKey,
    /// The execution policy captured from the runner's manifest at link
    /// time: the only policy a call through this link is admitted under.
    pub policy: lash_protocol_types::ExecutionPolicy,
    #[serde(default, skip_serializing_if = "is_default_projection")]
    pub projection: lash_protocol_types::ToolArgumentProjectionPolicy,
    /// The runner's codec payload, read back from the call's context and
    /// never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ExecutionBinding>,
    pub source: LinkSource,
}

impl OperationLink {
    /// The link of `definition`, which `provider` advertises, with the codec
    /// `payload` the provider states for it.
    pub fn advertised(
        provider: ProviderKey,
        definition: &ToolDefinition,
        payload: Option<ExecutionBinding>,
    ) -> Self {
        Self {
            tool: definition.manifest.id.clone(),
            provider,
            policy: definition.manifest.execution_policy,
            projection: definition.manifest.argument_projection.clone(),
            payload,
            source: LinkSource::Advertised,
        }
    }

    /// The link of the tool a committed deferred match resolved to
    /// `definition` with the codec `payload`, served by `provider`.
    pub fn resolved(
        provider: ProviderKey,
        definition: ToolDefinition,
        payload: Option<ExecutionBinding>,
    ) -> Self {
        Self {
            tool: definition.manifest.id.clone(),
            provider,
            policy: definition.manifest.execution_policy,
            projection: definition.manifest.argument_projection.clone(),
            payload,
            source: LinkSource::Resolved {
                definition: Box::new(definition),
            },
        }
    }

    /// The dispatch grant of `definition`, a host-named tool, through the
    /// deferred provider an env component declares as `deferred`, under
    /// `policy`: the tool's call is the resolution, bounded and retried as
    /// `policy` says, whatever `definition` declares.
    pub fn dispatch(
        deferred: ProviderKey,
        definition: ToolDefinition,
        policy: DeferredResolutionPolicy,
    ) -> Self {
        let definition = definition
            .with_execution(policy.execution)
            .with_execution_policy(policy.execution_policy);
        Self {
            tool: definition.manifest.id.clone(),
            provider: deferred,
            policy: policy.execution_policy,
            projection: definition.manifest.argument_projection.clone(),
            payload: None,
            source: LinkSource::Dispatch {
                definition: Box::new(definition),
            },
        }
    }

    /// The definition a committed deferred match retains, for a
    /// [`LinkSource::Resolved`] link.
    pub fn resolution(&self) -> Option<&ToolDefinition> {
        match &self.source {
            LinkSource::Resolved { definition } => Some(definition),
            LinkSource::Advertised | LinkSource::Dispatch { .. } => None,
        }
    }

    /// Whether this link is a dispatch grant ([`LinkSource::Dispatch`]):
    /// its call is a resolution, never a body of its own.
    pub fn dispatches(&self) -> bool {
        matches!(self.source, LinkSource::Dispatch { .. })
    }

    /// The definition a call through this link runs as: `served`, what the
    /// runner serves now, or the retained or granted definition the link
    /// itself serves, admitted under the link's policy and argument
    /// projection rather than the served manifest's (C1).
    pub fn admitted(&self, served: Option<&ToolDefinition>) -> Option<ToolDefinition> {
        let mut definition = match &self.source {
            LinkSource::Resolved { definition } | LinkSource::Dispatch { definition } => {
                (**definition).clone()
            }
            LinkSource::Advertised => served?.clone(),
        };
        definition.manifest.execution_policy = self.policy;
        definition.manifest.argument_projection = self.projection.clone();
        Some(definition)
    }

    /// What the link requires of a node that runs a call through it
    /// (FIG-6985): its provider, by key, in `requirements.providers`, and
    /// the codec its payload is read by, with the payload's format
    /// version, in `requirements.codecs`.
    pub fn require(&self, requirements: &mut lash_kernel_doc::ExecutionRequirements) {
        requirements.providers.insert(provider_key(&self.provider));
        if let Some(payload) = &self.payload {
            requirements
                .codecs
                .insert((provider_key(&payload.codec), payload.format.get()));
        }
    }

    /// The callbacks a call through this link runs on: its provider, then
    /// its payload's codec when it carries one.
    pub fn callbacks(
        &self,
    ) -> impl Iterator<Item = crate::store::plugin_writers::PluginCallbackIdentity> + '_ {
        std::iter::once(self.provider.callback_identity()).chain(
            self.payload
                .iter()
                .map(|payload| payload.codec.callback_identity()),
        )
    }
}

fn is_default_projection(projection: &lash_protocol_types::ToolArgumentProjectionPolicy) -> bool {
    projection.is_materialize_projected_values()
}

/// How the live runner serves a pinned call ([`judge`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judgement {
    /// The runner serves the linked tool under the pinned contract.
    Served,
    /// The linked tool is not served here.
    Missing,
    /// The runner serves the linked tool under another contract.
    ContractChanged {
        pinned: lash_kernel_doc::ContractId,
        live: lash_kernel_doc::ContractId,
    },
}

/// The one dispatch check (OP-CONTRACT-PIN, OP-LINK-ONE): whether the call
/// `link` serves, pinned to `pinned`, may run against `live`, what its
/// provider serves the linked tool as now. A resolved or dispatch link
/// serves its own definition, so `live` is read only for an advertised one.
/// The live contract id must equal the pinned one; nothing else about the
/// live tool is compared.
pub fn judge(
    pinned: &lash_kernel_doc::ContractId,
    link: &OperationLink,
    live: Option<&ToolDefinition>,
) -> Judgement {
    let Some(served) = link
        .admitted(live)
        .filter(|served| served.manifest.id == link.tool)
    else {
        return Judgement::Missing;
    };
    let live = served.contract_id();
    if live == *pinned {
        Judgement::Served
    } else {
        Judgement::ContractChanged {
            pinned: pinned.clone(),
            live,
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
    /// The path resolved to a tool its provider executes: a
    /// [`LinkSource::Resolved`] link.
    Resolved { link: OperationLink },
    /// The resolver refused the path.
    Refused { reason: String },
    /// No tool is available for the path, with an optional host note.
    Unavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl DeferredOutcome {
    /// The path resolved to `definition`, executed by `provider` with the
    /// codec `payload`.
    pub fn resolved(
        provider: ProviderKey,
        definition: ToolDefinition,
        payload: Option<ExecutionBinding>,
    ) -> Self {
        Self::Resolved {
            link: OperationLink::resolved(provider, definition, payload),
        }
    }
}

impl DeferredMatch {
    /// The resolved tool's link, when the path resolved.
    pub fn resolved(&self) -> Option<&OperationLink> {
        match &self.outcome {
            DeferredOutcome::Resolved { link } => Some(link),
            DeferredOutcome::Refused { .. } | DeferredOutcome::Unavailable { .. } => None,
        }
    }

    /// The resolved tool's retained definition, when the path resolved.
    pub fn definition(&self) -> Option<&ToolDefinition> {
        self.resolved().and_then(OperationLink::resolution)
    }
}

/// A provider's key as requirements name it: the plugin that registers it
/// and its key there, whichever registry serves it.
#[must_use]
pub fn provider_key(provider: &ProviderKey) -> String {
    format!("{}/{}", provider.owner(), provider.key())
}
