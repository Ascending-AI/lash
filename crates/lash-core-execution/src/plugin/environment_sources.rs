//! Source collection for the actor's change-driven environment. No turn driving,
//! retention or head mutation happens here; the actor assembles and publishes.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_core_store::environment::{
    EnvironmentChange, EnvironmentFragment, InputFrontier, SourceDescriptor, SourceId, SourceMode,
    SourceRevision, SourceSnapshot,
};
use lash_protocol_types::sync::MutexExt;
use {lash_kernel_value::package::PublicationDomain, lash_kernel_value::package::PublicationScope};

use super::{OwnerConfigView, PluginError, ToolProviderHandle};

mod catalog;

/// The actor supplies a mailbox writer. Sources retain only weak references;
/// the collector owns the subscription lifetime. The writer must only enqueue,
/// never call back into the source or drive a turn.
pub type EnvironmentChangeSink = Arc<dyn Fn(EnvironmentChange) + Send + Sync>;

/// A host may register its own coherent source using the same seam as plugins.
/// Snapshot revisions are authoritative; a notification is only a dirty hint.
pub trait EnvironmentSource: Send + Sync {
    fn descriptor(&self) -> SourceDescriptor;
    fn snapshot(&self, config: &OwnerConfigView) -> Result<SourceSnapshot, SourceCollectionError>;
    fn refresh(&self) {}
    fn config_dependencies(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }
    fn namespace_dependencies(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }
    fn subscribe(&self, _sink: &EnvironmentChangeSink) {}
}

#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SourceCollectionError {
    #[error(transparent)]
    Assembly(Box<lash_core_store::environment::AssemblyRefusal>),
    #[error("environment source {0:?} is already registered")]
    DuplicateSource(SourceId),
    #[error("environment source {0:?} is not registered")]
    UnknownSource(SourceId),
    #[error("environment source {expected:?} returned snapshot for {actual:?}")]
    WrongSource {
        expected: SourceId,
        actual: SourceId,
    },
    #[error("environment source {0:?} returned a regressed revision")]
    RegressedRevision(SourceId),
    #[error("namespace inputs changed while observing source {0:?}")]
    ChangedDuringSnapshot(SourceId),
    #[error("source {source_id:?} supplied an invalid route for {tool}")]
    InvalidRoute {
        source_id: SourceId,
        tool: lash_vocabulary::ToolId,
    },
    #[error(transparent)]
    Provider(Box<PluginError>),
}

impl From<lash_core_store::environment::AssemblyRefusal> for SourceCollectionError {
    fn from(refusal: lash_core_store::environment::AssemblyRefusal) -> Self {
        Self::Assembly(Box::new(refusal))
    }
}

impl From<PluginError> for SourceCollectionError {
    fn from(error: PluginError) -> Self {
        Self::Provider(Box::new(error))
    }
}

/// Registered once per session materialization. The frontier remains the actor's
/// durable fact: collection does not advance it, including on partial failure.
#[derive(Default)]
pub struct EnvironmentSources {
    sources: BTreeMap<SourceId, Arc<dyn EnvironmentSource>>,
    catalog: Option<Arc<catalog::CatalogSource>>,
    config_dependents: BTreeMap<String, BTreeSet<SourceId>>,
    namespace_dependents: BTreeMap<String, BTreeSet<SourceId>>,
    subscriptions: Vec<EnvironmentChangeSink>,
}
impl SourceCollectionError {
    /// Preserve the source's typed refusal across config validation's boundary.
    pub(crate) fn into_plugin_error(self) -> PluginError {
        if let Self::Provider(error) = self {
            return *error;
        }
        if let Self::Assembly(refusal) = self {
            let refusal =
                lash_execution_contracts::config_error::CoreConfigRefusal::environment_refusal(
                    *refusal,
                );
            return PluginError::Runtime(
                crate::RuntimeEffectControllerError::run_shape_refused(
                    crate::RunShapeRefusal::Owner { refusal },
                )
                .into_runtime_error(),
            );
        }
        let payload = match serde_json::to_value(&self) {
            Ok(payload) => payload,
            Err(error) => {
                return PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::RecordEncodingFailed,
                    error.to_string(),
                ));
            }
        };
        let code = crate::RuntimeErrorCode::ToolCatalogResolutionFailed;
        PluginError::Runtime(
            crate::RuntimeError::new(code.clone(), self.to_string()).with_cause(
                crate::RuntimeErrorCause::PluginOperation {
                    failure: Box::new(lash_protocol_types::PluginOperationFailure {
                        error_type: "lash.environment_source".into(),
                        error_version: std::num::NonZeroU32::MIN,
                        payload,
                        class: lash_protocol_types::PluginFailureClass::Redrivable,
                        code: crate::FailureCode::from(&code),
                        message: self.to_string(),
                        origin: None,
                    }),
                },
            ),
        )
    }
}

