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
    /// Rendering outlasted the call's render budget, queue wait included. A
    /// render still running then finishes unseen.
    #[error("prompt rendering outlasted its {budget_ms} ms budget")]
    BudgetExceeded { budget_ms: u32 },
    /// The render pool's queue of `capacity` renders is full, or the pool
    /// dropped a render without running it: a live fault of the process,
    /// never a call's outcome.
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

/// A family of sections a plugin derives from the tools offered to each
/// call, such as one section per offered MCP server. Every section it
/// contributes is keyed `<prefix>.<suffix>` under the registering plugin, and
/// behaves like a registered section: the host orders, places and excludes
/// it, and any plugin may wrap it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSectionFamilySpec {
    pub prefix: PromptSectionKey,
    pub default_placement: PromptPlacement,
    pub purposes: Vec<PromptPurpose>,
}

impl PromptSectionFamilySpec {
    /// A family for turn calls, with `default_placement` unless the host
    /// places a member.
    pub fn new(prefix: PromptSectionKey, default_placement: PromptPlacement) -> Self {
        Self {
            prefix,
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

    fn holds(&self, key: &PromptSectionKey) -> bool {
        key.as_str()
            .strip_prefix(self.prefix.as_str())
            .is_some_and(|rest| rest.starts_with('.'))
    }
}

/// One section a [`PromptSectionSource`] contributes: the suffix of its key
/// and its base renderer.
pub struct PromptFamilySection {
    pub suffix: PromptSectionKey,
    pub renderer: Arc<dyn PromptSection>,
}

/// The sections a family contributes to one call. A section is selected only
/// when the surface it describes is offered, so a source reads the call's
/// offered tools and nothing else. It must be repeat-safe.
pub trait PromptSectionSource: Send + Sync {
    /// The family's sections for a call offered `offered`, in order.
    fn sections(&self, offered: &OfferedTools) -> Vec<PromptFamilySection>;
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
    values: lash_core_store::plugin_state::NamespaceValues,
}

impl CommittedPluginNamespace {
    /// Only the runtime freezes a namespace, at the cut it builds from
    /// committed state, sharing `values`: a cut references the published
    /// values and copies none of them.
    pub(crate) fn new(
        generation: u64,
        values: impl Into<lash_core_store::plugin_state::NamespaceValues>,
    ) -> Self {
        Self {
            generation,
            values: values.into(),
        }
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

/// The tools actually offered to this call, derived from one pinned catalog.
/// Discovery keeps non-inline members available in the catalog without
/// offering their declarations or guidance in this call's inline surface.
#[derive(Clone, Debug, Default)]
pub struct OfferedTools {
    catalog: Arc<crate::ToolCatalog>,
    discovery: bool,
}

impl OfferedTools {
    /// A pinned catalog with its protocol's discovery policy. Without discovery,
    /// every member is offered; with discovery, only inline members are offered.
    pub fn new(catalog: Arc<crate::ToolCatalog>, discovery: bool) -> Self {
        Self { catalog, discovery }
    }

    /// The complete pinned catalog, including discovery-hidden members.
    pub fn catalog(&self) -> &Arc<crate::ToolCatalog> {
        &self.catalog
    }

    /// The manifests offered as native declarations or code-callable bindings.
    /// Discovery-hidden tools are absent from both views.
    pub fn manifests(&self) -> impl Iterator<Item = &crate::ToolManifest> {
        self.catalog
            .tools
            .iter()
            .map(|entry| &entry.manifest)
            .filter(|manifest| !self.discovery || manifest.inline)
    }

    /// Whether a tool named `name` is offered in the inline surface.
    pub fn offers(&self, name: &str) -> bool {
        self.manifests().any(|manifest| manifest.name == name)
    }
}

/// The admitted model the call runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptModel {
    pub profile: Option<crate::LlmProfileKey>,
    pub context_window_tokens: Option<u64>,
    /// The prompt usage the session last committed: the previous turn's,
    /// constant across one turn's calls. `None` before any call committed.
    pub committed_usage: Option<crate::LlmUsage>,
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
    protocol: Option<ProtocolPromptFacts>,
}

impl std::fmt::Debug for PromptCut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptCut")
            .field("call", &self.call)
            .field("offered", &self.offered)
            .field("model", &self.model)
            .field("history", &self.history)
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
    /// Only the runtime builds a cut, from committed state
    /// ([`core_internal::prompt_cut`](crate::core_internal::prompt_cut)).
    pub(crate) fn new(parts: PromptCutParts) -> Self {
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
            protocol: None,
        }
    }

    /// The protocol's committed facts for this call.
    #[must_use]
    pub(crate) fn with_protocol_facts(mut self, facts: Option<ProtocolPromptFacts>) -> Self {
        self.protocol = facts;
        self
    }

    /// The tools offered to the call, which select its family sections.
    pub fn offered(&self) -> &OfferedTools {
        &self.offered
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

    /// The call owner's derived facts of type `T`: a turn's protocol facts,
    /// or an owned call's purpose-specific section inputs. They are frozen
    /// for composition; admission records the resulting text.
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
struct RegisteredPromptFamily {
    owner: PluginRevision,
    spec: PromptSectionFamilySpec,
    source: Arc<dyn PromptSectionSource>,
}

impl RegisteredPromptFamily {
    fn id(&self) -> PromptSectionId {
        PromptSectionId::new(self.owner.plugin.clone(), self.spec.prefix.clone())
    }

    fn holds(&self, section: &PromptSectionId) -> bool {
        self.owner.plugin == section.owner && self.spec.holds(&section.key)
    }
}

/// A section or a family, in registration order.
#[derive(Clone)]
enum RegisteredPromptEntry {
    Section(RegisteredPromptSection),
    Family(RegisteredPromptFamily),
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

/// Every section, family and wrapper the installed plugins registered, in
/// plugin registration order, then declaration order.
#[derive(Clone, Default)]
pub(crate) struct PromptRegistry {
    entries: Vec<RegisteredPromptEntry>,
    wraps: Vec<RegisteredPromptWrap>,
}

impl PromptRegistry {
    fn sections(&self) -> impl Iterator<Item = &RegisteredPromptSection> {
        self.entries.iter().filter_map(|entry| match entry {
            RegisteredPromptEntry::Section(section) => Some(section),
            RegisteredPromptEntry::Family(_) => None,
        })
    }

    fn families(&self) -> impl Iterator<Item = &RegisteredPromptFamily> {
        self.entries.iter().filter_map(|entry| match entry {
            RegisteredPromptEntry::Family(family) => Some(family),
            RegisteredPromptEntry::Section(_) => None,
        })
    }

    /// Whether `section` names a registered section or a key a registered
    /// family holds.
    fn knows(&self, section: &PromptSectionId) -> bool {
        self.sections()
            .any(|registered| &registered.id() == section)
            || self.families().any(|family| family.holds(section))
    }
}

/// Registers the plugin's prompt sections, families and wrappers.
pub struct PromptRegistrations<'a> {
    pub(super) reg: &'a mut PluginRegistrar,
}

impl PromptRegistrations<'_> {
    /// Register a section under this plugin's id and `spec.key`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Registration`] when this plugin already registered a
    /// section under the key, or a family that holds it.
    pub fn section(
        self,
        spec: PromptSectionSpec,
        renderer: Arc<dyn PromptSection>,
    ) -> Result<(), PluginError> {
        let owner = self.reg.owner.clone();
        let registry = &mut self.reg.contributions.prompt;
        let id = PromptSectionId::new(owner.plugin.clone(), spec.key.clone());
        if registry.knows(&id) {
            return Err(PluginError::Registration(format!(
                "duplicate prompt section `{id}`"
            )));
        }
        registry
            .entries
            .push(RegisteredPromptEntry::Section(RegisteredPromptSection {
                owner,
                spec,
                renderer,
            }));
        Ok(())
    }

    /// Register a family of sections keyed `<spec.prefix>.<suffix>` under
    /// this plugin's id, which `source` derives from each call's offered
    /// tools.
    ///
    /// # Errors
    ///
    /// [`PluginError::Registration`] when the family's keys overlap a
    /// section or another family this plugin registered.
    pub fn family(
        self,
        spec: PromptSectionFamilySpec,
        source: Arc<dyn PromptSectionSource>,
    ) -> Result<(), PluginError> {
        let owner = self.reg.owner.clone();
        let registry = &mut self.reg.contributions.prompt;
        let overlaps = registry.entries.iter().any(|entry| match entry {
            RegisteredPromptEntry::Section(section) => {
                section.owner.plugin == owner.plugin && spec.holds(&section.spec.key)
            }
            RegisteredPromptEntry::Family(family) => {
                family.owner.plugin == owner.plugin
                    && (family.spec.prefix == spec.prefix
                        || family.spec.holds(&spec.prefix)
                        || spec.holds(&family.spec.prefix))
            }
        });
        if overlaps {
            return Err(PluginError::Registration(format!(
                "prompt section family `{}/{}` overlaps a registered section or family",
                owner.plugin, spec.prefix
            )));
        }
        registry
            .entries
            .push(RegisteredPromptEntry::Family(RegisteredPromptFamily {
                owner,
                spec,
                source,
            }));
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

/// A registered section family as the host sees it: `family` is the owner
/// and the prefix every member key starts with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSectionFamilyInfo {
    pub family: PromptSectionId,
    pub owner: PluginRevision,
    pub default_placement: PromptPlacement,
    pub purposes: Vec<PromptPurpose>,
}

/// A session's registered sections, families and wrappers. Read-only.
#[derive(Clone, Default)]
pub struct PromptCatalog(Arc<PromptRegistry>);

/// A section selected for one call, before placement.
struct SelectedSection {
    id: PromptSectionId,
    owner: PluginRevision,
    default_placement: PromptPlacement,
    renderer: Arc<dyn PromptSection>,
}

impl PromptCatalog {
    pub(crate) fn new(registry: PromptRegistry) -> Self {
        Self(Arc::new(registry))
    }

    /// Every registered section, in registration order.
    pub fn sections(&self) -> Vec<PromptSectionInfo> {
        self.0
            .sections()
            .map(|section| PromptSectionInfo {
                section: section.id(),
                owner: section.owner.clone(),
                default_placement: section.spec.default_placement,
                purposes: section.spec.purposes.clone(),
            })
            .collect()
    }

    /// Every registered section family, in registration order.
    pub fn families(&self) -> Vec<PromptSectionFamilyInfo> {
        self.0
            .families()
            .map(|family| PromptSectionFamilyInfo {
                family: family.id(),
                owner: family.owner.clone(),
                default_placement: family.spec.default_placement,
                purposes: family.spec.purposes.clone(),
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

    /// The sections that render for a `purpose` call offered `offered`, in
    /// registration order, each family expanded in place.
    fn select(
        &self,
        purpose: &PromptPurpose,
        offered: &OfferedTools,
    ) -> Result<Vec<SelectedSection>, PromptPlanError> {
        let mut selected = Vec::new();
        for entry in &self.0.entries {
            match entry {
                RegisteredPromptEntry::Section(section) => {
                    if section.spec.purposes.contains(purpose) {
                        selected.push(SelectedSection {
                            id: section.id(),
                            owner: section.owner.clone(),
                            default_placement: section.spec.default_placement,
                            renderer: Arc::clone(&section.renderer),
                        });
                    }
                }
                RegisteredPromptEntry::Family(family) => {
                    if !family.spec.purposes.contains(purpose) {
                        continue;
                    }
                    let refused =
                        |section: String, reason: &str| PromptPlanError::SourceSectionRefused {
                            family: family.id(),
                            section,
                            reason: reason.into(),
                        };
                    let mut members = BTreeSet::new();
                    for member in family.source.sections(offered) {
                        let key = format!("{}.{}", family.spec.prefix, member.suffix);
                        let key = PromptSectionKey::new(key.clone())
                            .map_err(|error| refused(key, &error.to_string()))?;
                        if !members.insert(key.clone()) {
                            return Err(refused(key.to_string(), "contributed twice"));
                        }
                        selected.push(SelectedSection {
                            id: PromptSectionId::new(family.owner.plugin.clone(), key),
                            owner: family.owner.clone(),
                            default_placement: family.spec.default_placement,
                            renderer: member.renderer,
                        });
                    }
                }
            }
        }
        Ok(selected)
    }

    /// Validate a new host plan against the registered catalog. A key under
    /// a registered family's prefix is admitted even when this call offers
    /// none of that family's tools.
    pub fn validate_plan(&self, plan: &PromptPlan) -> Result<(), PromptPlanError> {
        plan.validate()?;
        for section in plan
            .order
            .iter()
            .chain(plan.placements.iter().map(|p| &p.section))
        {
            if !self.0.knows(section) {
                return Err(PromptPlanError::UnknownSection {
                    section: section.clone(),
                });
            }
        }
        Ok(())
    }

    /// How `plan` resolves for a `purpose` call offered `offered`: the
    /// record [`resolve`](Self::resolve) would admit, with no renderer run.
    /// A host previews its plan with it; nothing composes or records.
    ///
    /// # Errors
    ///
    /// [`PromptPlanError`], as [`resolve`](Self::resolve) refuses.
    pub fn preview(
        &self,
        plan: &PromptPlan,
        purpose: &PromptPurpose,
        offered: &OfferedTools,
    ) -> Result<ResolvedPromptPlan, PromptPlanError> {
        self.resolve(plan, purpose, offered)
            .map(|composition| composition.record)
    }

    /// Resolve `plan` for a `purpose` call offered `offered`: the sections
    /// that render for it (each family contributing the sections its source
    /// derives from `offered`), the plan's order first and the rest in
    /// registration order, each with the host's placement or its plugin's
    /// default, and each with its wrapper chain. An excluded section keeps
    /// its place in the record but no chain: its wrappers are recorded as
    /// absent. Overrides whose section is no longer registered are skipped
    /// and recorded under `absent_overrides`.
    ///
    /// # Errors
    ///
    /// [`PromptPlanError`] when the plan breaks its own rules, a source contributes
    /// an invalid section, or the call would exceed its section or wrapper
    /// count.
    pub(crate) fn resolve(
        &self,
        plan: &PromptPlan,
        purpose: &PromptPurpose,
        offered: &OfferedTools,
    ) -> Result<ResolvedPromptComposition, PromptPlanError> {
        plan.validate()?;
        let absent_overrides = plan
            .order
            .iter()
            .chain(plan.placements.iter().map(|p| &p.section))
            .filter(|section| !self.0.knows(section))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut selected = self
            .select(purpose, offered)?
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        let mut ordered = Vec::with_capacity(selected.len());
        for section in &plan.order {
            if let Some(found) = selected
                .iter_mut()
                .find(|candidate| candidate.as_ref().is_some_and(|c| &c.id == section))
                .and_then(Option::take)
            {
                ordered.push(found);
            }
        }
        ordered.extend(selected.into_iter().flatten());
        let count = u32::try_from(ordered.len()).unwrap_or(u32::MAX);
        if count > plan.limits.max_sections.get() {
            return Err(PromptPlanError::TooManySections {
                count,
                limit: plan.limits.max_sections.get(),
            });
        }
        let placed = ordered
            .into_iter()
            .map(|section| {
                let (placement, source) = match plan.placement(&section.id) {
                    Some(placement) => (placement, PlacementSource::Host),
                    None => (section.default_placement, PlacementSource::PluginDefault),
                };
                (section, placement, source)
            })
            .collect::<Vec<_>>();
        let composed = placed
            .iter()
            .filter(|(_, placement, _)| *placement != PromptPlacement::Excluded)
            .map(|(section, _, _)| section.id.clone())
            .collect::<BTreeSet<_>>();
        let (applied, absent): (Vec<_>, Vec<_>) = self
            .wraps()
            .into_iter()
            .zip(self.0.wraps.iter())
            .partition(|(resolved, _)| composed.contains(&resolved.target));
        let wrap_count = u32::try_from(applied.len()).unwrap_or(u32::MAX);
        if wrap_count > plan.limits.max_wrappers.get() {
            return Err(PromptPlanError::TooManyWrappers {
                count: wrap_count,
                limit: plan.limits.max_wrappers.get(),
            });
        }
        let mut sections = Vec::with_capacity(placed.len());
        let mut records = Vec::with_capacity(placed.len());
        for (section, placement, placement_source) in placed {
            let chain = applied
                .iter()
                .filter(|(resolved, _)| resolved.target == section.id)
                .collect::<Vec<_>>();
            records.push(ResolvedPromptSection {
                section: section.id,
                owner: section.owner,
                placement,
                placement_source,
                wraps: chain.iter().map(|(resolved, _)| resolved.clone()).collect(),
            });
            sections.push(ResolvedSectionRenderers {
                base: section.renderer,
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
                absent_overrides,
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
pub(crate) struct ResolvedPromptComposition {
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
    pub(crate) fn record(&self) -> &ResolvedPromptPlan {
        &self.record
    }

    /// Compose the section at `index` over `cut`: the base renderer, then
    /// each wrapper in chain order, each over the previous output. For base
    /// `R` and wrappers `A` then `B`, the final text is `B(A(R))`. Every
    /// output is normalized ([`SectionText::normalized`]) and held to the
    /// per-section limit; a refusal or panic is attributed to its site. A
    /// section the host excluded composes to an omission without running its
    /// renderer.
    ///
    /// # Errors
    ///
    /// [`PromptCompositionError`] attributed to the renderer or wrapper that
    /// refused or overran.
    ///
    /// # Panics
    ///
    /// When `index` is not a section of the plan.
    pub(crate) fn compose_section(
        &self,
        index: usize,
        cut: &PromptCut,
    ) -> Result<ComposedSection, PromptCompositionError> {
        let resolved = &self.record.sections[index];
        let renderers = &self.sections[index];
        if resolved.placement == PromptPlacement::Excluded {
            return Ok(ComposedSection {
                base: SectionText::Omit,
                wraps: Vec::new(),
                value: SectionText::Omit,
            });
        }
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
    /// The prompt sections, families and wrappers this session's plugins
    /// registered.
    pub fn prompt_catalog(&self) -> PromptCatalog {
        PromptCatalog::new(self.capabilities().contributions.prompt.clone())
    }

    /// [`Self::prompt_catalog`], or `None` before the session's plugins are
    /// built: a session builds them for its first run.
    pub fn built_prompt_catalog(&self) -> Option<PromptCatalog> {
        self.capabilities
            .get()
            .map(|capabilities| PromptCatalog::new(capabilities.contributions.prompt.clone()))
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
    AdmittedCallLoadError, ComposedPrompt, LoadedAdmittedCall, LoadedPromptSnapshot,
    PROMPT_SECTION_SEPARATOR, PromptRenderPool, PromptRenderPoolConfig, admission_record,
    load_admitted_call,
};

#[cfg(test)]
mod tests;
