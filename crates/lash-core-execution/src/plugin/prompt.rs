//! Prompt sections (FIG-5254, ADR 0133): the one contract by which plugins
//! contribute model-facing instruction text.
//!
//! A plugin registers keyed sections through [`PluginRegistrar::prompt`].
//! Each section is owned as `(plugin id, local key)`; registering one key
//! twice is refused. Plugins are trusted: any plugin may wrap any section,
//! protocol sections included, and a wrapper may prepend, append, replace or
//! omit the text. A section's wrapper chain runs in plugin registration
//! order, then declaration order within a plugin.
//!
//! The host owns the [`PromptPlan`] (session config): the section order and
//! each section's [`PromptPlacement`]. [`PromptCatalog::resolve`] applies it
//! to the registered sections for one call and records the result as a
//! [`ResolvedPromptPlan`].
//!
//! A renderer reads only a [`PromptInput`]: one committed cut of the call,
//! its own plugin's frozen namespace and its admitted config. The input
//! holds no writable service and no other plugin's state. Renderers and
//! wrappers must be repeat-safe until the call is admitted: a crash before
//! admission renders again. This is a trusted-code contract, not a sandbox.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use crate::prompt_sections::{
    AppliedPromptWrap, PlacementSource, PromptKeyError, PromptLimits, PromptPlacement, PromptPlan,
    PromptPlanError, PromptPurpose, PromptSectionId, PromptSectionKey, PromptSectionPlacement,
    PromptSnapshot, PromptSnapshotVersion, PromptTextRef, PromptWrapId, PromptWrapKey,
    RecordedSectionText, RenderedPromptSection, ResolvedPromptPlan, ResolvedPromptSection,
    ResolvedPromptWrap,
};
use crate::store::plugin_writers::PluginRevision;

use super::{PluginError, PluginRegistrar, PluginStateError};

/// A section's or wrapper's output: exact text, or an explicit omission. An
/// error is never an omission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionText {
    Text(String),
    Omit,
}

impl SectionText {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// The canonical form: empty text is an omission. Any other text is
    /// kept byte for byte.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::Text(text) if text.is_empty() => Self::Omit,
            other => other,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Omit => None,
        }
    }

    /// The recorded form of this text.
    pub fn record(&self) -> RecordedSectionText {
        match self {
            Self::Text(text) => RecordedSectionText::Text {
                text: PromptTextRef::of(text),
            },
            Self::Omit => RecordedSectionText::Omitted,
        }
    }
}

/// Why a section renderer or wrapper refused to produce text.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[serde(deny_unknown_fields)]
#[error("{message}")]
pub struct PromptRenderError {
    pub message: String,
}

impl PromptRenderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<PluginStateError> for PromptRenderError {
    fn from(error: PluginStateError) -> Self {
        Self::new(error.to_string())
    }
}

/// The renderer or wrapper a composition failure is attributed to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptRenderSite {
    Base {
        section: PromptSectionId,
        owner: PluginRevision,
    },
    Wrap {
        wrap: PromptWrapId,
        owner: PluginRevision,
        target: PromptSectionId,
        ordinal: u32,
    },
}

/// Why a call's prompt could not be composed. Composition fails closed: no
/// request is sent, and no earlier text stands in.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptCompositionError {
    /// The host's plan does not resolve against the registered sections.
    #[error("prompt plan: {error}")]
    Plan { error: PromptPlanError },
    /// A renderer or wrapper refused.
    #[error("prompt render at {site:?}: {error}")]
    Render {
        site: Box<PromptRenderSite>,
        error: PromptRenderError,
    },
    /// A renderer or wrapper panicked.
    #[error("prompt render at {site:?} panicked")]
    Panicked { site: Box<PromptRenderSite> },
    /// A base text or wrapper output exceeds the per-section limit.
    #[error("prompt render at {site:?} produced {bytes} bytes, above the limit of {limit}")]
    SectionTooLarge {
        site: Box<PromptRenderSite>,
        bytes: u64,
        limit: u32,
    },
    /// The call's final section text exceeds the total limit.
    #[error("the prompt's sections total {bytes} bytes, above the limit of {limit}")]
    TotalTooLarge { bytes: u64, limit: u32 },
    /// Rendering outlasted the call's render budget. A render still running
    /// then finishes unseen.
    #[error("prompt rendering outlasted its {budget_ms} ms budget")]
    BudgetExceeded { budget_ms: u32 },
    /// The render pool's queue of `capacity` renders is full, or the pool
    /// dropped a render without running it.
    #[error("the prompt render pool cannot take this call's renders (capacity {capacity})")]
    RenderersBusy { capacity: u32 },
}

