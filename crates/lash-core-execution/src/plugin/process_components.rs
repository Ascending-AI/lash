//! Process components (FIG-6034; final-spec §3.2).
//!
//! A process is built only from the components its environment names, in
//! the environment's order, from a registry of their own: a session builds
//! session plugins, a process builds process components, and both share the
//! execution building blocks (one dispatch, one hook pipeline, one
//! presentation pipeline and one prompt composer). A component never sees a
//! session: its context holds the environment, its own entry, the capability
//! refs and the ports its entry grants.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_store::process_env::{
    ProcessComponentEntry, ProcessComponentId, ProcessConfigKey, ProcessEnv, ProcessEnvRefusal,
};
use lash_core_store::run_spec::{CapabilityRef, SlotId};

use super::prompt::{PromptSection, PromptSectionSpec};
use super::registrar::PluginContributions;
use super::*;
use crate::{ProcessId, SessionId, ToolProvider};

/// What a component declares about itself: its id and formats for stored state and config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessComponentDeclaration {
    pub id: ProcessComponentId,
    /// The format of stored state and config the component reads natively.
    pub format_version: FormatVersion,
    /// Every state and config format it reads. Contains `format_version`.
    pub readable_formats: Vec<FormatVersion>,
    /// Every state and config format it writes. Contains `format_version`.
    pub writable_formats: Vec<FormatVersion>,
}

impl ProcessComponentDeclaration {
    /// The declaration of a component that reads and writes only `format_version`.
    ///
    /// # Errors
    ///
    /// The id is refused.
    pub fn initial(id: &str, format_version: FormatVersion) -> Result<Self, PluginError> {
        Ok(Self {
            id: ProcessComponentId::new(id)
                .map_err(|error| PluginError::Registration(error.to_string()))?,
            format_version,
            readable_formats: vec![format_version],
            writable_formats: vec![format_version],
        })
    }

    /// The claim requirements a process under `env` carries from its
    /// start: one format token for every component `env` names.
    pub fn env_requirements(env: &ProcessEnv) -> BTreeSet<crate::ObligationToken> {
        env.components()
            .iter()
            .map(|entry| {
                crate::ObligationToken::for_component(entry.id(), entry.config().format_version())
            })
            .collect()
    }
}

/// A process component as a host installs it.
pub trait ProcessComponentFactory: Send + Sync {
    fn declaration(&self) -> ProcessComponentDeclaration;

    /// Pure whole-namespace conversion into the component's native format.
    fn migrate_format(
        &self,
        from: FormatVersion,
        namespace: FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, FormatRefusal> {
        if from == self.declaration().format_version {
            Ok(value)
        } else {
            Err(FormatRefusal {
                plugin: self.declaration().id.to_string(),
                namespace,
                stored: from,
                readable: self.declaration().format_version,
            })
        }
    }

    /// Pure encoding into a declared writer format. Native encoding is identity.
    fn encode_format(
        &self,
        to: FormatVersion,
        namespace: FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, FormatRefusal> {
        if to == self.declaration().format_version {
            Ok(value.clone())
        } else {
            Err(FormatRefusal {
                plugin: self.declaration().id.to_string(),
                namespace,
                stored: to,
                readable: self.declaration().format_version,
            })
        }
    }

    /// The type its config decodes as; checked against the key it is
    /// registered under.
    fn config_type(&self) -> TypeId;

    /// Judge this component's entry in `env`. Pure; called at admission.
    ///
    /// # Errors
    ///
    /// The typed refusal the publish answers with.
    fn validate(
        &self,
        entry: &ProcessComponentEntry,
        env: &ProcessEnv,
    ) -> Result<(), ProcessEnvRefusal>;

    /// The component's contributions. Side-effect free: live services bind
    /// lazily through `cx.ports`. Called with `process: None` at admission
    /// and for a catalog preview.
    ///
    /// # Errors
    ///
    /// The component cannot build under `cx`.
    fn recipe(self: Arc<Self>) -> std::result::Result<ProcessRecipe, PluginError>;
}

/// What one component builds under.
pub struct ProcessComponentContext<'a> {
    /// The process it builds for; `None` at admission or a catalog preview.
    pub process: Option<&'a ProcessId>,
    pub env: &'a ProcessEnv,
    /// This component's entry; decode its config with `entry.decode(KEY)`.
    pub entry: &'a ProcessComponentEntry,
    /// The env's capability refs.
    pub capabilities: &'a BTreeMap<SlotId, CapabilityRef>,
    pub ports: &'a ProcessComponentPorts,
    /// This component's step-state namespace in `process` (FIG-5268): what
    /// its committed step outcomes published. `None` with no process.
    pub state: Option<PluginStateView>,
}

/// The host's read of named sessions a component's entry may be granted
/// (D17).
pub trait ProcessSessionReads: Send + Sync {
    /// A read service for `session`.
    ///
    /// # Errors
    ///
    /// The session cannot be read.
    fn session(&self, session: &SessionId) -> Result<Arc<dyn SessionReadService>, PluginError>;
}

/// The host services every component of a deployment builds against.
#[derive(Clone)]
pub struct ProcessComponentHost {
    pub tracing: crate::trace::TraceRuntime,
    pub extensions: PluginExtensions,
    pub sessions: Option<Arc<dyn ProcessSessionReads>>,
}

impl ProcessComponentHost {
    pub fn new(tracing: crate::trace::TraceRuntime) -> Self {
        Self {
            tracing,
            extensions: PluginExtensions::default(),
            sessions: None,
        }
    }