impl EnvironmentSources {
    /// Collect one candidate from this source set, including its catalog-only
    /// contributors. Used for explicit validation, never for a catalog read.
    pub(crate) fn initial_body(
        &self,
        config: &OwnerConfigView,
    ) -> Result<lash_core_store::environment::EnvironmentBody, SourceCollectionError> {
        use lash_core_store::environment::assemble;
        let snapshots = self.collect(&InputFrontier::default(), [], config)?;
        let body = assemble(None, snapshots)?;
        let projected = self.project(
            body.fragments().iter().map(|(id, f)| (id, f.fragment())),
            config,
        )?;
        if projected.is_empty() {
            Ok(body)
        } else {
            Ok(assemble(Some(&body), projected)?)
        }
    }

    pub fn register(
        &mut self,
        source: Arc<dyn EnvironmentSource>,
        sink: EnvironmentChangeSink,
    ) -> Result<(), SourceCollectionError> {
        let id = source.descriptor().source;
        if self.sources.contains_key(&id) {
            return Err(SourceCollectionError::DuplicateSource(id));
        }
        for owner in source.config_dependencies() {
            self.config_dependents
                .entry(owner)
                .or_default()
                .insert(id.clone());
        }
        for owner in source.namespace_dependencies() {
            self.namespace_dependents
                .entry(owner)
                .or_default()
                .insert(id.clone());
        }
        // Give this registration its own lifetime even when callers retain
        // the original mailbox writer or share it with another collector.
        let subscription: EnvironmentChangeSink = Arc::new(move |change| sink(change));
        source.subscribe(&subscription);
        self.subscriptions.push(subscription);
        self.sources.insert(id, source);
        Ok(())
    }
    pub fn descriptors(&self) -> impl Iterator<Item = SourceDescriptor> + '_ {
        self.sources.values().map(|source| source.descriptor())
    }
    /// Call only after the config commit, with the owner IDs that changed.
    pub fn config_committed<'a>(
        &self,
        owners: impl IntoIterator<Item = &'a str>,
    ) -> EnvironmentChange {
        EnvironmentChange::ConfigSelection {
            sources: dependents(&self.config_dependents, owners),
        }
    }
    /// Call only after namespace publication, with its changed owner IDs.
    pub fn namespace_committed<'a>(
        &self,
        owners: impl IntoIterator<Item = &'a str>,
    ) -> EnvironmentChange {
        EnvironmentChange::ConfigSelection {
            sources: dependents(&self.namespace_dependents, owners),
        }
    }
    /// Drain mailbox changes once at a model-call boundary. Empty frontiers
    /// initialize all sources; otherwise only dirty sources are invoked. Same
    /// incarnation notices at/below the frontier do no work. A different
    /// incarnation is reconciled by asking the current registered source once,
    /// never by adopting the notice's possibly stale revision.
    pub fn collect(
        &self,
        frontier: &InputFrontier,
        changes: impl IntoIterator<Item = EnvironmentChange>,
        config: &OwnerConfigView,
    ) -> Result<Vec<SourceSnapshot>, SourceCollectionError> {
        lash_core_ids::operational_metrics::turn::sync_phase("environment collection", || {
            let mut dirty: BTreeMap<SourceId, bool> = self
                .sources
                .keys()
                .filter(|id| !frontier.contains_key(*id))
                .map(|id| (id.clone(), true))
                .collect();
            let mut refresh = BTreeSet::new();
            for change in changes {
                match change {
                    EnvironmentChange::ConfigSelection { sources } => {
                        for source in sources {
                            self.require(&source)?;
                            dirty.insert(source, true);
                        }
                    }
                    EnvironmentChange::Refresh { sources } => {
                        let sources =
                            sources.unwrap_or_else(|| self.sources.keys().cloned().collect());
                        for id in sources {
                            if self.require(&id)?.descriptor().mode != SourceMode::Immutable {
                                // Coalesced below so duplicate refresh mail asks once.
                                refresh.insert(id.clone());
                                dirty.insert(id, true);
                            }
                        }
                    }
                    EnvironmentChange::SourceRevision { source, revision } => {
                        if self.require(&source)?.descriptor().mode == SourceMode::Notifying
                            && frontier.get(&source).is_none_or(|old| {
                                old.incarnation != revision.incarnation
                                    || old.counter < revision.counter
                            })
                        {
                            dirty.entry(source).or_insert(false);
                        }
                    }
                }
            }
            for id in refresh {
                self.require(&id)?.refresh();
            }
            let mut snapshots = Vec::with_capacity(dirty.len());
            for (id, forced) in dirty {
                let snapshot = self.require(&id)?.snapshot(config)?;
                if snapshot.source != id {
                    return Err(SourceCollectionError::WrongSource {
                        expected: id,
                        actual: snapshot.source,
                    });
                }
                if let Some(old) = frontier.get(&id)
                    && old.incarnation == snapshot.revision.incarnation
                {
                    if snapshot.revision.counter < old.counter {
                        return Err(SourceCollectionError::RegressedRevision(id));
                    }
                    if !forced && snapshot.revision == *old {
                        continue;
                    }
                }
                snapshots.push(snapshot);
            }
            Ok(snapshots)
        })
    }
    /// Complete derived projections only for a dirty candidate. Source owners
    /// retain coherence witnesses; a replay consumes their recorded output.
    pub fn project<'a>(
        &self,
        fragments: impl IntoIterator<Item = (&'a SourceId, &'a EnvironmentFragment)>,
        config: &OwnerConfigView,
    ) -> Result<Vec<SourceSnapshot>, SourceCollectionError> {
        let Some(catalog) = &self.catalog else {
            return Ok(Vec::new());
        };
        let fragments: Vec<_> = fragments.into_iter().collect();
        let recorded = fragments
            .iter()
            .find_map(|(id, fragment)| (id.0 == catalog::SOURCE).then_some(*fragment));
        let tools = fragments
            .iter()
            .flat_map(|(_, f)| f.tools.iter().map(|tool| tool.definition.manifest.clone()))
            .collect();
        catalog
            .project(tools, recorded, config)
            .map(|snapshot| vec![snapshot])
    }
    fn require(&self, id: &SourceId) -> Result<&Arc<dyn EnvironmentSource>, SourceCollectionError> {
        self.sources
            .get(id)
            .ok_or_else(|| SourceCollectionError::UnknownSource(id.clone()))
    }
}
fn dependents<'a>(
    index: &BTreeMap<String, BTreeSet<SourceId>>,
    owners: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<SourceId> {
    owners
        .into_iter()
        .flat_map(|owner| index.get(owner).into_iter().flatten().cloned())
        .collect()
}