/// A section a plugin registers: its local key, where it goes when the host
/// states no placement, and the purposes it renders for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSectionSpec {
    pub key: PromptSectionKey,
    pub default_placement: PromptPlacement,
    pub purposes: Vec<PromptPurpose>,
}

impl PromptSectionSpec {
    /// A section for turn calls, with `default_placement` unless the host
    /// places it.
    pub fn new(key: PromptSectionKey, default_placement: PromptPlacement) -> Self {
        Self {
            key,
            default_placement,
            purposes: vec![PromptPurpose::Turn],
        }
    }

    /// Render for exactly `purposes`.
    #[must_use]
    pub fn purposes(mut self, purposes: impl IntoIterator<Item = PromptPurpose>) -> Self {
        self.purposes = purposes.into_iter().collect();
        self
    }
}

/// A wrapper a plugin registers: its local key and the section it wraps,
/// which any plugin may own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptWrapSpec {
    pub key: PromptWrapKey,
    pub target: PromptSectionId,
}

impl PromptWrapSpec {
    pub fn new(key: PromptWrapKey, target: PromptSectionId) -> Self {
        Self { key, target }
    }
}

/// A section's base renderer. It runs once per new model call, over the
/// call's committed cut, and must be repeat-safe until the call is admitted.
pub trait PromptSection: Send + Sync {
    /// # Errors
    ///
    /// [`PromptRenderError`] when the section cannot render. The call is not
    /// sent.
    fn render(&self, input: &PromptInput<'_>) -> Result<SectionText, PromptRenderError>;
}

impl<F> PromptSection for F
where
    F: for<'a> Fn(&PromptInput<'a>) -> Result<SectionText, PromptRenderError> + Send + Sync,
{
    fn render(&self, input: &PromptInput<'_>) -> Result<SectionText, PromptRenderError> {
        self(input)
    }
}

/// The section a wrapper is applied to, as the call resolved it.
#[derive(Clone, Copy, Debug)]
pub struct PromptWrapTarget<'a> {
    pub section: &'a PromptSectionId,
    pub placement: PromptPlacement,
}

/// A trusted wrapper over one section. It receives the text the chain has
/// produced so far and returns the next: unchanged, prepended, appended,
/// replaced, or [`SectionText::Omit`]. It may replace an omission. It reads
/// its own plugin's namespace, never the target's.
pub trait PromptSectionWrap: Send + Sync {
    /// # Errors
    ///
    /// [`PromptRenderError`] when the wrapper cannot produce text. The call
    /// is not sent.
    fn wrap(
        &self,
        input: &PromptInput<'_>,
        target: PromptWrapTarget<'_>,
        previous: SectionText,
    ) -> Result<SectionText, PromptRenderError>;
}

impl<F> PromptSectionWrap for F
where
    F: for<'a, 'b> Fn(
            &PromptInput<'a>,
            PromptWrapTarget<'b>,
            SectionText,
        ) -> Result<SectionText, PromptRenderError>
        + Send
        + Sync,
{
    fn wrap(
        &self,
        input: &PromptInput<'_>,
        target: PromptWrapTarget<'_>,
        previous: SectionText,
    ) -> Result<SectionText, PromptRenderError> {
        self(input, target, previous)
    }
}

/// A plugin's namespace frozen at one published generation. Reads never see
/// a later publication.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommittedPluginNamespace {
    generation: u64,
    values: BTreeMap<String, serde_json::Value>,
}

impl CommittedPluginNamespace {
    pub fn new(generation: u64, values: BTreeMap<String, serde_json::Value>) -> Self {
        Self { generation, values }
    }