    #[must_use]
    pub fn with_session_reads(mut self, sessions: Arc<dyn ProcessSessionReads>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// The ports `entry` grants.
    pub fn ports(&self, entry: &ProcessComponentEntry) -> ProcessComponentPorts {
        ProcessComponentPorts {
            tracing: self.tracing.clone(),
            extensions: self.extensions.clone(),
            granted: entry.sessions().clone(),
            reads: self.sessions.clone(),
        }
    }
}

/// The live services a component binds lazily.
pub struct ProcessComponentPorts {
    pub tracing: crate::trace::TraceRuntime,
    pub extensions: PluginExtensions,
    granted: BTreeSet<SessionId>,
    reads: Option<Arc<dyn ProcessSessionReads>>,
}

impl ProcessComponentPorts {
    /// A read service for `session` when this component's env entry grants
    /// it (D17).
    ///
    /// # Errors
    ///
    /// The entry does not grant `session`, or the host serves no session
    /// reads.
    pub fn session(&self, session: &SessionId) -> Result<Arc<dyn SessionReadService>, PluginError> {
        if !self.granted.contains(session) {
            return Err(PluginError::Session(format!(
                "this component's environment entry grants no read of session `{session}`"
            )));
        }
        let Some(reads) = self.reads.as_ref() else {
            return Err(PluginError::Session(
                "this deployment serves no session reads to process components".into(),
            ));
        };
        reads.session(session)
    }
}

/// Ergonomic static component entries in the shared registration model.
#[derive(Clone, Default)]
pub struct ProcessComponentSpec {
    pub(crate) registrations: PluginRegistrar<(), registries::Process>,
    pub(crate) errors: Vec<PluginError>,
}
impl ProcessComponentSpec {
    pub fn new() -> Self {
        Self::default()
    }
    fn admit(&mut self, result: Result<(), PluginError>) {
        if let Err(error) = result {
            self.errors.push(error);
        }
    }
    pub fn with_tool_provider(
        mut self,
        key: ToolProviderKey,
        provider: Arc<dyn ToolProvider>,
    ) -> Self {
        let result = self
            .registrations
            .tools()
            .record_provider(key, move |_, _| Ok(Arc::clone(&provider)));
        self.admit(result);
        self
    }
    pub fn with_prompt_section(
        mut self,
        spec: PromptSectionSpec,
        section: Arc<dyn PromptSection>,
    ) -> Self {
        let result = self
            .registrations
            .prompt()
            .section(spec, move |_, _| Ok(Arc::clone(&section)));
        self.admit(result);
        self
    }
    pub fn with_state_reducer(mut self, name: impl Into<String>, reducer: StateReducer) -> Self {
        let result = self
            .registrations
            .state_reducer(name, move |_, _| Ok(Arc::clone(&reducer)));
        self.admit(result);
        self
    }
    pub fn recipe(
        self,
        owner: &str,
    ) -> std::result::Result<ProcessRecipe, crate::plugin::PluginError> {
        if let Some(error) = self.errors.first() {
            return Err(error.clone());
        }
        Ok(ProcessRecipe::from_spec(owner, self.registrations))
    }
}

pub(super) struct RegisteredComponent {
    pub(super) factory: Arc<dyn ProcessComponentFactory>,
    pub(super) declaration: ProcessComponentDeclaration,
    recipe: Arc<ProcessRecipe>,
}

/// The components a deployment installs, by id, and the host services they
/// build against. Separate from the session plugin registry.
///
/// It also holds the components the deployment declares retired
/// (FIG-6296): installed nowhere any more, and named only by sealed envs.
/// A new env that names one is refused
/// ([`ProcessEnvRefusal::ComponentRetired`]). A sealed env that names one
/// builds without it, unless its run still depends on it: an effect the
/// env links to one of its providers would have no body
/// ([`ProcessEnvRefusal::RetiredComponentServes`]), and state it published
/// would be read by nothing
/// ([`ProcessEnvRefusal::RetiredComponentHasState`]). What it decided before
/// it was retired, its hooks' and presenters' part in a call, is recorded
/// with the call's outcome, and a resumed run reads the record.
#[derive(Clone)]
pub struct ProcessComponentRegistry {
    pub(super) components: Arc<BTreeMap<String, Arc<RegisteredComponent>>>,
    retired: super::registries::RegistryRetirements<
        super::registries::Process,
        ProcessComponentDeclaration,
    >,
    host: ProcessComponentHost,
    materialized_inventories: super::MaterializedInventoryReader,
}

impl Default for ProcessComponentRegistry {
    fn default() -> Self {
        Self {
            components: Arc::default(),
            retired: Default::default(),
            materialized_inventories: Default::default(),
            host: ProcessComponentHost::new(crate::trace::TraceRuntime::default()),
        }
    }
}

impl std::fmt::Debug for ProcessComponentRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_set()
            .entries(self.components.keys())
            .finish()
    }
}