/// Stable source identity is the registered provider identity, shared with its
/// package hook. It never uses a vector position or a display tool name.
pub fn provider_source_id(key: &ToolProviderHandle) -> SourceId {
    SourceId(format!("provider:{key}"))
}

type WeakChangeSink = std::sync::Weak<dyn Fn(EnvironmentChange) + Send + Sync>;
type InvalidationIndex = BTreeMap<String, Vec<(SourceId, WeakChangeSink)>>;
type NamespaceSlices = BTreeMap<String, lash_core_store::plugin_state::PluginNamespaceState>;

/// Only the content address participates in environment identity. Namespace
/// values already belong to the checkpoint's keyed graph; copying them into
/// each environment would rewrite unrelated, unchanged state on publication.
fn namespace_identity(
    namespace: &lash_core_store::plugin_state::PluginNamespaceState,
) -> serde_json::Value {
    serde_json::json!({
        "format_version": namespace.format_version,
        "values_root": namespace.body().values,
    })
}

/// Resident subscription routing, populated from each source's declarations.
/// Weak writers make dropping the collector release every registration.
#[derive(Default)]
pub(crate) struct SourceInvalidations {
    config: InvalidationIndex,
    namespaces: InvalidationIndex,
}
impl std::fmt::Debug for SourceInvalidations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceInvalidations")
    }
}
impl SourceInvalidations {
    fn register(
        &mut self,
        source: &SourceId,
        config: BTreeSet<String>,
        namespaces: BTreeSet<String>,
        sink: &EnvironmentChangeSink,
    ) {
        for (owners, index) in [
            (config, &mut self.config),
            (namespaces, &mut self.namespaces),
        ] {
            for owner in owners {
                index
                    .entry(owner)
                    .or_default()
                    .push((source.clone(), Arc::downgrade(sink)));
            }
        }
    }
    pub(super) fn config_committed(&mut self, owners: &[String]) {
        Self::notify(&mut self.config, owners);
    }
    pub(super) fn namespace_committed(&mut self, owners: &[String]) {
        Self::notify(&mut self.namespaces, owners);
    }
    fn notify(index: &mut InvalidationIndex, owners: &[String]) {
        for owner in owners {
            if let Some(subscribers) = index.get_mut(owner) {
                subscribers.retain(|(source, weak)| {
                    if let Some(sink) = weak.upgrade() {
                        sink(EnvironmentChange::ConfigSelection {
                            sources: [source.clone()].into(),
                        });
                        true
                    } else {
                        false
                    }
                });
            }
        }
    }
}