    /// The published generation the values were frozen at.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.values.get(key)
    }

    /// # Errors
    ///
    /// [`PluginStateError::Decode`] when the value does not decode as `T`.
    pub fn get_as<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, PluginStateError> {
        self.values
            .get(key)
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|source| PluginStateError::Decode {
                    key: key.into(),
                    message: source.to_string(),
                })
            })
            .transpose()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

/// Which call a prompt is composed for. A resend of an admitted call is an
/// attempt of the same call and never composes again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptCall {
    pub session_id: crate::SessionId,
    pub frame: Option<lash_core_store::session_identity::FrameNodeId>,
    pub run: Option<lash_sansio::RunId>,
    pub turn: Option<crate::TurnId>,
    /// The protocol iteration the call belongs to.
    pub iteration: u32,
    /// The call's ordinal within its owner: distinct for every new call.
    pub call: u32,
    pub purpose: PromptPurpose,
}

/// The tools actually offered to this call, not every installed tool.
#[derive(Clone, Debug, Default)]
pub struct OfferedTools {
    /// Tools offered as native declarations.
    pub native: Vec<String>,
    /// Tools offered as code-callable bindings.
    pub callable: Vec<String>,
    /// The pinned catalog the call offers: every tool's manifest and
    /// contract, including the ones a discovery operation keeps out of the
    /// inline surface. Empty for a call that offers no tools, such as a
    /// compaction's.
    pub catalog: Arc<crate::ToolCatalog>,
}

/// The admitted model the call runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptModel {
    pub profile: Option<crate::LlmProfileKey>,
    pub context_window_tokens: Option<u64>,
    /// The prompt usage the session last committed: the previous turn's,
    /// constant across one turn's calls. `None` before any call committed.
    pub committed_usage: Option<crate::TokenUsage>,
}

/// Facts the session's protocol derives from its committed execution state
/// for its own sections, such as the values a program has bound. The
/// protocol owns their type; the runtime carries them opaquely.
pub type ProtocolPromptFacts = Arc<dyn std::any::Any + Send + Sync>;

/// The call's projected history, measured before any section text is added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectedHistoryStats {
    pub messages: u32,
    pub estimated_tokens: u64,
}

/// One call's committed cut: everything any renderer of the call may read.
/// The runtime builds it from committed state only, after the preceding
/// outcomes and namespace publications are durable. Renderers never see it
/// whole: each gets [`PromptInput`], which exposes only its own namespace
/// and config.
#[derive(Clone)]
pub struct PromptCut {
    call: PromptCall,
    config: crate::AdmittedPluginConfig,
    session: Option<crate::SessionReadView>,
    offered: OfferedTools,
    model: PromptModel,
    history: ProjectedHistoryStats,
    namespaces: BTreeMap<String, CommittedPluginNamespace>,
    subagent: Option<crate::SubagentSessionContext>,
    protocol: Option<ProtocolPromptFacts>,
}

impl std::fmt::Debug for PromptCut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptCut")
            .field("call", &self.call)
            .field("offered", &self.offered)
            .field("model", &self.model)
            .field("history", &self.history)
            .field("subagent", &self.subagent)
            .field("protocol", &self.protocol.is_some())
            .finish_non_exhaustive()
    }
}

/// The parts of a [`PromptCut`].
#[derive(Clone, Debug)]
pub struct PromptCutParts {
    pub call: PromptCall,
    pub config: crate::AdmittedPluginConfig,
    /// A read view of the committed frame.
    pub session: Option<crate::SessionReadView>,
    pub offered: OfferedTools,
    pub model: PromptModel,
    pub history: ProjectedHistoryStats,
    /// Every plugin's namespace, frozen at the cut.
    pub namespaces: BTreeMap<String, CommittedPluginNamespace>,
}

impl PromptCut {
    pub fn new(parts: PromptCutParts) -> Self {
        let PromptCutParts {
            call,
            config,
            session,
            offered,
            model,
            history,
            namespaces,
        } = parts;
        Self {
            call,
            config,
            session,
            offered,
            model,
            history,
            namespaces,
            subagent: None,
            protocol: None,
        }
    }

    /// The session's recorded subagent authority, when it is a subagent.
    #[must_use]
    pub fn with_subagent(mut self, subagent: Option<crate::SubagentSessionContext>) -> Self {
        self.subagent = subagent;
        self
    }