impl ProcessComponentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// This registry, building its components against `host`.
    #[must_use]
    pub fn with_host(mut self, host: ProcessComponentHost) -> Self {
        self.host = host;
        self
    }

    pub fn host(&self) -> &ProcessComponentHost {
        &self.host
    }

    /// Check that these components serve every link of `env` (P1): the
    /// link's provider, built under the env's config, serves its tool with
    /// the surface the link retained, a `call_dynamic` link's provider is a
    /// component's deferred provider, which matches its target per call,
    /// and a resolved link's provider serves the definition it retains under
    /// a binding a component reads.
    ///
    /// # Errors
    ///
    /// [`ProcessEnvRefusal::ToolNotServed`],
    /// [`ProcessEnvRefusal::LinkedSurfaceChanged`] or
    /// [`ProcessEnvRefusal::BindingUnreadable`] naming the effect, or the
    /// refusal of a component that does not compose.
    pub fn check_served(&self, env: &Arc<ProcessEnv>) -> Result<(), ProcessEnvRefusal> {
        if env.effects().is_empty() {
            return Ok(());
        }
        let composition = self.compose(env, None)?;
        for (effect, link) in env.effects() {
            match composition.step_link(env, effect) {
                StepLink::Served { .. } => {}
                StepLink::Unreadable { codec, reason } => {
                    return Err(ProcessEnvRefusal::BindingUnreadable {
                        effect: effect.to_string(),
                        tool: link.tool().to_string(),
                        codec: codec.to_string(),
                        reason,
                    });
                }
                StepLink::Drifted {
                    kind: crate::ToolSurfaceDriftKind::Changed,
                    ..
                } => {
                    return Err(ProcessEnvRefusal::LinkedSurfaceChanged {
                        effect: effect.to_string(),
                        tool: link.tool().to_string(),
                    });
                }
                StepLink::Drifted { .. } | StepLink::Unlinked => {
                    return Err(ProcessEnvRefusal::ToolNotServed {
                        effect: effect.to_string(),
                        tool: link.tool().to_string(),
                        provider: format!("{}/{}", link.provider.owner(), link.provider.key()),
                    });
                }
            }
        }
        Ok(())
    }

    /// Install `factory` under `key`.
    ///
    /// # Errors
    ///
    /// A key naming another component than the declaration, a config type
    /// other than the key's, a duplicate id, or a declaration whose formats
    /// do not contain its own native format.
    pub fn register<T: 'static>(
        &mut self,
        factory: Arc<dyn ProcessComponentFactory>,
        key: ProcessConfigKey<T>,
    ) -> Result<(), PluginError> {
        let declaration = factory.declaration();
        if declaration.id.as_str() != key.component() {
            return Err(PluginError::Registration(format!(
                "process component `{}` registered under the config key of `{}`",
                declaration.id,
                key.component()
            )));
        }
        if factory.config_type() != TypeId::of::<T>() {
            return Err(PluginError::Registration(format!(
                "process component `{}` decodes a config type other than its key's",
                declaration.id
            )));
        }
        if key.format() != declaration.format_version
            || !declaration
                .readable_formats
                .contains(&declaration.format_version)
            || !declaration
                .writable_formats
                .contains(&declaration.format_version)
        {
            return Err(PluginError::Registration(format!(
                "process component `{}` declares formats that do not contain its native format",
                declaration.id
            )));
        }
        if self.components.contains_key(declaration.id.as_str()) {
            return Err(PluginError::Registration(format!(
                "duplicate process component `{}`",
                declaration.id
            )));
        }
        self.retired.check_installation(declaration.id.as_str())?;
        let mut recipe = Arc::clone(&factory).recipe()?;
        if recipe
            .entries
            .iter()
            .any(|entry| entry.key.owner() != declaration.id.as_str())
        {
            return Err(PluginError::Registration(format!(
                "component `{}` declared another owner",
                declaration.id
            )));
        }
        recipe.entries.push(ContributionDescriptor {
            registry: RegistryKind::Process,
            kind: ContributionKind::ConfigNamespace,
            key: ContributionKey::Named {
                owner: declaration.id.to_string(),
                key: key.component().into(),
            },
            registration_order: u32::try_from(recipe.entries.len()).unwrap_or(u32::MAX),
            priority: None,
            details: ContributionDetails::Named,
        });
        let recipe = Arc::new(recipe);
        let mut components = (*self.components).clone();
        components.insert(
            declaration.id.as_str().to_owned(),
            Arc::new(RegisteredComponent {
                factory,
                declaration,
                recipe,
            }),
        );
        self.components = Arc::new(components);
        Ok(())
    }

    /// Declare the component `declaration` names retired (FIG-6296): this
    /// deployment installs it nowhere any more. `declaration` is the one it
    /// was installed under: a sealed env may name it at any format it reads,
    /// and this node serves the claim token of each, so such an env's
    /// processes are still claimed here and build without it.
    ///
    /// # Errors
    ///
    /// The component is installed, or already retired.
    pub fn retire(&mut self, declaration: ProcessComponentDeclaration) -> Result<(), PluginError> {
        let id = declaration.id.to_string();
        self.retired
            .retire(&id, declaration, self.components.contains_key(&id))
    }

    /// The components `env` names that this registry declares retired, in
    /// env order: what a process under `env` builds without.
    pub fn retired_in(&self, env: &ProcessEnv) -> Vec<ProcessComponentId> {
        env.components()
            .iter()
            .filter(|entry| self.retired.contains_key(entry.id().as_str()))
            .map(|entry| entry.id().clone())
            .collect()
    }

    /// `env` without the components this registry declares retired: what a
    /// child restricted from a sealed env names, since a new env may not
    /// name a retired component.
    pub fn without_retired(&self, env: ProcessEnv) -> ProcessEnv {
        if self.retired_in(&env).is_empty() {
            return env;
        }
        let entries = env
            .components()
            .iter()
            .filter(|entry| !self.retired.contains_key(entry.id().as_str()))
            .cloned()
            .collect();
        env.with_component_entries(entries)
    }

    /// `env` as this registry builds it, from the recorded `state` of its
    /// process: without its retired components, each checked to leave
    /// nothing the run depends on.
    ///
    /// # Errors
    ///
    /// The env links an effect to a retired component's provider, or a
    /// retired component holds recorded state.
    fn sealed(
        &self,
        env: &ProcessEnv,
        state: &PluginState,
    ) -> Result<ProcessEnv, ProcessEnvRefusal> {
        for id in self.retired_in(env) {
            if let Some(effect) = env.effects().iter().find_map(|(effect, link)| {
                (link.provider.registry() == crate::RegistryKind::Process
                    && link.provider.owner() == id.as_str())
                .then_some(effect)
            }) {
                return Err(ProcessEnvRefusal::RetiredComponentServes {
                    id: id.to_string(),
                    effect: effect.to_string(),
                });
            }
            if state
                .plugins
                .get(id.as_str())
                .is_some_and(|namespace| !namespace.values.is_empty())
            {
                return Err(ProcessEnvRefusal::RetiredComponentHasState { id: id.to_string() });
            }
        }
        Ok(self.without_retired(env.clone()))
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    /// The installed components' declarations, by id.
    pub fn declarations(&self) -> impl Iterator<Item = &ProcessComponentDeclaration> {
        self.components
            .values()
            .map(|registered| &registered.declaration)
    }

    pub fn declaration(&self, id: &str) -> Option<&ProcessComponentDeclaration> {
        self.components
            .get(id)
            .map(|registered| &registered.declaration)
    }

    /// The claim tokens this registry serves (§6.2, S7): every state and config
    /// format each component reads, and the callback tokens of its
    /// installed registrations.
    pub fn served_tokens(&self) -> BTreeSet<crate::ObligationToken> {
        let mut served = self.installed_inventory().served_tokens();
        served.extend(self.retired.served_tokens());
        for registered in self
            .components
            .values()
            .filter(|registered| self.preflight_component(registered).is_ok())
        {
            let declaration = &registered.declaration;
            for format in &declaration.readable_formats {
                served.insert(crate::ObligationToken::for_component(
                    &declaration.id,
                    *format,
                ));
            }
        }
        for declaration in self.retired.values() {
            for format in &declaration.readable_formats {
                served.insert(crate::ObligationToken::for_component(
                    &declaration.id,
                    *format,
                ));
            }
        }
        served
    }

    pub fn materialized_inventories(&self) -> super::MaterializedInventoryReader {
        self.materialized_inventories.clone()
    }

    /// Validate unconditional deployment ports without admitting an environment or binding an owner.
    pub fn preflight_deployment(&self) -> Result<(), PluginError> {
        for registered in self.components.values() {
            self.preflight_component(registered)
                .map_err(PluginError::Preflight)?;
        }
        Ok(())
    }

    fn preflight_component(&self, registered: &RegisteredComponent) -> Result<(), PreflightIssue> {
        for service in &registered.recipe.requirements.services {
            if *service != ServiceKind::GrantedSessionReads || self.host.sessions.is_none() {
                return Err(PreflightIssue::MissingService {
                    owner: registered.declaration.id.to_string(),
                    service: *service,
                });
            }
        }
        Ok(())
    }

    pub fn installed_inventory(&self) -> InstalledInventory {
        let mut entries = Vec::new();
        let mut readiness = Vec::new();
        for registered in self.components.values() {
            let ready = self
                .preflight_component(registered)
                .map_or_else(ContributionReadiness::Refused, |_| {
                    ContributionReadiness::Ready
                });
            for mut entry in registered.recipe.entries.clone() {
                entry.registration_order = u32::try_from(entries.len()).unwrap_or(u32::MAX);
                entries.push(entry);
                readiness.push(ready.clone());
            }
        }
        InstalledInventory {
            scope: InventoryScope::Deployment(crate::RegistryKind::Process),
            entries,
            readiness,
            stateless: Default::default(),
        }
    }

    /// What each installed component declares about the config formats it
    /// writes, keyed `component:<id>` (§2.6): what the fleet record's writer
    /// ranges for components are provisioned from, as a session plugin's are
    /// from its declaration.
    pub fn writer_registrations(
        &self,
    ) -> Vec<crate::store::plugin_writers::PluginWriterRegistration> {
        self.declarations()
            .map(
                |declaration| crate::store::plugin_writers::PluginWriterRegistration {
                    plugin: declaration.id.writer_key(),
                    native: declaration.format_version,
                    writable: declaration.writable_formats.clone(),
                },
            )
            .collect()
    }

    /// Admission steps 2–7 of a new environment (§3.3): unique ids,
    /// installed components that are not retired, readable formats, each
    /// `validate`, each `build(process: None)`, and a composable set. Ambient tools are
    /// refused first (D7). The fleet writer range (step 8) is the store's,
    /// checked at publication.
    ///
    /// # Errors
    ///
    /// The first refusal, typed.
    pub fn admit(&self, env: &ProcessEnv) -> Result<(), ProcessEnvRefusal> {
        env.check_shape()?;
        for entry in env.components() {
            if self.retired.contains_key(entry.id().as_str()) {
                return Err(ProcessEnvRefusal::ComponentRetired {
                    id: entry.id().to_string(),
                });
            }
            let registered = self.components.get(entry.id().as_str()).ok_or_else(|| {
                ProcessEnvRefusal::ComponentNotInstalled {
                    id: entry.id().to_string(),
                }
            })?;
            let format = entry.config().format_version();
            if !registered.declaration.readable_formats.contains(&format) {
                return Err(ProcessEnvRefusal::ConfigFormatUnreadable {
                    id: entry.id().to_string(),
                    format: format.get(),
                    readable: registered
                        .declaration
                        .readable_formats
                        .iter()
                        .map(|format| format.get())
                        .collect(),
                });
            }
        }
        for entry in env.components() {
            self.component(entry)?.factory.validate(entry, env)?;
        }
        self.contributions(env, None, &Arc::default()).map(drop)
    }

    fn component(
        &self,
        entry: &ProcessComponentEntry,
    ) -> Result<&Arc<RegisteredComponent>, ProcessEnvRefusal> {
        self.components.get(entry.id().as_str()).ok_or_else(|| {
            ProcessEnvRefusal::ComponentNotInstalled {
                id: entry.id().to_string(),
            }
        })
    }

    /// Build every component `env` names, in env order, and register their
    /// contributions through the same registrar a session plugin's go
    /// through, each owned by the component's id.
    fn contributions(
        &self,
        env: &ProcessEnv,
        process: Option<&ProcessId>,
        state: &Arc<std::sync::Mutex<state::PluginStateRegistry>>,
    ) -> Result<BuiltComponents, ProcessEnvRefusal> {
        let mut contributions = PluginContributions::default();
        let mut owners = Vec::with_capacity(env.components().len());
        let mut presenter = None;
        for entry in env.components() {
            let registered = self.component(entry)?;
            for service in &registered.recipe.requirements.services {
                if *service != ServiceKind::GrantedSessionReads
                    || self.host.sessions.is_none()
                    || entry.sessions().is_empty()
                {
                    return Err(ProcessEnvRefusal::PluginPreflight {
                        issue: PreflightIssue::MissingService {
                            owner: entry.id().to_string(),
                            service: *service,
                        },
                    });
                }
            }
            if registered.recipe.entries.iter().any(|entry| {
                entry.kind == ContributionKind::Callback(CallbackSlot::PresentationPresenter)
            }) {
                if let Some(first) = presenter {
                    return Err(ProcessEnvRefusal::DuplicatePresenter {
                        first,
                        second: entry.id().to_string(),
                    });
                }
                presenter = Some(entry.id().to_string());
            }
        }
        let entries = env
            .components()
            .iter()
            .map(|entry| self.component(entry))
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .flat_map(|registered| registered.recipe.entries.clone())
            .enumerate()
            .map(|(index, mut entry)| {
                entry.registration_order = u32::try_from(index).unwrap_or(u32::MAX);
                entry
            })
            .collect::<Vec<_>>();
        let inventory = InstalledInventory {
            scope: process.map_or(
                InventoryScope::Deployment(crate::RegistryKind::Process),
                |process| InventoryScope::Owner(crate::RuntimeOwner::Process(process.clone())),
            ),
            readiness: vec![ContributionReadiness::Ready; entries.len()],
            entries,
            stateless: Default::default(),
        };
        contributions.inventory = Some(Arc::new(inventory));
        for entry in env.components() {
            let registered = self.component(entry)?;
            let id = entry.id().to_string();
            let ports = self.host.ports(entry);
            let state_view = match process {
                Some(process) => PluginStateView::bind(
                    &crate::RuntimeOwner::Process(process.clone()),
                    &id,
                    Arc::clone(state),
                ),
                None => PluginStateView::detached(&id, Arc::clone(state)),
            };
            let mut reg = BoundRegistrar::for_process(id.clone());
            reg.contributions = contributions;
            registered
                .recipe
                .bind(
                    &ProcessComponentContext {
                        process,
                        env,
                        entry,
                        capabilities: env.capabilities(),
                        ports: &ports,
                        state: process.map(|_| state_view.clone()),
                    },
                    &state_view,
                    &mut reg,
                )
                .map_err(|error| ProcessEnvRefusal::ComponentBuildFailed {
                    id: id.clone(),
                    reason: error.to_string(),
                })?;
            contributions = reg.contributions;
            owners.push(id);
        }
        let parts = ProcessParts::consume(&mut contributions).map_err(|error| {
            ProcessEnvRefusal::ComponentBuildFailed {
                id: owners.last().cloned().unwrap_or_default(),
                reason: error.to_string(),
            }
        })?;
        Ok(BuiltComponents { owners, parts })
    }

    /// The execution composition `env`'s components make for `process`
    /// (§5.1): only the components the env names, in env order, without
    /// the ones this registry declares retired.
    ///
    /// # Errors
    ///
    /// A component is not installed here, or it did not build, or the run
    /// depends on a retired one.
    pub fn compose(
        &self,
        env: &ProcessEnv,
        process: Option<&ProcessId>,
    ) -> Result<ProcessComposition, ProcessEnvRefusal> {
        let mut snapshot = PluginState::default();
        for entry in env.components() {
            snapshot.plugins.insert(
                entry.id().to_string(),
                PluginNamespaceState {
                    format_version: entry.config().format_version(),
                    ..Default::default()
                },
            );
        }
        self.compose_with_state(env, process, &snapshot)
    }

    /// Build from a recorded whole component view, before any binder sees it.
    #[doc(hidden)]
    pub fn compose_with_state(
        &self,
        env: &ProcessEnv,
        process: Option<&ProcessId>,
        snapshot: &PluginState,
    ) -> Result<ProcessComposition, ProcessEnvRefusal> {
        let env = &self.sealed(env, snapshot)?;
        let state = Arc::new(std::sync::Mutex::new(
            state::PluginStateRegistry::from_snapshot(Some(snapshot)),
        ));
        let BuiltComponents { owners, parts } = self.contributions(env, process, &state)?;
        if let Some(process) = process
            && let Some(inventory) = &parts.inventory
        {
            self.materialized_inventories
                .observe(&crate::RuntimeOwner::Process(process.clone()), inventory);
        }
        Ok(ProcessComposition {
            composition: Arc::new(parts.composition),
            registry: parts.registry,
            owners,
            reducers: parts.reducers,
            formats: env
                .components()
                .iter()
                .map(|entry| {
                    Ok((
                        entry.id().to_string(),
                        self.component(entry)?.declaration.format_version,
                    ))
                })
                .collect::<Result<_, ProcessEnvRefusal>>()?,
            state,
        })
    }
}