struct ProviderSource {
    key: ToolProviderHandle,
    tools: BTreeSet<ToolProviderHandle>,
    registry: Arc<crate::ToolRegistry>,
    packages: BTreeMap<ToolProviderHandle, Arc<dyn super::packages::PackageProvider>>,
    domain: PublicationDomain,
    scope: PublicationScope,
    revision: Mutex<SourceRevision>,
    namespaces: Option<Arc<Mutex<super::state::PluginStateRegistry>>>,
    subscriptions: Mutex<Vec<EnvironmentChangeSink>>,
}
impl ProviderSource {
    fn namespace_slices(&self) -> NamespaceSlices {
        let owners = self.namespace_dependencies();
        if owners.is_empty() {
            return BTreeMap::new();
        }
        let Some(namespaces) = &self.namespaces else {
            return BTreeMap::new();
        };
        let state = namespaces.lock_recover();
        owners
            .into_iter()
            .filter_map(|owner| {
                state
                    .data
                    .plugins
                    .get(&owner)
                    .map(|namespace| (owner, namespace.clone()))
            })
            .collect()
    }
    fn dependencies(&self, namespaces: bool) -> BTreeSet<String> {
        let mut owners = BTreeSet::new();
        for key in &self.tools {
            let provider = self
                .registry
                .provider(key)
                .unwrap_or_else(|| unreachable!("registered source member"));
            if namespaces {
                owners.extend(provider.environment_namespace_dependencies());
            } else {
                owners.insert(key.owner().to_owned());
                owners.extend(provider.environment_config_dependencies());
            }
        }
        for (key, provider) in &self.packages {
            if namespaces {
                owners.extend(provider.environment_namespace_dependencies());
            } else {
                owners.insert(key.owner().to_owned());
                owners.extend(provider.environment_config_dependencies());
            }
        }
        owners
    }
}
impl EnvironmentSource for ProviderSource {
    fn refresh(&self) {
        for key in &self.tools {
            if let Some(provider) = self.registry.provider(key) {
                provider.refresh_environment();
            }
        }
        for provider in self.packages.values() {
            if !provider.shares_tool_source() || self.tools.is_empty() {
                provider.refresh_environment();
            }
        }
    }
    fn descriptor(&self) -> SourceDescriptor {
        let modes = self
            .tools
            .iter()
            .filter_map(|key| {
                self.registry
                    .provider(key)
                    .map(|provider| provider.environment_mode())
            })
            .chain(
                self.packages
                    .values()
                    .map(|provider| provider.environment_mode()),
            );
        let mode = modes.fold(SourceMode::Immutable, |prior, mode| match (prior, mode) {
            (SourceMode::Notifying, _) | (_, SourceMode::Notifying) => SourceMode::Notifying,
            (SourceMode::RefreshOnly, _) | (_, SourceMode::RefreshOnly) => SourceMode::RefreshOnly,
            _ => SourceMode::Immutable,
        });
        SourceDescriptor {
            source: provider_source_id(&self.key),
            mode,
        }
    }
    fn config_dependencies(&self) -> BTreeSet<String> {
        self.dependencies(false)
    }
    fn namespace_dependencies(&self) -> BTreeSet<String> {
        self.dependencies(true)
    }
    fn subscribe(&self, sink: &EnvironmentChangeSink) {
        let source = provider_source_id(&self.key);
        if let Some(namespaces) = &self.namespaces {
            namespaces.lock_recover().environment.register(
                &source,
                self.config_dependencies(),
                self.namespace_dependencies(),
                sink,
            );
        }
        let composite = self.tools.len() > 1 || self.packages.len() > 1;
        let sink = if composite {
            // Independent native revisions cannot be ordered against a group
            // revision. The group observes current members on one dirty hint.
            let source = source.clone();
            let target = sink.clone();
            let forwarding: EnvironmentChangeSink = Arc::new(move |_| {
                target(EnvironmentChange::ConfigSelection {
                    sources: [source.clone()].into(),
                })
            });
            self.subscriptions.lock_recover().push(forwarding.clone());
            forwarding
        } else {
            sink.clone()
        };
        for key in &self.tools {
            if let Some(provider) = self.registry.provider(key) {
                provider.subscribe_environment(&source, &sink);
            }
        }
        for provider in self.packages.values() {
            if !provider.shares_tool_source() || self.tools.is_empty() {
                provider.subscribe_environment(&source, &sink);
            }
        }
    }
    fn snapshot(&self, config: &OwnerConfigView) -> Result<SourceSnapshot, SourceCollectionError> {
        let namespaces = self.namespace_slices();
        let revision = {
            let mut revision = self.revision.lock_recover();
            revision.counter = revision
                .counter
                .checked_add(1)
                .ok_or_else(|| PluginError::Session("source revision exhausted".into()))?;
            revision.clone()
        };
        let source = provider_source_id(&self.key);
        let mut snapshot = SourceSnapshot {
            source: source.clone(),
            revision: revision.clone(),
            fragment: EnvironmentFragment::default(),
        };
        for key in &self.tools {
            let observation = self.registry.snapshot_source(
                key,
                &source,
                revision.clone(),
                config,
                Some((&self.domain, &self.scope)),
            )?;
            if observation.source != source {
                return Err(SourceCollectionError::WrongSource {
                    expected: source,
                    actual: observation.source,
                });
            }
            if self.tools.len() == 1 {
                snapshot.revision = observation.revision;
            }
            merge(&mut snapshot.fragment, observation.fragment);
        }
        for (key, provider) in &self.packages {
            if self.tools.is_empty() {
                lash_core_ids::operational_metrics::turn::callbacks(1, 0, 0);
                let observation = provider.environment_snapshot(
                    config,
                    &self.domain,
                    &self.scope,
                    key,
                    (source.clone(), revision.clone()),
                )?;
                if observation.source != source {
                    return Err(SourceCollectionError::WrongSource {
                        expected: source,
                        actual: observation.source,
                    });
                }
                if self.packages.len() == 1 {
                    snapshot.revision = observation.revision;
                }
                merge(&mut snapshot.fragment, observation.fragment);
            } else {
                let fragment = provider.environment_fragment(
                    config,
                    &self.domain,
                    &self.scope,
                    key,
                    Some(&snapshot.fragment.tools),
                )?;
                merge(&mut snapshot.fragment, fragment);
            }
        }
        for candidate in &snapshot.fragment.routes {
            let manifest = &candidate.definition.manifest;
            if candidate.operation != manifest.operation
                || candidate.route.provider.registry() != self.key.registry()
                || !self
                    .tools
                    .iter()
                    .chain(self.packages.keys())
                    .any(|member| member.owner() == candidate.route.provider.owner())
                || candidate.route.tool != manifest.id
                || candidate.route.policy != manifest.execution_policy
                || candidate.route.projection != manifest.argument_projection
                || !self.registry.serves(&candidate.route.provider)
                || candidate.route.policy == lash_kernel_value::ExecutionPolicy::Read
                    && self.registry.reads(&candidate.route.provider).is_none()
            {
                return Err(SourceCollectionError::InvalidRoute {
                    source_id: snapshot.source.clone(),
                    tool: candidate.route.tool.clone(),
                });
            }
        }
        if namespaces != self.namespace_slices() {
            return Err(SourceCollectionError::ChangedDuringSnapshot(
                snapshot.source,
            ));
        }
        for (owner, namespace) in namespaces {
            snapshot
                .fragment
                .config
                .insert(format!("namespace:{owner}"), namespace_identity(&namespace));
        }
        for owner in self.config_dependencies() {
            if let Some(value) = config.environment_config_slice(&owner).map_err(|error| {
                PluginError::ProviderConfig {
                    owner: owner.clone(),
                    message: error.to_string(),
                }
            })? {
                snapshot.fragment.config.insert(owner, value);
            }
        }
        Ok(snapshot)
    }
}
fn merge(into: &mut EnvironmentFragment, fragment: EnvironmentFragment) {
    into.tools.extend(fragment.tools);
    into.model_removals.extend(fragment.model_removals);
    if fragment.deferred_provider.is_some() {
        into.deferred_provider = fragment.deferred_provider;
    }
    into.routes.extend(fragment.routes);
    into.package_roots.extend(fragment.package_roots);
    into.namespace_claims.extend(fragment.namespace_claims);
    into.deferred_grants.extend(fragment.deferred_grants);
    into.dependencies.extend(fragment.dependencies);
    for (registry, requirements) in fragment.identities {
        into.identities
            .entry(registry)
            .or_default()
            .extend(&requirements);
    }
    into.engines.extend(fragment.engines);
    into.config.extend(fragment.config);
    if fragment.consumer.is_some() {
        into.consumer = fragment.consumer;
    }
}
impl super::ExecutionComposition {
    /// Package publishers declare any tool registrations they jointly project.
    /// Connected memberships form one source, so each provider is asked once
    /// and identical tool/route facts stay in one coherent fragment.
    pub(super) fn environment_sources(
        &self,
        domain: PublicationDomain,
        scope: PublicationScope,
        host: EnvironmentFragment,
        sink: EnvironmentChangeSink,
        namespaces: Option<Arc<Mutex<super::state::PluginStateRegistry>>>,
    ) -> Result<EnvironmentSources, SourceCollectionError> {
        let mut sources = EnvironmentSources::default();
        let mut packages: BTreeMap<_, _> = self
            .package_providers
            .iter()
            .map(|registered| (registered.identity.clone(), registered.hook.clone()))
            .collect();
        let mut graph: BTreeMap<_, BTreeSet<_>> = self
            .tool_registry
            .keys()
            .chain(packages.keys())
            .cloned()
            .map(|key| (key, BTreeSet::new()))
            .collect();
        for (key, provider) in &packages {
            for member in provider.environment_tool_sources(key) {
                if member.registry() != key.registry() || !self.tool_registry.serves(&member) {
                    return Err(PluginError::Registration(format!(
                        "package source {key} projects unregistered tool provider {member}"
                    ))
                    .into());
                }
                graph.entry(key.clone()).or_default().insert(member.clone());
                graph.entry(member).or_default().insert(key.clone());
            }
        }
        while let Some(key) = graph.keys().next().cloned() {
            let mut pending = vec![key.clone()];
            let mut members = BTreeSet::new();
            while let Some(member) = pending.pop() {
                if let Some(neighbors) = graph.remove(&member) {
                    members.insert(member);
                    pending.extend(neighbors);
                }
            }
            let tools = members
                .iter()
                .filter(|key| self.tool_registry.serves(key))
                .cloned()
                .collect();
            let packages = members
                .iter()
                .filter_map(|key| packages.remove(key).map(|provider| (key.clone(), provider)))
                .collect();
            sources.register(
                Arc::new(ProviderSource {
                    key,
                    tools,
                    registry: self.tool_registry.clone(),
                    packages,
                    domain: domain.clone(),
                    scope: scope.clone(),
                    namespaces: namespaces.clone(),
                    subscriptions: Mutex::new(Vec::new()),
                    revision: Mutex::new(SourceRevision {
                        incarnation: lash_core_store::environment::fresh_identity(),
                        counter: 0,
                    }),
                }),
                sink.clone(),
            )?;
        }
        sources.register(
            Arc::new(ImmutableEnvironmentSource::new(
                SourceId("host".into()),
                host,
            )),
            sink,
        )?;
        Ok(sources)
    }
}
impl super::PluginSession {
    /// Called by the commit owner after sticky configuration commits, never
    /// when a run publishes its derived configuration view.
    pub fn environment_config_committed(&self, owners: &[String]) {
        self.state
            .lock_recover()
            .environment
            .config_committed(owners);
    }