    /// The protocol's committed facts for this call.
    #[must_use]
    pub fn with_protocol_facts(mut self, facts: Option<ProtocolPromptFacts>) -> Self {
        self.protocol = facts;
        self
    }

    /// The input `plugin_id`'s renderers and wrappers read.
    pub fn input_for<'a>(&'a self, plugin_id: &'a str) -> PromptInput<'a> {
        PromptInput {
            cut: self,
            plugin_id,
            state: self.namespaces.get(plugin_id).map_or_else(
                || Cow::Owned(CommittedPluginNamespace::default()),
                Cow::Borrowed,
            ),
        }
    }
}

/// What one plugin's renderer or wrapper reads: the call's committed cut,
/// its own frozen namespace and its admitted config. It holds no writable
/// service and no other plugin's state.
#[derive(Clone, Debug)]
pub struct PromptInput<'a> {
    cut: &'a PromptCut,
    plugin_id: &'a str,
    state: Cow<'a, CommittedPluginNamespace>,
}

impl PromptInput<'_> {
    pub fn plugin_id(&self) -> &str {
        self.plugin_id
    }

    pub fn call(&self) -> &PromptCall {
        &self.cut.call
    }

    pub fn purpose(&self) -> &PromptPurpose {
        &self.cut.call.purpose
    }

    /// This plugin's namespace, frozen at the cut.
    pub fn state(&self) -> &CommittedPluginNamespace {
        &self.state
    }

    /// The session config revision the call was admitted under.
    pub fn config_revision(&self) -> u64 {
        self.cut.config.revision
    }

    /// This plugin's admitted config namespace, decoded.
    ///
    /// # Errors
    ///
    /// The decode error when the namespace does not decode as `T`.
    pub fn config<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>, serde_json::Error> {
        self.cut.config.decode(self.plugin_id)
    }

    /// A read view of the committed frame, when the call has a session.
    pub fn session(&self) -> Option<&crate::SessionReadView> {
        self.cut.session.as_ref()
    }

    pub fn offered(&self) -> &OfferedTools {
        &self.cut.offered
    }

    pub fn model(&self) -> &PromptModel {
        &self.cut.model
    }

    pub fn history(&self) -> ProjectedHistoryStats {
        self.cut.history
    }

    /// The session's recorded subagent authority, when it is a subagent.
    pub fn subagent(&self) -> Option<&crate::SubagentSessionContext> {
        self.cut.subagent.as_ref()
    }

    /// The protocol's committed facts for this call, when the session's
    /// protocol derived facts of type `T`.
    pub fn protocol_facts<T: std::any::Any>(&self) -> Option<&T> {
        self.cut.protocol.as_deref()?.downcast_ref::<T>()
    }
}

#[derive(Clone)]
struct RegisteredPromptSection {
    owner: PluginRevision,
    spec: PromptSectionSpec,
    renderer: Arc<dyn PromptSection>,
}

impl RegisteredPromptSection {
    fn id(&self) -> PromptSectionId {
        PromptSectionId::new(self.owner.plugin.clone(), self.spec.key.clone())
    }
}

#[derive(Clone)]
struct RegisteredPromptWrap {
    owner: PluginRevision,
    spec: PromptWrapSpec,
    wrapper: Arc<dyn PromptSectionWrap>,
}

impl RegisteredPromptWrap {
    fn id(&self) -> PromptWrapId {
        PromptWrapId {
            owner: self.owner.plugin.clone(),
            key: self.spec.key.clone(),
        }
    }
}

/// Every section and wrapper the installed plugins registered, in plugin
/// registration order, then declaration order.
#[derive(Clone, Default)]
pub(crate) struct PromptRegistry {
    sections: Vec<RegisteredPromptSection>,
    wraps: Vec<RegisteredPromptWrap>,
}

/// Registers the plugin's prompt sections and wrappers.
pub struct PromptRegistrations<'a> {
    pub(super) reg: &'a mut PluginRegistrar,
}