struct BuiltComponents {
    owners: Vec<String>,
    parts: ProcessParts,
}

/// What a process runs of its components' bound contributions: the tool
/// registry their providers, deferred provider and readers route through,
/// the execution composition (the catalog contributors, the four tool
/// hooks, the presentation pipeline and the prompt sections), and the state
/// reducers. It takes exactly what it runs and leaves the rest.
pub(super) struct ProcessParts {
    inventory: Option<Arc<InstalledInventory>>,
    registry: Arc<crate::ToolRegistry>,
    composition: ExecutionComposition,
    reducers: BTreeMap<String, BTreeMap<String, StateReducer>>,
}

impl ProcessParts {
    pub(super) fn consume(contributions: &mut PluginContributions) -> Result<Self, PluginError> {
        let registry = super::runtime_impl::build_tool_registry(contributions)?;
        Ok(Self {
            inventory: contributions.inventory.clone(),
            reducers: std::mem::take(&mut contributions.state_reducers),
            composition: ExecutionComposition::new(contributions, Arc::clone(&registry)),
            registry,
        })
    }
}

/// What a process's components make: its execution composition, the tool
/// registry its providers route through, its owners in env order, and what
/// its step state reduces and admits by (FIG-5268).
pub struct ProcessComposition {
    pub composition: Arc<ExecutionComposition>,
    pub registry: Arc<crate::ToolRegistry>,
    pub owners: Vec<String>,
    pub(crate) reducers: BTreeMap<String, BTreeMap<String, StateReducer>>,
    pub(crate) formats: BTreeMap<String, FormatVersion>,
    /// The step state the components' views read (FIG-5268).
    pub(in crate::plugin) state: Arc<std::sync::Mutex<state::PluginStateRegistry>>,
}