    /// Create source adapters once when the session is materialized. The sink
    /// enqueues contract events in the session actor mailbox. No turn is driven.
    pub fn environment_sources(
        self: &Arc<Self>,
        sink: EnvironmentChangeSink,
    ) -> Result<EnvironmentSources, SourceCollectionError> {
        let session = self.owner.session_id().ok_or_else(|| {
            PluginError::Session("environment sources require a session owner".into())
        })?;
        let packages = &self.host.package_namespace.packages;
        let mut host = EnvironmentFragment {
            package_roots: packages.snapshot.advertisements.clone(),
            routes: packages.routes.clone(),
            consumer: Some(lash_kernel_value::package::PublicationContext {
                domain: self.host.package_namespace.domain.clone(),
                scope: PublicationScope::Session(session.clone()),
                publisher: lash_kernel_value::package::PublisherInstance(format!(
                    "session:{session}"
                )),
                root: lash_kernel_value::names::Segment::new("session")
                    .unwrap_or_else(|_| unreachable!("static root")),
            }),
            ..Default::default()
        };
        if let Some(inventory) = self.installed_inventory() {
            let offered = inventory.offers();
            host.identities.insert(
                super::RegistryKind::Session,
                lash_vocabulary::claims::ClaimRequirements {
                    machine: offered.machine,
                    providers: offered.providers,
                    codecs: offered.codecs,
                    callbacks: offered.callbacks,
                    components: offered.components,
                },
            );
        }
        if let Some(deferred) = self.composition().deferred_key() {
            host.deferred_provider = Some(deferred);
        }
        let mut sources = self.composition().environment_sources(
            self.host.package_namespace.domain.clone(),
            PublicationScope::Session(session.clone()),
            host,
            sink.clone(),
            Some(self.state.clone()),
        )?;
        self.register_catalog_source(&mut sources, sink)?;
        Ok(sources)
    }
}
/// Host/registry facts whose replacement creates a new source incarnation.
pub struct ImmutableEnvironmentSource(SourceSnapshot);
impl ImmutableEnvironmentSource {
    pub fn new(source: SourceId, fragment: EnvironmentFragment) -> Self {
        Self(SourceSnapshot {
            source,
            revision: SourceRevision {
                incarnation: lash_core_store::environment::fresh_identity(),
                counter: 0,
            },
            fragment,
        })
    }
}
impl EnvironmentSource for ImmutableEnvironmentSource {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            source: self.0.source.clone(),
            mode: SourceMode::Immutable,
        }
    }
    fn snapshot(&self, _: &OwnerConfigView) -> Result<SourceSnapshot, SourceCollectionError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests;