impl PromptRegistrations<'_> {
    /// Register a section under this plugin's id and `spec.key`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Registration`] when this plugin already registered a
    /// section under the key.
    pub fn section(
        self,
        spec: PromptSectionSpec,
        renderer: Arc<dyn PromptSection>,
    ) -> Result<(), PluginError> {
        let owner = self.reg.owner.clone();
        let registry = &mut self.reg.contributions.prompt;
        if registry
            .sections
            .iter()
            .any(|section| section.owner.plugin == owner.plugin && section.spec.key == spec.key)
        {
            return Err(PluginError::Registration(format!(
                "duplicate prompt section `{}/{}`",
                owner.plugin, spec.key
            )));
        }
        registry.sections.push(RegisteredPromptSection {
            owner,
            spec,
            renderer,
        });
        Ok(())
    }

    /// Register a wrapper over `spec.target`, which any plugin may own. The
    /// target need not be registered yet: a call whose plan lacks it records
    /// the wrapper as absent and does not run it.
    ///
    /// # Errors
    ///
    /// [`PluginError::Registration`] when this plugin already registered a
    /// wrapper under the key.
    pub fn wrap(
        self,
        spec: PromptWrapSpec,
        wrapper: Arc<dyn PromptSectionWrap>,
    ) -> Result<(), PluginError> {
        let owner = self.reg.owner.clone();
        let registry = &mut self.reg.contributions.prompt;
        if registry
            .wraps
            .iter()
            .any(|wrap| wrap.owner.plugin == owner.plugin && wrap.spec.key == spec.key)
        {
            return Err(PluginError::Registration(format!(
                "duplicate prompt wrapper `{}/{}`",
                owner.plugin, spec.key
            )));
        }
        registry.wraps.push(RegisteredPromptWrap {
            owner,
            spec,
            wrapper,
        });
        Ok(())
    }
}

/// A registered section as the host sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSectionInfo {
    pub section: PromptSectionId,
    pub owner: PluginRevision,
    pub default_placement: PromptPlacement,
    pub purposes: Vec<PromptPurpose>,
}

/// A session's registered sections and wrappers. Read-only.
#[derive(Clone, Default)]
pub struct PromptCatalog(Arc<PromptRegistry>);

impl PromptCatalog {
    pub(crate) fn new(registry: PromptRegistry) -> Self {
        Self(Arc::new(registry))
    }

    /// Every registered section, in registration order.
    pub fn sections(&self) -> Vec<PromptSectionInfo> {
        self.0
            .sections
            .iter()
            .map(|section| PromptSectionInfo {
                section: section.id(),
                owner: section.owner.clone(),
                default_placement: section.spec.default_placement,
                purposes: section.spec.purposes.clone(),
            })
            .collect()
    }

    /// Every registered wrapper, in chain order: plugin registration order,
    /// then declaration order.
    pub fn wraps(&self) -> Vec<ResolvedPromptWrap> {
        self.0
            .wraps
            .iter()
            .zip(0u32..)
            .map(|(wrap, ordinal)| ResolvedPromptWrap {
                wrap: wrap.id(),
                owner: wrap.owner.clone(),
                target: wrap.spec.target.clone(),
                ordinal,
            })
            .collect()
    }