/// How a node serves one linked call of a process (D3).
#[derive(Clone, Debug)]
pub enum StepLink {
    /// The env links no such effect.
    Unlinked,
    /// The link's provider serves the tool as the env retained it: the call
    /// is admitted under `manifest` and binds `binding`.
    Served {
        manifest: Box<ToolManifest>,
        binding: Box<crate::tool_run::AdmittedBinding>,
    },
    /// The link's provider does not serve the tool, or serves it changed
    /// since the env retained its surface: the call settles typed, without
    /// a body.
    Drifted {
        tool: crate::ToolId,
        kind: crate::ToolSurfaceDriftKind,
    },
    /// A resolved link's binding cannot be read here (S8): its codec is not
    /// registered, or does not read its format. The call's process parks
    /// before it is admitted.
    Unreadable {
        codec: crate::ProviderKey,
        reason: String,
    },
}

impl ProcessComposition {
    /// How this node serves the linked call of `effect` under `env` (D3):
    /// the link's provider, by its key, serving the linked tool with the
    /// surface the link retained, judged now, before the call's `x_start`.
    /// A resolved link's provider serves the definition the link retains,
    /// advertised or not, once this node reads its binding.
    #[must_use]
    pub fn step_link(
        &self,
        env: &Arc<crate::ProcessEnv>,
        effect: &lash_kernel_doc::EffectName,
    ) -> StepLink {
        let Some(link) = env.effects().get(effect) else {
            return StepLink::Unlinked;
        };
        let config = super::OwnerConfigView::components(Arc::clone(env));
        let drifted = |kind| StepLink::Drifted {
            tool: link.tool().clone(),
            kind,
        };
        let dynamic = link.kind == crate::LinkKind::Dynamic;
        if dynamic != (effect.as_str() == crate::CALL_DYNAMIC_EFFECT)
            || (dynamic
                && link.surface != crate::ProcessSurface::of(&crate::call_dynamic_endpoint()))
        {
            return drifted(crate::ToolSurfaceDriftKind::Changed);
        }
        // The `call_dynamic` endpoint is no provider's advertisement: it is
        // served wherever the deferred provider it links is registered.
        if link.kind == crate::LinkKind::Dynamic {
            if self.registry.deferred_provider(&link.provider).is_none() {
                return drifted(crate::ToolSurfaceDriftKind::Missing);
            }
            return match self
                .composition
                .tool_run_binding(link.tool(), Some(&link.provider))
            {
                Ok(binding) => StepLink::Served {
                    manifest: Box::new(crate::call_dynamic_endpoint().manifest),
                    binding: Box::new(binding),
                },
                Err(_) => drifted(crate::ToolSurfaceDriftKind::Missing),
            };
        }
        let definition = match &link.kind {
            crate::LinkKind::Resolved(resolved) => {
                if crate::ProcessSurface::of(&resolved.definition) != link.surface {
                    return drifted(crate::ToolSurfaceDriftKind::Changed);
                }
                if let Some(binding) = &resolved.binding
                    && let Err(reason) = self.check_binding(binding)
                {
                    return StepLink::Unreadable {
                        codec: binding.codec.clone(),
                        reason,
                    };
                }
                resolved.definition.clone()
            }
            crate::LinkKind::Tool | crate::LinkKind::Dynamic => {
                let Some(definition) = self.registry.served(&link.provider, link.tool(), &config)
                else {
                    return drifted(crate::ToolSurfaceDriftKind::Missing);
                };
                if crate::ProcessSurface::of(&definition) != link.surface {
                    return drifted(crate::ToolSurfaceDriftKind::Changed);
                }
                definition
            }
        };
        match self
            .composition
            .tool_run_binding(link.tool(), Some(&link.provider))
        {
            Ok(binding) => StepLink::Served {
                manifest: Box::new(definition.manifest),
                binding: Box::new(binding),
            },
            Err(_) => drifted(crate::ToolSurfaceDriftKind::Missing),
        }
    }