/// A host or package publisher installs whole immutable fragments under one
/// synchronization owner, then announces that committed observation. Holding
/// package closures remains the version owner's responsibility, not this source's.
pub struct PublishedEnvironmentSource {
    state: Mutex<PublishedSourceState>,
}
struct PublishedSourceState {
    snapshot: SourceSnapshot,
    subscribers: Vec<WeakChangeSink>,
}
impl PublishedEnvironmentSource {
    pub fn new(source: SourceId, fragment: EnvironmentFragment) -> Self {
        Self {
            state: Mutex::new(PublishedSourceState {
                snapshot: SourceSnapshot {
                    source,
                    revision: SourceRevision {
                        incarnation: lash_core_store::environment::fresh_identity(),
                        counter: 0,
                    },
                    fragment,
                },
                subscribers: Vec::new(),
            }),
        }
    }
    /// Equal republications are silent. Changed packages carry their new
    /// content/publication keys in `fragment`; no old snapshot is modified.
    pub fn publish(&self, fragment: EnvironmentFragment) -> Result<(), SourceCollectionError> {
        let mut state = self.state.lock_recover();
        if state.snapshot.fragment == fragment {
            return Ok(());
        }
        let counter = state
            .snapshot
            .revision
            .counter
            .checked_add(1)
            .ok_or_else(|| {
                SourceCollectionError::RegressedRevision(state.snapshot.source.clone())
            })?;
        state.snapshot.fragment = fragment;
        state.snapshot.revision.counter = counter;
        let change = EnvironmentChange::SourceRevision {
            source: state.snapshot.source.clone(),
            revision: state.snapshot.revision.clone(),
        };
        state.subscribers.retain(|weak| {
            if let Some(sink) = weak.upgrade() {
                sink(change.clone());
                true
            } else {
                false
            }
        });
        Ok(())
    }
}
impl EnvironmentSource for PublishedEnvironmentSource {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            source: self.state.lock_recover().snapshot.source.clone(),
            mode: SourceMode::Notifying,
        }
    }
    fn snapshot(&self, _: &OwnerConfigView) -> Result<SourceSnapshot, SourceCollectionError> {
        Ok(self.state.lock_recover().snapshot.clone())
    }
    fn subscribe(&self, sink: &EnvironmentChangeSink) {
        self.state
            .lock_recover()
            .subscribers
            .push(Arc::downgrade(sink));
    }
}