    /// Resolve `plan` for a `purpose` call: the sections that render for
    /// it, the plan's order first and the rest in registration order, each
    /// with the host's placement or its plugin's default, and each with its
    /// wrapper chain.
    ///
    /// # Errors
    ///
    /// [`PromptPlanError`] when the plan breaks its own rules, names an
    /// unregistered section, or the call would exceed its section or
    /// wrapper count.
    pub fn resolve(
        &self,
        plan: &PromptPlan,
        purpose: &PromptPurpose,
    ) -> Result<ResolvedPromptComposition, PromptPlanError> {
        plan.validate()?;
        let registered = self
            .0
            .sections
            .iter()
            .map(RegisteredPromptSection::id)
            .collect::<BTreeSet<_>>();
        for section in plan
            .order
            .iter()
            .chain(plan.placements.iter().map(|placed| &placed.section))
        {
            if !registered.contains(section) {
                return Err(PromptPlanError::UnknownSection {
                    section: section.clone(),
                });
            }
        }
        let selected = self
            .0
            .sections
            .iter()
            .filter(|section| section.spec.purposes.contains(purpose))
            .collect::<Vec<_>>();
        let mut ordered = Vec::with_capacity(selected.len());
        for section in &plan.order {
            if let Some(found) = selected.iter().find(|candidate| &candidate.id() == section) {
                ordered.push(*found);
            }
        }
        for section in &selected {
            if !plan.order.contains(&section.id()) {
                ordered.push(section);
            }
        }
        let count = u32::try_from(ordered.len()).unwrap_or(u32::MAX);
        if count > plan.limits.max_sections.get() {
            return Err(PromptPlanError::TooManySections {
                count,
                limit: plan.limits.max_sections.get(),
            });
        }
        let ids = ordered
            .iter()
            .map(|section| section.id())
            .collect::<BTreeSet<_>>();
        let all_wraps = self.wraps();
        let (applied, absent): (Vec<_>, Vec<_>) = all_wraps
            .into_iter()
            .zip(self.0.wraps.iter())
            .partition(|(resolved, _)| ids.contains(&resolved.target));
        let wrap_count = u32::try_from(applied.len()).unwrap_or(u32::MAX);
        if wrap_count > plan.limits.max_wrappers.get() {
            return Err(PromptPlanError::TooManyWrappers {
                count: wrap_count,
                limit: plan.limits.max_wrappers.get(),
            });
        }
        let mut sections = Vec::with_capacity(ordered.len());
        let mut records = Vec::with_capacity(ordered.len());
        for section in ordered {
            let id = section.id();
            let (placement, placement_source) = match plan.placement(&id) {
                Some(placement) => (placement, PlacementSource::Host),
                None => (
                    section.spec.default_placement,
                    PlacementSource::PluginDefault,
                ),
            };
            let chain = applied
                .iter()
                .filter(|(resolved, _)| resolved.target == id)
                .collect::<Vec<_>>();
            records.push(ResolvedPromptSection {
                section: id,
                owner: section.owner.clone(),
                placement,
                placement_source,
                wraps: chain.iter().map(|(resolved, _)| resolved.clone()).collect(),
            });
            sections.push(ResolvedSectionRenderers {
                base: Arc::clone(&section.renderer),
                wraps: chain
                    .iter()
                    .map(|(_, registered)| Arc::clone(&registered.wrapper))
                    .collect(),
            });
        }
        Ok(ResolvedPromptComposition {
            record: ResolvedPromptPlan {
                purpose: purpose.clone(),
                sections: records,
                absent_targets: absent.into_iter().map(|(resolved, _)| resolved).collect(),
                limits: plan.limits,
            },
            sections,
        })
    }
}

struct ResolvedSectionRenderers {
    base: Arc<dyn PromptSection>,
    wraps: Vec<Arc<dyn PromptSectionWrap>>,
}

/// A resolved plan and the renderers it runs. Its [`record`](Self::record)
/// is what a call admits.
pub struct ResolvedPromptComposition {
    record: ResolvedPromptPlan,
    sections: Vec<ResolvedSectionRenderers>,
}

/// One section's text through its chain: the base text, each wrapper's
/// output in chain order, and the final text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedSection {
    pub base: SectionText,
    pub wraps: Vec<(PromptWrapId, SectionText)>,
    pub value: SectionText,
}

impl ComposedSection {
    /// The snapshot record of this section, placed as `resolved` says.
    pub fn record(&self, resolved: &ResolvedPromptSection) -> RenderedPromptSection {
        RenderedPromptSection {
            section: resolved.section.clone(),
            placement: resolved.placement,
            base: self.base.record(),
            wraps: self
                .wraps
                .iter()
                .map(|(wrap, output)| AppliedPromptWrap {
                    wrap: wrap.clone(),
                    output: output.record(),
                })
                .collect(),
            value: self.value.record(),
        }
    }
}

/// Run one renderer or wrapper: its output normalized, its refusal or
/// panic attributed to `site`.
fn run_site(
    site: &dyn Fn() -> Box<PromptRenderSite>,
    render: impl FnOnce() -> Result<SectionText, PromptRenderError>,
) -> Result<SectionText, PromptCompositionError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(render)) {
        Ok(Ok(text)) => Ok(text.normalized()),
        Ok(Err(error)) => Err(PromptCompositionError::Render {
            site: site(),
            error,
        }),
        Err(_) => Err(PromptCompositionError::Panicked { site: site() }),
    }
}

