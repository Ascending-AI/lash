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
use crate::prompt_sections::PromptPurpose;
use crate::{ProcessId, SessionId, ToolProvider};

/// What a component declares about itself: its id and config formats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessComponentDeclaration {
    pub id: ProcessComponentId,
    /// The config format the component writes natively.
    pub config_format: FormatVersion,
    /// Every config format it reads. Contains `config_format`.
    pub readable_config_formats: Vec<FormatVersion>,
    /// Every config format it writes. Contains `config_format`.
    pub writable_config_formats: Vec<FormatVersion>,
}

impl ProcessComponentDeclaration {
    /// The declaration of a component that reads and writes only `config_format`.
    ///
    /// # Errors
    ///
    /// The id is refused.
    pub fn initial(id: &str, config_format: FormatVersion) -> Result<Self, PluginError> {
        Ok(Self {
            id: ProcessComponentId::new(id)
                .map_err(|error| PluginError::Registration(error.to_string()))?,
            config_format,
            readable_config_formats: vec![config_format],
            writable_config_formats: vec![config_format],
        })
    }

    /// The claim requirement a process naming this component at
    /// `format` carries, and a node registering it serves (§6.2).
    pub fn config_token(id: &ProcessComponentId, format: FormatVersion) -> String {
        format!("component:{id}@cfg{format}")
    }

    /// The claim requirements a process under `env` carries from its
    /// start: one config token for every component `env` names.
    pub fn env_requirements(env: &ProcessEnv) -> Vec<String> {
        let tokens: BTreeSet<String> = env
            .components()
            .iter()
            .map(|entry| Self::config_token(entry.id(), entry.config().format_version()))
            .collect();
        tokens.into_iter().collect()
    }
}