    /// How this node serves the call of the tool `matched` resolved a
    /// `call_dynamic` path to (P6): the provider the match records, by its
    /// key, bound as the match routes it. One not registered here settles
    /// the call typed, without a body, as a missing linked tool does (D3).
    #[must_use]
    pub fn matched_link(&self, matched: &crate::DeferredMatch) -> StepLink {
        let Some((provider, definition, _)) = matched.resolved() else {
            return StepLink::Unlinked;
        };
        match self
            .composition
            .tool_run_binding(&definition.manifest.id, Some(provider))
        {
            Ok(binding) => StepLink::Served {
                manifest: Box::new(definition.manifest.clone()),
                binding: Box::new(binding),
            },
            Err(_) => StepLink::Drifted {
                tool: definition.manifest.id.clone(),
                kind: crate::ToolSurfaceDriftKind::Missing,
            },
        }
    }

    /// Check that this node reads the execution binding `matched` routes
    /// its call with, before the call is admitted (S8): its codec is a
    /// provider registered here that declares the binding's format.
    ///
    /// # Errors
    ///
    /// Why the binding cannot be read here: an unserved codec, or a format
    /// it does not read. The call's process parks.
    pub fn check_matched_binding(&self, matched: &crate::DeferredMatch) -> Result<(), String> {
        let Some((_, _, Some(binding))) = matched.resolved() else {
            return Ok(());
        };
        self.check_binding(binding)
    }