impl ResolvedPromptComposition {
    pub fn record(&self) -> &ResolvedPromptPlan {
        &self.record
    }

    /// Compose the section at `index` over `cut`: the base renderer, then
    /// each wrapper in chain order, each over the previous output. For base
    /// `R` and wrappers `A` then `B`, the final text is `B(A(R))`. Every
    /// output is normalized ([`SectionText::normalized`]) and held to the
    /// per-section limit; a refusal or panic is attributed to its site.
    ///
    /// # Errors
    ///
    /// [`PromptCompositionError`] attributed to the renderer or wrapper that
    /// refused or overran.
    ///
    /// # Panics
    ///
    /// When `index` is not a section of the plan.
    pub fn compose_section(
        &self,
        index: usize,
        cut: &PromptCut,
    ) -> Result<ComposedSection, PromptCompositionError> {
        let resolved = &self.record.sections[index];
        let renderers = &self.sections[index];
        let limit = self.record.limits.max_section_bytes.get();
        let within = |site: &dyn Fn() -> Box<PromptRenderSite>, text: &SectionText| {
            let bytes = text.as_text().map_or(0, str::len) as u64;
            if bytes > u64::from(limit) {
                return Err(PromptCompositionError::SectionTooLarge {
                    site: site(),
                    bytes,
                    limit,
                });
            }
            Ok(())
        };
        let base_site = || {
            Box::new(PromptRenderSite::Base {
                section: resolved.section.clone(),
                owner: resolved.owner.clone(),
            })
        };
        let base = run_site(&base_site, || {
            renderers
                .base
                .render(&cut.input_for(&resolved.owner.plugin))
        })?;
        within(&base_site, &base)?;
        let target = PromptWrapTarget {
            section: &resolved.section,
            placement: resolved.placement,
        };
        let mut value = base.clone();
        let mut wraps = Vec::with_capacity(renderers.wraps.len());
        for (wrapper, wrap) in renderers.wraps.iter().zip(&resolved.wraps) {
            let site = || {
                Box::new(PromptRenderSite::Wrap {
                    wrap: wrap.wrap.clone(),
                    owner: wrap.owner.clone(),
                    target: wrap.target.clone(),
                    ordinal: wrap.ordinal,
                })
            };
            let previous = value;
            value = run_site(&site, move || {
                wrapper.wrap(&cut.input_for(&wrap.owner.plugin), target, previous)
            })?;
            within(&site, &value)?;
            wraps.push((wrap.wrap.clone(), value.clone()));
        }
        Ok(ComposedSection { base, wraps, value })
    }
}

#[cfg(any(test, feature = "testing"))]
impl PromptCatalog {
    /// The sections and wrappers `plugins` register, in order, as a session
    /// build registers them.
    ///
    /// # Errors
    ///
    /// The [`PluginError`] a plugin's registration refuses with.
    pub fn of_plugins(plugins: &[Arc<dyn super::SessionPlugin>]) -> Result<Self, PluginError> {
        let mut contributions = super::PluginContributions::default();
        for plugin in plugins {
            let mut reg = PluginRegistrar::new(PluginRevision::new(
                plugin.id(),
                super::BehaviorRevision::ONE,
            ));
            reg.contributions = contributions;
            plugin.register(&mut reg)?;
            contributions = reg.contributions;
        }
        Ok(Self::new(contributions.prompt))
    }
}

impl super::PluginSession {
    /// The prompt sections and wrappers this session's plugins registered.
    pub fn prompt_catalog(&self) -> PromptCatalog {
        PromptCatalog::new(self.capabilities().contributions.prompt.clone())
    }
}

impl super::PluginRegistrar {
    /// Register prompt sections and wrappers.
    pub fn prompt(&mut self) -> PromptRegistrations<'_> {
        PromptRegistrations { reg: self }
    }
}

mod composer;

pub use composer::{
    ComposedPrompt, LoadedPromptSnapshot, PROMPT_SECTION_SEPARATOR, PromptRenderPool,
    PromptSnapshotLoadError, load_prompt_snapshot,
};

#[cfg(test)]
mod tests;