/// A process component as a host installs it.
pub trait ProcessComponentFactory: Send + Sync {
    fn declaration(&self) -> ProcessComponentDeclaration;

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
    fn build(&self, cx: &ProcessComponentContext<'_>) -> Result<ProcessComponentSpec, PluginError>;
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

/// Only what process execution consumes. Nothing session-shaped.
#[derive(Clone, Default)]
pub struct ProcessComponentSpec {
    /// Each provider under its declared key, the static identity its calls
    /// bind and dispatch through.
    pub tool_providers: Vec<(HookKey, Arc<dyn ToolProvider>)>,
    /// The deferred provider, under its declared key: an env has at most
    /// one.
    pub deferred_provider: Option<(HookKey, Arc<dyn crate::DeferredToolProvider>)>,
    pub tool_catalog_contributors: Vec<(HookKey, ToolCatalogContributor)>,
    pub tool_args_transforms: Vec<(HookKey, ToolArgsTransformHook)>,
    pub tool_args_checks: Vec<(HookKey, ToolArgsCheckHook)>,
    pub tool_result_transforms: Vec<(HookKey, ToolResultTransformHook)>,
    pub tool_result_checks: Vec<(HookKey, ToolResultCheckHook)>,
    /// At most one per composition (D12).
    pub presenter: Option<ToolPresentationPresenter>,
    pub presentation_steps: Vec<(HookKey, ToolPresentationStep)>,
    /// Sections of direct calls only (D16): any other purpose is refused.
    pub direct_prompt_sections: Vec<(PromptSectionSpec, Arc<dyn PromptSection>)>,
    /// The reducers the component's step state applies (FIG-5268).
    pub state_reducers: Vec<(String, StateReducer)>,
}

struct RegisteredComponent {
    factory: Arc<dyn ProcessComponentFactory>,
    declaration: ProcessComponentDeclaration,
}

/// The components a deployment installs, by id, and the host services they
/// build against. Separate from the session plugin registry.
#[derive(Clone)]
pub struct ProcessComponentRegistry {
    components: Arc<BTreeMap<String, Arc<RegisteredComponent>>>,
    host: ProcessComponentHost,
}

impl Default for ProcessComponentRegistry {
    fn default() -> Self {
        Self {
            components: Arc::default(),
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
    /// the surface the link retained, and a `call_dynamic` link's provider
    /// is a component's deferred provider, which matches its target per
    /// call.
    ///
    /// # Errors
    ///
    /// [`ProcessEnvRefusal::ToolNotServed`] or
    /// [`ProcessEnvRefusal::LinkedSurfaceChanged`] naming the effect, or the
    /// refusal of a component that does not compose.
    pub fn check_served(&self, env: &Arc<ProcessEnv>) -> Result<(), ProcessEnvRefusal> {
        if env.effects().is_empty() {
            return Ok(());
        }
        let composition = self.compose(env, None)?;
        for (effect, link) in env.effects() {
            match composition.step_link(env, effect) {
                StepLink::Served { .. } => {}
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
                        provider: format!("{}/{}", link.provider.owner, link.provider.key),
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
    /// do not contain its own config format.
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
        if key.format() != declaration.config_format
            || !declaration
                .readable_config_formats
                .contains(&declaration.config_format)
            || !declaration
                .writable_config_formats
                .contains(&declaration.config_format)
        {
            return Err(PluginError::Registration(format!(
                "process component `{}` declares formats that do not contain its config format",
                declaration.id
            )));
        }
        if self.components.contains_key(declaration.id.as_str()) {
            return Err(PluginError::Registration(format!(
                "duplicate process component `{}`",
                declaration.id
            )));
        }
        let mut components = (*self.components).clone();
        components.insert(
            declaration.id.as_str().to_owned(),
            Arc::new(RegisteredComponent {
                factory,
                declaration,
            }),
        );
        self.components = Arc::new(components);
        Ok(())
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

    /// The claim requirements this registry serves (§6.2, S7): each
    /// component's readable config formats, and the token of every process
    /// callback its components register.
    pub fn serves(&self) -> Vec<String> {
        let mut serves = BTreeSet::new();
        for declaration in self.declarations() {
            for format in &declaration.readable_config_formats {
                serves.insert(ProcessComponentDeclaration::config_token(
                    &declaration.id,
                    *format,
                ));
            }
        }
        serves.extend(self.served_callbacks());
        serves.into_iter().collect()
    }

    /// The obligation tokens of the callbacks the installed components
    /// register (S7): a projection of each component's registrations, built
    /// alone under its unit config with no process, as an admission builds
    /// it. A component whose config is not `()` cannot be built without a
    /// process's config and projects none; one that does not build is
    /// logged and projects none.
    fn served_callbacks(&self) -> Vec<String> {
        let mut tokens = Vec::new();
        for registered in self.components.values() {
            if registered.factory.config_type() != TypeId::of::<()>() {
                continue;
            }
            let declaration = &registered.declaration;
            let entry = ProcessComponentEntry::from_parts(
                declaration.id.clone(),
                lash_core_store::process_env::ProcessComponentConfig::new(
                    declaration.config_format,
                    serde_json::Value::Null,
                ),
                BTreeSet::new(),
            );
            let composed = ProcessEnv::builder(crate::MaxToolCalls::new(1))
                .entry(entry)
                .build()
                .and_then(|env| self.compose(&env, None));
            match composed {
                Ok(composition) => tokens.extend(crate::ObligationToken::of_callbacks(
                    crate::RegistryKind::Process,
                    composition
                        .composition
                        .contributions()
                        .map(|(_, identity)| identity),
                )),
                Err(error) => tracing::error!(
                    component = %declaration.id,
                    %error,
                    "a process component serves no callbacks"
                ),
            }
        }
        tokens
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
                    native: declaration.config_format,
                    writable: declaration.writable_config_formats.clone(),
                },
            )
            .collect()
    }

    /// Admission steps 2–7 of a new environment (§3.3): unique ids,
    /// installed components, readable formats, each `validate`, each
    /// `build(process: None)`, and a composable set. Ambient tools are
    /// refused first (D7). The fleet writer range (step 8) is the store's,
    /// checked at publication.
    ///
    /// # Errors
    ///
    /// The first refusal, typed.
    pub fn admit(&self, env: &ProcessEnv) -> Result<(), ProcessEnvRefusal> {
        env.check_shape()?;
        for entry in env.components() {
            let registered = self.components.get(entry.id().as_str()).ok_or_else(|| {
                ProcessEnvRefusal::ComponentNotInstalled {
                    id: entry.id().to_string(),
                }
            })?;
            let format = entry.config().format_version();
            if !registered
                .declaration
                .readable_config_formats
                .contains(&format)
            {
                return Err(ProcessEnvRefusal::ConfigFormatUnreadable {
                    id: entry.id().to_string(),
                    format: format.get(),
                    readable: registered
                        .declaration
                        .readable_config_formats
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
        let mut presenter_owner: Option<String> = None;
        let mut owners = Vec::with_capacity(env.components().len());
        for entry in env.components() {
            let registered = self.component(entry)?;
            let id = entry.id().to_string();
            let ports = self.host.ports(entry);
            let spec = registered
                .factory
                .build(&ProcessComponentContext {
                    process,
                    env,
                    entry,
                    capabilities: env.capabilities(),
                    ports: &ports,
                    state: process.map(|process| {
                        PluginStateView::bind(
                            &crate::RuntimeOwner::Process(process.clone()),
                            &id,
                            Arc::clone(state),
                        )
                    }),
                })
                .map_err(|error| ProcessEnvRefusal::ComponentBuildFailed {
                    id: id.clone(),
                    reason: error.to_string(),
                })?;
            if spec.presenter.is_some() {
                if let Some(first) = presenter_owner.take() {
                    return Err(ProcessEnvRefusal::DuplicatePresenter { first, second: id });
                }
                presenter_owner = Some(id.clone());
            }
            if let Some((section, _)) = spec.direct_prompt_sections.iter().find(|(section, _)| {
                section
                    .purposes
                    .iter()
                    .any(|purpose| !matches!(purpose, PromptPurpose::Direct { .. }))
            }) {
                return Err(ProcessEnvRefusal::ComponentBuildFailed {
                    id,
                    reason: format!(
                        "prompt section `{}` serves a purpose other than a direct call",
                        section.key
                    ),
                });
            }
            let owner = id.clone();
            let mut reg = PluginRegistrar::new(owner.clone());
            reg.contributions = contributions;
            register_spec(&mut reg, spec).map_err(|error| {
                ProcessEnvRefusal::ComponentBuildFailed {
                    id: id.clone(),
                    reason: error.to_string(),
                }
            })?;
            contributions = reg.contributions;
            owners.push(owner);
        }
        let registry =
            super::runtime_impl::build_tool_registry(&contributions).map_err(|error| {
                ProcessEnvRefusal::ComponentBuildFailed {
                    id: owners.last().cloned().unwrap_or_default(),
                    reason: error.to_string(),
                }
            })?;
        Ok(BuiltComponents {
            owners,
            contributions,
            registry,
        })
    }

    /// The execution composition `env`'s components make for `process`
    /// (§5.1): only the components the env names, in env order.
    ///
    /// # Errors
    ///
    /// A component is not installed here, or it did not build.
    pub fn compose(
        &self,
        env: &ProcessEnv,
        process: Option<&ProcessId>,
    ) -> Result<ProcessComposition, ProcessEnvRefusal> {
        let state = Arc::default();
        let BuiltComponents {
            owners,
            mut contributions,
            registry,
        } = self.contributions(env, process, &state)?;
        let reducers = std::mem::take(&mut contributions.state_reducers);
        let composition = Arc::new(ExecutionComposition::new(
            &mut contributions,
            Arc::clone(&registry),
        ));
        Ok(ProcessComposition {
            composition,
            registry,
            owners,
            reducers,
            state,
        })
    }
}

struct BuiltComponents {
    owners: Vec<String>,
    contributions: PluginContributions,
    registry: Arc<crate::ToolRegistry>,
}

fn register_spec(reg: &mut PluginRegistrar, spec: ProcessComponentSpec) -> Result<(), PluginError> {
    let ProcessComponentSpec {
        tool_providers,
        deferred_provider,
        tool_catalog_contributors,
        tool_args_transforms,
        tool_args_checks,
        tool_result_transforms,
        tool_result_checks,
        presenter,
        presentation_steps,
        direct_prompt_sections,
        state_reducers,
    } = spec;
    for (key, provider) in tool_providers {
        reg.tools().provider(key, provider)?;
    }
    if let Some((key, provider)) = deferred_provider {
        reg.tools().deferred(key, provider)?;
    }
    for (key, contributor) in tool_catalog_contributors {
        reg.tool_catalog().contribute(key, contributor)?;
    }
    for (key, hook) in tool_args_transforms {
        reg.tool_calls().transform_args(key, hook)?;
    }
    for (key, hook) in tool_args_checks {
        reg.tool_calls().check_args(key, hook)?;
    }
    for (key, hook) in tool_result_transforms {
        reg.tool_calls().transform_result(key, hook)?;
    }
    for (key, hook) in tool_result_checks {
        reg.tool_calls().check_result(key, hook)?;
    }
    if let Some(presenter) = presenter {
        reg.tool_results().presenter(presenter)?;
    }
    for (key, step) in presentation_steps {
        reg.tool_results().presentation_step(key, step)?;
    }
    for (section, renderer) in direct_prompt_sections {
        reg.prompt().section(section, renderer)?;
    }
    for (name, reducer) in state_reducers {
        reg.state_reducer(name, reducer)?;
    }
    Ok(())
}

/// What a process's components make: its execution composition, the tool
/// registry its providers route through, its owners in env order, and what
/// its step state reduces and admits by (FIG-5268).
pub struct ProcessComposition {
    pub composition: Arc<ExecutionComposition>,
    pub registry: Arc<crate::ToolRegistry>,
    pub owners: Vec<String>,
    pub(crate) reducers: BTreeMap<String, BTreeMap<String, StateReducer>>,
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
}

impl ProcessComposition {
    /// How this node serves the linked call of `effect` under `env` (D3):
    /// the link's provider, by its key, serving the linked tool with the
    /// surface the link retained, judged now, before the call's `x_start`.
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
        let Some(definition) = self.registry.served(&link.provider, link.tool(), &config) else {
            return drifted(crate::ToolSurfaceDriftKind::Missing);
        };
        if crate::ProcessSurface::of(&definition) != link.surface {
            return drifted(crate::ToolSurfaceDriftKind::Changed);
        }
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
    /// path the provider does not answer is unavailable, and its note is
    /// bounded.
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
        let provider = self
            .registry
            .deferred_provider(&link.provider)
            .ok_or_else(|| {
                PluginError::Registration(format!(
                    "the deferred provider `{}` the env links is not served on this node",
                    link.provider
                ))
            })?;
        let config = super::OwnerConfigView::components(Arc::clone(env));
        let cx = crate::DeferredResolveContext::new(owner, None, env.capabilities(), &config);
        let outcome = provider
            .resolve(&cx, &[path])
            .await
            .remove(path)
            .unwrap_or(crate::DeferredOutcome::Unavailable { note: None });
        Ok(crate::DeferredMatch {
            requested: path.to_owned(),
            outcome: crate::bounded_deferred_outcome(outcome),
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
pub struct StaticProcessComponent {
    declaration: ProcessComponentDeclaration,
    spec: ProcessComponentSpec,
}

impl StaticPluginFactory {
    /// This factory's spec as a process component at `config_format`.
    ///
    /// # Errors
    ///
    /// The spec fills a field no process consumes (anything but providers,
    /// catalog contributors, the four tool hooks, presentation steps and
    /// state reducers), or the id is refused.
    pub fn as_component(
        &self,
        config_format: FormatVersion,
    ) -> Result<StaticProcessComponent, PluginError> {
        let spec = self.spec();
        let session_only = [
            (
                "extension_contributions",
                spec.extension_contributions.is_empty(),
            ),
            ("before_turn_hooks", spec.before_turn_hooks.is_empty()),
            ("after_turn_hooks", spec.after_turn_hooks.is_empty()),
            ("checkpoint_hooks", spec.checkpoint_hooks.is_empty()),
            (
                "assistant_stream_hooks",
                spec.assistant_stream_hooks.is_empty(),
            ),
            (
                "assistant_response_hooks",
                spec.assistant_response_hooks.is_empty(),
            ),
            (
                "assistant_stream_finished_hooks",
                spec.assistant_stream_finished_hooks.is_empty(),
            ),
            ("runtime_event_hooks", spec.runtime_event_hooks.is_empty()),
            ("plugin_operations", spec.plugin_operations.is_empty()),
            ("context_compactors", spec.context_compactors.is_empty()),
            (
                "context_pressure_hooks",
                spec.context_pressure_hooks.is_empty(),
            ),
        ];
        if let Some((field, _)) = session_only.iter().find(|(_, empty)| !*empty) {
            return Err(PluginError::Registration(format!(
                "plugin `{}` fills `{field}`, which no process consumes",
                self.plugin_declaration().id
            )));
        }
        let declaration = self.plugin_declaration();
        let component =
            ProcessComponentDeclaration::initial(declaration.id.as_str(), config_format)?;
        Ok(StaticProcessComponent {
            declaration: component,
            spec: ProcessComponentSpec {
                tool_providers: spec.tool_providers.clone(),
                deferred_provider: spec.deferred_provider.clone(),
                tool_catalog_contributors: spec.tool_catalog_contributors.clone(),
                tool_args_transforms: spec.tool_args_transforms.clone(),
                tool_args_checks: spec.tool_args_checks.clone(),
                tool_result_transforms: spec.tool_result_transforms.clone(),
                tool_result_checks: spec.tool_result_checks.clone(),
                presenter: None,
                presentation_steps: spec.presentation_steps.clone(),
                direct_prompt_sections: Vec::new(),
                state_reducers: spec.state_reducers.clone(),
            },
        })
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

    fn build(
        &self,
        _cx: &ProcessComponentContext<'_>,
    ) -> Result<ProcessComponentSpec, PluginError> {
        Ok(self.spec.clone())
    }
}