    /// Check that `binding`'s codec is a provider registered here that
    /// reads its format (S8).
    fn check_binding(&self, binding: &crate::ExecutionBinding) -> Result<(), String> {
        let readable = self
            .registry
            .provider(&binding.codec)
            .map(|provider| provider.readable_binding_formats())
            .ok_or_else(|| {
                format!(
                    "the execution binding's codec `{}` is not served on this node",
                    binding.codec
                )
            })?;
        binding
            .check_readable(readable)
            .map_err(|unreadable| unreadable.to_string())
    }

    /// Resolve `path`, the call path of a `call_dynamic` call `owner` makes
    /// under `env`, through the deferred provider `env` links the endpoint
    /// to, under `env`'s config (P6). Resolution is read-only discovery: a
    /// path the provider does not answer is unavailable. Host notes and
    /// refusal reasons are preserved verbatim.
    ///
    /// # Errors
    ///
    /// `env` links no `call_dynamic` endpoint, or its deferred provider is
    /// not registered here.
    pub async fn resolve_dynamic(
        &self,
        env: &Arc<crate::ProcessEnv>,
        owner: &crate::ExecutionOwner,
        path: &str,
    ) -> Result<crate::DeferredMatch, PluginError> {
        let link = env
            .effects()
            .get(
                &lash_kernel_doc::EffectName::new(crate::CALL_DYNAMIC_EFFECT)
                    .map_err(|error| PluginError::Registration(error.to_string()))?,
            )
            .filter(|link| {
                link.kind == crate::LinkKind::Dynamic
                    && link.surface == crate::ProcessSurface::of(&crate::call_dynamic_endpoint())
            })
            .ok_or_else(|| {
                PluginError::Registration("the process env links no call_dynamic endpoint".into())
            })?;
        let config = super::OwnerConfigView::components(Arc::clone(env));
        config.validate_provider(&link.provider)?;
        let provider = self
            .registry
            .deferred_provider(&link.provider)
            .ok_or_else(|| {
                PluginError::Registration(format!(
                    "the deferred provider `{}` the env links is not served on this node",
                    link.provider
                ))
            })?;
        let cx = crate::DeferredResolveContext::new(
            &link.provider,
            owner,
            None,
            env.capabilities(),
            &config,
        );
        let outcome = provider
            .resolve(&cx, &[path])
            .await
            .remove(path)
            .unwrap_or(crate::DeferredOutcome::Unavailable { note: None });
        if let crate::DeferredOutcome::Resolved {
            provider, binding, ..
        } = &outcome
        {
            config.validate_provider(provider)?;
            if let Some(binding) = binding {
                config.validate_provider(&binding.codec)?;
            }
        }
        Ok(crate::DeferredMatch {
            requested: path.to_owned(),
            outcome,
        })
    }

    /// The tools a process's linked calls run on, as this node's components
    /// serve them under `env`'s config (FIG-6074): each distinct tool an
    /// effect is linked to whose served projection is the surface its link
    /// retains, routed to its link's provider. A linked tool this node does
    /// not serve, or serves with another surface, is left out: a call of it
    /// settles as a typed tool failure when its step is admitted, before any
    /// body starts. The components' catalog contributors run over the
    /// linked tools as `owner`'s, under `extensions`: one that refuses
    /// refuses the catalog.
    ///
    /// # Errors
    ///
    /// A contributor refuses, or a served tool does not build into a
    /// catalog.
    pub fn linked_tools(
        &self,
        env: &Arc<crate::ProcessEnv>,
        owner: crate::RuntimeOwner,
        extensions: &PluginExtensions,
        isolation_engines: BTreeSet<String>,
    ) -> Result<(crate::tool_registry::SurfaceTools, crate::ToolCatalog), PluginError> {
        let config = super::OwnerConfigView::components(Arc::clone(env));
        let mut entries = BTreeMap::<crate::ToolId, crate::SurfaceEntry>::new();
        for link in env.effects().values() {
            if entries.contains_key(link.tool()) {
                continue;
            }
            let Some(definition) = self.registry.served(&link.provider, link.tool(), &config)
            else {
                continue;
            };
            if crate::ProcessSurface::of(&definition) != link.surface {
                continue;
            }
            entries.insert(
                link.tool().clone(),
                crate::SurfaceEntry {
                    provider: link.provider.clone(),
                    definition,
                },
            );
        }
        let definitions = entries
            .values()
            .map(|entry| entry.definition.clone())
            .collect::<Vec<_>>();
        let manifests = definitions
            .iter()
            .map(|definition| definition.manifest.clone())
            .collect::<Vec<_>>();
        let contributions = super::session_obj::collect_owned_sync(
            self.composition.tool_catalog_contributors(),
            ToolCatalogContext {
                owner,
                tools: manifests.clone(),
                config,
                extensions: extensions.clone(),
            },
            |hook, ctx| hook(ctx),
        )?
        .into_iter()
        .map(|owned| owned.value)
        .collect::<Vec<_>>();
        let catalog = crate::build_tool_catalog(crate::ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |manifest: &ToolManifest| {
                definitions
                    .iter()
                    .find(|definition| definition.manifest.id == manifest.id)
                    .map(|definition| Arc::new(definition.contract.clone()))
            })),
            contributions,
            isolation_engines,
        })
        .map_err(|error| PluginError::Registration(error.to_string()))?;
        Ok((
            crate::tool_registry::SurfaceTools::of_entries(
                entries.into_values(),
                Arc::clone(&self.registry),
            ),
            catalog,
        ))
    }
}

/// A [`StaticPluginFactory`]'s spec as a process component whose config is
/// `()` (D12): what process controls and presentation-only plugins use.
#[derive(Clone)]
pub struct StaticProcessComponent {
    declaration: ProcessComponentDeclaration,
    spec: ProcessComponentSpec,
}

impl StaticPluginFactory {
    /// This factory's spec as a process component at `format_version`.
    ///
    /// # Errors
    ///
    /// The spec declares a contribution the contribution table does not
    /// admit in a process ([`registries`]), or the id is refused.
    pub fn as_component(
        &self,
        format_version: FormatVersion,
    ) -> Result<StaticProcessComponent, PluginError> {
        let spec = self.spec();
        if let Some(error) = spec.errors.first() {
            return Err(error.clone());
        }
        let declaration = self.plugin_declaration();
        let registrations = spec
            .registrations
            .clone()
            .admitted_in::<registries::Process>(declaration.id.as_str())?;
        Ok(StaticProcessComponent {
            declaration: ProcessComponentDeclaration::initial(
                declaration.id.as_str(),
                format_version,
            )?,
            spec: ProcessComponentSpec {
                registrations,
                errors: Vec::new(),
            },
        })
    }
}

impl StaticProcessComponent {
    /// The scoped address of a provider in this component's declared entries.
    pub fn provider_handle(&self, key: ToolProviderKey) -> Result<ToolProviderHandle, PluginError> {
        if let Some(error) = self.spec.errors.first() {
            return Err(error.clone());
        }
        self.spec
            .registrations
            .provider_handle(self.declaration.id.as_str(), key)
    }
}

impl ProcessComponentFactory for StaticProcessComponent {
    fn declaration(&self) -> ProcessComponentDeclaration {
        self.declaration.clone()
    }

    fn config_type(&self) -> TypeId {
        TypeId::of::<()>()
    }

    fn validate(
        &self,
        entry: &ProcessComponentEntry,
        _env: &ProcessEnv,
    ) -> Result<(), ProcessEnvRefusal> {
        entry
            .config()
            .decode::<()>()
            .map_err(|error| ProcessEnvRefusal::ConfigRefused {
                id: entry.id().to_string(),
                reason: error.to_string(),
            })
    }

    fn recipe(self: Arc<Self>) -> std::result::Result<ProcessRecipe, crate::plugin::PluginError> {
        self.spec.clone().recipe(self.declaration.id.as_str())
    }
}
