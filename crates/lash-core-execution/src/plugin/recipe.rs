//! Executable declarations frozen before binding an owner.
use super::prompt::{
    PromptSectionFamilySpec, PromptSectionId, PromptSectionSpec, PromptWrapId, PromptWrapSpec,
};
use super::registrar::BoundRegistrar;
use super::*;
pub use lash_core_store::obligations::{ObligationToken, RegistryKind};
pub use lash_core_store::tool_run::CallbackSlot;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContributionKind {
    Callback(CallbackSlot),
    PromptSection,
    PromptFamily,
    PromptWrapper,
    StateReducer,
    ConfigNamespace,
    ConfigCommand,
    Extension,
    ProcessEngine,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContributionKey {
    Callback(PluginCallbackIdentity),
    PromptSection(PromptSectionId),
    PromptFamily(PromptSectionId),
    PromptWrapper(PromptWrapId),
    Named { owner: String, key: String },
}
impl ContributionKey {
    pub fn owner(&self) -> &str {
        match self {
            Self::Callback(key) => &key.owner,
            Self::PromptSection(key) | Self::PromptFamily(key) => &key.owner,
            Self::PromptWrapper(key) => &key.owner,
            Self::Named { owner, .. } => owner,
        }
    }
    fn set_owner(&mut self, owner: &str) {
        match self {
            Self::Callback(key) => key.owner = owner.into(),
            Self::PromptSection(key) | Self::PromptFamily(key) => key.owner = owner.into(),
            Self::PromptWrapper(key) => key.owner = owner.into(),
            Self::Named { owner: bound, .. } => *bound = owner.into(),
        }
    }
}
#[derive(Clone, Debug)]
pub enum ContributionDetails {
    Callback { stream_state_from: Option<HookKey> },
    Operation(PluginOperationDef),
    PromptSection(PromptSectionSpec),
    PromptFamily(PromptSectionFamilySpec),
    PromptWrapper(PromptWrapSpec),
    Named,
}
#[derive(Clone, Debug)]
pub struct ContributionDescriptor {
    pub registry: RegistryKind,
    pub kind: ContributionKind,
    pub key: ContributionKey,
    pub registration_order: u32,
    pub priority: Option<i32>,
    pub details: ContributionDetails,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InventoryScope {
    Deployment(RegistryKind),
    Owner(crate::RuntimeOwner),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContributionReadiness {
    Ready,
    Refused(PreflightIssue),
}
#[derive(Clone, Debug)]
pub struct InstalledInventory {
    pub scope: InventoryScope,
    pub entries: Vec<ContributionDescriptor>,
    pub readiness: Vec<ContributionReadiness>,
}
impl InstalledInventory {
    pub fn callbacks(&self) -> impl Iterator<Item = &PluginCallbackIdentity> {
        self.entries.iter().filter_map(|entry| match &entry.key {
            ContributionKey::Callback(key) => Some(key),
            _ => None,
        })
    }

    /// The claim tokens these registrations serve (S7): one for every ready
    /// callback entry, each an installed resolver, through the registry it
    /// was registered in. Nothing is built or invoked.
    pub fn served_tokens(&self) -> std::collections::BTreeSet<ObligationToken> {
        self.entries
            .iter()
            .zip(&self.readiness)
            .filter_map(|(entry, readiness)| match (&entry.key, readiness) {
                (ContributionKey::Callback(key), ContributionReadiness::Ready) => {
                    Some(ObligationToken::for_callback(entry.registry, key))
                }
                _ => None,
            })
            .collect()
    }
}
pub use lash_core_store::plugin_registrations::{PreflightIssue, RequiredPort, ServiceKind};
#[derive(Clone, Debug, Default)]
pub struct Requirements {
    pub services: std::collections::BTreeSet<ServiceKind>,
}

type EntryBinder<T> =
    Arc<dyn Fn(&T, &PluginStateView, &mut BoundRegistrar) -> Result<(), PluginError> + Send + Sync>;
struct Registration<T> {
    descriptor: ContributionDescriptor,
    bind: EntryBinder<T>,
}
impl<T> Clone for Registration<T> {
    fn clone(&self) -> Self {
        Self {
            descriptor: self.descriptor.clone(),
            bind: Arc::clone(&self.bind),
        }
    }
}
/// A pure list of typed contributions. Every entry contains its descriptor
/// and the binder that supplies its declared implementation.
pub struct PluginRegistrar<T = ()> {
    entries: Vec<Registration<T>>,
    owner: Option<String>,
    registry: RegistryKind,
    pub requirements: Requirements,
}
impl<T> Clone for PluginRegistrar<T> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            registry: self.registry,
            entries: self.entries.clone(),
            requirements: self.requirements.clone(),
        }
    }
}
impl Default for PluginRegistrar<()> {
    fn default() -> Self {
        Self {
            owner: None,
            registry: RegistryKind::Session,
            entries: Vec::new(),
            requirements: Requirements::default(),
        }
    }
}
impl<T: Send + Sync + 'static> PluginRegistrar<T> {
    pub fn new(owner: &str) -> Self {
        Self {
            owner: Some(owner.into()),
            registry: RegistryKind::Session,
            entries: Vec::new(),
            requirements: Requirements::default(),
        }
    }
    pub fn for_process(owner: &str) -> Self {
        Self {
            registry: RegistryKind::Process,
            ..Self::new(owner)
        }
    }

    pub fn require(&mut self, service: ServiceKind) {
        self.requirements.services.insert(service);
    }
    fn add(
        &mut self,
        kind: ContributionKind,
        mut key: ContributionKey,
        priority: Option<i32>,
        details: ContributionDetails,
        bind: EntryBinder<T>,
    ) -> Result<(), PluginError> {
        if let Some(owner) = &self.owner {
            key.set_owner(owner);
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.descriptor.kind == kind && entry.descriptor.key == key)
        {
            // Every operation kind shares one name space: a query and a task
            // of one name are the same declared address.
            return Err(PluginError::Registration(match &details {
                ContributionDetails::Operation(def) => {
                    format!("duplicate plugin operation name `{}`", def.name)
                }
                _ => format!("duplicate registration {key:?}"),
            }));
        }
        let registration_order = u32::try_from(self.entries.len())
            .map_err(|_| PluginError::Registration("too many registrations".into()))?;
        self.entries.push(Registration {
            descriptor: ContributionDescriptor {
                registry: RegistryKind::Session,
                kind,
                key,
                registration_order,
                priority,
                details,
            },
            bind,
        });
        Ok(())
    }
    fn callback(
        &mut self,
        slot: CallbackSlot,
        key: Option<HookKey>,
        priority: Option<i32>,
        details: ContributionDetails,
        bind: EntryBinder<T>,
    ) -> Result<(), PluginError> {
        let key = key.map_or_else(
            || slot.key_prefix().to_owned(),
            |key| format!("{}:{key}", slot.key_prefix()),
        );
        self.add(
            ContributionKind::Callback(slot),
            ContributionKey::Callback(PluginCallbackIdentity {
                owner: String::new(),
                key,
            }),
            priority,
            details,
            bind,
        )
    }
    pub fn state_reducer(
        &mut self,
        name: impl Into<String>,
        binder: impl Fn(&T, &PluginStateView) -> Result<StateReducer, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        let name = name.into();
        self.add(
            ContributionKind::StateReducer,
            ContributionKey::Named {
                owner: String::new(),
                key: name.clone(),
            },
            None,
            ContributionDetails::Named,
            Arc::new(move |owner, state, reg| {
                reg.state_reducer(name.clone(), binder(owner, state)?)
            }),
        )
    }
    pub fn extension(
        &mut self,
        id: impl Into<String>,
        binder: impl Fn(&T, &PluginStateView) -> Result<serde_json::Value, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        let id = id.into();
        self.add(
            ContributionKind::Extension,
            ContributionKey::Named {
                owner: String::new(),
                key: id.clone(),
            },
            None,
            ContributionDetails::Named,
            Arc::new(move |owner, state, reg| {
                reg.contributions
                    .extensions
                    .push(PluginExtensionContribution::from_value(
                        id.clone(),
                        binder(owner, state)?,
                    ));
                Ok(())
            }),
        )
    }
    pub fn prompt(&mut self) -> PromptRecipeRegistrations<'_, T> {
        PromptRecipeRegistrations { reg: self }
    }
    pub fn operations(&mut self) -> OperationRecipeRegistrations<'_, T> {
        OperationRecipeRegistrations { reg: self }
    }
    pub(crate) fn descriptors(
        &self,
        owner: &str,
        registry: RegistryKind,
    ) -> Vec<ContributionDescriptor> {
        self.entries
            .iter()
            .map(|entry| {
                let mut descriptor = entry.descriptor.clone();
                descriptor.registry = self.owner.as_ref().map_or(registry, |_| self.registry);
                descriptor
                    .key
                    .set_owner(self.owner.as_deref().unwrap_or(owner));
                descriptor
            })
            .collect()
    }
    pub(crate) fn provider_handle(
        &self,
        owner: &str,
        registry: RegistryKind,
        key: ToolProviderKey,
    ) -> Result<ToolProviderHandle, PluginError> {
        let handle = ToolProviderHandle::new(registry, owner, key);
        let identity = handle.callback_identity();
        if self.descriptors(owner, registry).iter().any(|entry| {
            entry.registry == registry
                && entry.kind == ContributionKind::Callback(CallbackSlot::ToolProvider)
                && entry.key == ContributionKey::Callback(identity.clone())
        }) {
            Ok(handle)
        } else {
            Err(PluginError::Registration(format!(
                "provider `{handle}` was not declared"
            )))
        }
    }

    pub(crate) fn bind(
        &self,
        owner: &T,
        state: &PluginStateView,
        reg: &mut BoundRegistrar,
    ) -> Result<(), PluginError> {
        for entry in &self.entries {
            (entry.bind)(owner, state, reg)?;
        }
        reg.validate_stream_state_pairs()
    }
}
macro_rules! keyed_facet {
    ($facet:ident, $method:ident, $( $name:ident : $slot:ident => $ty:ty ),+ $(,)?) => {
        pub struct $facet<'a, T = ()> { reg: &'a mut PluginRegistrar<T> }
        impl<T: Send + Sync + 'static> PluginRegistrar<T> {
            pub fn $method(&mut self) -> $facet<'_, T> { $facet { reg: self } }
        }
        impl<T: Send + Sync + 'static> $facet<'_, T> {
            $(pub fn $name(self, key: HookKey, binder: impl Fn(&T, &PluginStateView) -> Result<$ty, PluginError> + Send + Sync + 'static) -> Result<(), PluginError> {
                self.reg.callback(CallbackSlot::$slot, Some(key), None, ContributionDetails::Callback { stream_state_from: None },
                    Arc::new(move |owner, state, reg| reg.$method().$name(key, binder(owner, state)?)))
            })+
        }
    }
}
keyed_facet!(TurnRegistrations, turn, before: BeforeTurn => BeforeTurnHook, after: AfterTurn => AfterTurnHook, checkpoint: Checkpoint => CheckpointHook);
keyed_facet!(ToolCallRegistrations, tool_calls, transform_args: ToolArgsTransform => ToolArgsTransformHook, check_args: ToolArgsCheck => ToolArgsCheckHook, transform_result: ToolResultTransform => ToolResultTransformHook, check_result: ToolResultCheck => ToolResultCheckHook);
keyed_facet!(ToolCatalogRegistrations, tool_catalog, contribute: ToolCatalog => ToolCatalogContributor);
keyed_facet!(SessionRegistrations, session, on_event: RuntimeEvent => PluginLifecycleEventHook);
keyed_facet!(OutputRegistrations, output, stream: AssistantStream => AssistantStreamHook, stream_finished: AssistantStreamFinished => AssistantStreamFinishedHook);
keyed_facet!(ToolResultRegistrations, tool_results, presentation_step: PresentationStep => ToolPresentationStep);
macro_rules! singleton {
    ($facet:ident, $method:ident, $( $name:ident : $slot:ident => $ty:ty ),+ $(,)?) => {
        pub struct $facet<'a, T = ()> { reg: &'a mut PluginRegistrar<T> }
        impl<T: Send + Sync + 'static> PluginRegistrar<T> { pub fn $method(&mut self) -> $facet<'_, T> { $facet { reg: self } } }
        impl<T: Send + Sync + 'static> $facet<'_, T> {
            $(pub fn $name(self, binder: impl Fn(&T, &PluginStateView) -> Result<$ty, PluginError> + Send + Sync + 'static) -> Result<(), PluginError> {
                self.reg.callback(CallbackSlot::$slot, None, None, ContributionDetails::Callback { stream_state_from: None },
                    Arc::new(move |owner, state, reg| reg.$method().$name(binder(owner, state)?)))
            })+
        }
    }
}
singleton!(ProtocolRegistrations, protocol, session: ProtocolSession => Arc<dyn ProtocolSessionPlugin>, protocol_driver: ProtocolDriver => Arc<dyn ProtocolDriverPlugin>);
singleton!(ExecutionRegistrations, execution, code_executor: CodeExecutor => Arc<dyn CodeExecutorPlugin>);
impl<T: Send + Sync + 'static> OutputRegistrations<'_, T> {
    pub fn response(
        self,
        key: HookKey,
        stream_state_from: Option<HookKey>,
        binder: impl Fn(&T, &PluginStateView) -> Result<AssistantResponseHook, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::AssistantResponse,
            Some(key),
            None,
            ContributionDetails::Callback { stream_state_from },
            Arc::new(move |owner, state, reg| {
                reg.output()
                    .response(key, stream_state_from, binder(owner, state)?)
            }),
        )
    }
    pub fn assistant_prose_projector(
        self,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<Arc<dyn AssistantProseProjectorPlugin>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::AssistantProseProjector,
            None,
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| {
                reg.output()
                    .assistant_prose_projector(binder(owner, state)?)
            }),
        )
    }
}
impl<T: Send + Sync + 'static> ToolResultRegistrations<'_, T> {
    pub fn presenter(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<ToolPresentationPresenter, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::PresentationPresenter,
            None,
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| reg.tool_results().presenter(binder(owner, state)?)),
        )
    }
}
pub struct ContextRegistrations<'a, T = ()> {
    reg: &'a mut PluginRegistrar<T>,
}
impl<T: Send + Sync + 'static> PluginRegistrar<T> {
    pub fn context(&mut self) -> ContextRegistrations<'_, T> {
        ContextRegistrations { reg: self }
    }
}
impl<T: Send + Sync + 'static> ContextRegistrations<'_, T> {
    pub fn pressure(
        self,
        key: HookKey,
        priority: i32,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ContextPressureHook>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::ContextPressure,
            Some(key),
            Some(priority),
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| {
                reg.context().pressure(key, priority, binder(owner, state)?)
            }),
        )
    }
    pub fn compact(
        self,
        key: HookKey,
        priority: i32,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ContextCompactor>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::ContextCompactor,
            Some(key),
            Some(priority),
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| {
                reg.context().compact(key, priority, binder(owner, state)?)
            }),
        )
    }
    pub fn attachment_omissions(
        self,
        key: HookKey,
        priority: i32,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn AttachmentOmissionPolicy>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::AttachmentOmission,
            Some(key),
            Some(priority),
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| {
                reg.context()
                    .attachment_omissions(key, priority, binder(owner, state)?)
            }),
        )
    }
}
pub struct ToolRegistrations<'a, T = ()> {
    reg: &'a mut PluginRegistrar<T>,
}
impl<T: Send + Sync + 'static> PluginRegistrar<T> {
    pub fn tools(&mut self) -> ToolRegistrations<'_, T> {
        ToolRegistrations { reg: self }
    }
}
impl<T: Send + Sync + 'static> ToolRegistrations<'_, T> {
    pub fn provider(
        self,
        key: ToolProviderKey,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError> {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(self.reg.registry, owner, key);
        self.record_provider(key, binder)?;
        Ok(handle)
    }
    pub(crate) fn record_provider(
        self,
        key: ToolProviderKey,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::ToolProvider,
            Some(HookKey::__literal(key.as_str())),
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| reg.tools().provider(key, binder(owner, state)?)),
        )
    }
    pub fn deferred(
        self,
        key: ToolProviderKey,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<Arc<dyn crate::DeferredToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError> {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(self.reg.registry, owner, key);
        self.record_deferred(key, binder)?;
        Ok(handle)
    }
    pub(crate) fn record_deferred(
        self,
        key: ToolProviderKey,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<Arc<dyn crate::DeferredToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.callback(
            CallbackSlot::ToolProvider,
            Some(HookKey::__literal(key.as_str())),
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
            },
            Arc::new(move |owner, state, reg| reg.tools().deferred(key, binder(owner, state)?)),
        )
    }
}
pub struct PromptRecipeRegistrations<'a, T = ()> {
    reg: &'a mut PluginRegistrar<T>,
}
impl<T: Send + Sync + 'static> PromptRecipeRegistrations<'_, T> {
    pub fn section(
        self,
        spec: PromptSectionSpec,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<Arc<dyn super::prompt::PromptSection>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.add(
            ContributionKind::PromptSection,
            ContributionKey::PromptSection(PromptSectionId::new(String::new(), spec.key.clone())),
            None,
            ContributionDetails::PromptSection(spec.clone()),
            Arc::new(move |owner, state, reg| {
                reg.prompt().section(spec.clone(), binder(owner, state)?)
            }),
        )
    }
    pub fn family(
        self,
        spec: PromptSectionFamilySpec,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<Arc<dyn super::prompt::PromptSectionSource>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.add(
            ContributionKind::PromptFamily,
            ContributionKey::PromptFamily(PromptSectionId::new(String::new(), spec.prefix.clone())),
            None,
            ContributionDetails::PromptFamily(spec.clone()),
            Arc::new(move |owner, state, reg| {
                reg.prompt().family(spec.clone(), binder(owner, state)?)
            }),
        )
    }
    pub fn wrap(
        self,
        spec: PromptWrapSpec,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<Arc<dyn super::prompt::PromptSectionWrap>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        self.reg.add(
            ContributionKind::PromptWrapper,
            ContributionKey::PromptWrapper(PromptWrapId {
                owner: String::new(),
                key: spec.key.clone(),
            }),
            None,
            ContributionDetails::PromptWrapper(spec.clone()),
            Arc::new(move |owner, state, reg| {
                reg.prompt().wrap(spec.clone(), binder(owner, state)?)
            }),
        )
    }
}
pub struct OperationRecipeRegistrations<'a, T = ()> {
    reg: &'a mut PluginRegistrar<T>,
}
impl<T: Send + Sync + 'static> OperationRecipeRegistrations<'_, T> {
    #[cfg(test)]
    pub(crate) fn query(
        self,
        spec: PluginOperationSpec,
        binder: impl Fn(&T, &PluginStateView) -> Result<PluginQueryHandler, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError> {
        let def = PluginOperationDef::from_spec(spec.clone(), PluginOperationKind::Query);
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations().query(spec.clone(), binder(owner, state)?)
            }),
        )
    }

    pub fn typed_query<Op, F, Fut>(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<F, PluginError> + Send + Sync + 'static,
    ) -> Result<(), PluginError>
    where
        Op: PluginQuery,
        F: Fn(PluginQueryContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        let def = PluginOperationDef::from_spec(
            plugin_operation_spec::<Op>(),
            PluginOperationKind::Query,
        );
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations()
                    .typed_query::<Op, _, _>(binder(owner, state)?)
            }),
        )
    }
    pub fn typed_command<Op, F, Fut>(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<F, PluginError> + Send + Sync + 'static,
    ) -> Result<(), PluginError>
    where
        Op: PluginCommand,
        F: Fn(PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<PluginOperationOutcome<Op::Output>, Op::Error>>
            + Send
            + 'static,
    {
        let def = PluginOperationDef::from_spec(
            plugin_operation_spec::<Op>(),
            PluginOperationKind::Command,
        );
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations()
                    .typed_command::<Op, _, _>(binder(owner, state)?)
            }),
        )
    }
    pub fn typed_task<Op, F, Fut>(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<F, PluginError> + Send + Sync + 'static,
    ) -> Result<(), PluginError>
    where
        Op: PluginTask,
        F: Fn(PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<PluginOperationOutcome<Op::Output>, Op::Error>>
            + Send
            + 'static,
    {
        let def =
            PluginOperationDef::from_spec(plugin_operation_spec::<Op>(), PluginOperationKind::Task);
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations()
                    .typed_task::<Op, _, _>(binder(owner, state)?)
            }),
        )
    }
    pub fn typed_command_value<Op, F, Fut>(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<F, PluginError> + Send + Sync + 'static,
    ) -> Result<(), PluginError>
    where
        Op: PluginCommand,
        F: Fn(PluginCommandContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        let def = PluginOperationDef::from_spec(
            plugin_operation_spec::<Op>(),
            PluginOperationKind::Command,
        );
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations()
                    .typed_command_value::<Op, _, _>(binder(owner, state)?)
            }),
        )
    }
    pub fn typed_task_value<Op, F, Fut>(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<F, PluginError> + Send + Sync + 'static,
    ) -> Result<(), PluginError>
    where
        Op: PluginTask,
        F: Fn(PluginTaskContext, Op::Args) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Op::Output, Op::Error>> + Send + 'static,
    {
        let def =
            PluginOperationDef::from_spec(plugin_operation_spec::<Op>(), PluginOperationKind::Task);
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations()
                    .typed_task_value::<Op, _, _>(binder(owner, state)?)
            }),
        )
    }
    pub(crate) fn register(
        self,
        operation: PluginOperationRegistration,
    ) -> Result<(), PluginError> {
        let def = operation.def().clone();
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add(
            ContributionKind::Callback(CallbackSlot::Operation),
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |_, _, reg| reg.operations().register(operation.clone())),
        )
    }
}
type ProcessEngineBinder = Arc<
    dyn Fn(
            &ProcessEngineContributionContext<'_>,
        ) -> Result<crate::ProcessEngineRegistration, PluginError>
        + Send
        + Sync,
>;
#[derive(Clone)]
struct DeclaredProcessEngine {
    kind: String,
    bind: ProcessEngineBinder,
}

type SessionOwnerBinder = Arc<
    dyn Fn(
            &PluginSessionContext,
            &PluginStateView,
            &mut BoundRegistrar,
        ) -> Result<Arc<dyn SessionPlugin>, PluginError>
        + Send
        + Sync,
>;

/// Session-local construction happens only after all recipes pass preflight.
#[derive(Clone)]
pub struct PluginRecipe {
    owner: String,
    pub(crate) configuration: ConfigRegistrar,
    pub(crate) factory_extensions: Vec<PluginExtensionContribution>,
    engines: Vec<DeclaredProcessEngine>,
    pub(crate) entries: Vec<ContributionDescriptor>,
    pub requirements: Requirements,
    bind: SessionOwnerBinder,
}
impl PluginRecipe {
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }
    /// Freeze the codecs registered through the configuration facet.
    pub fn with_configuration(mut self, configuration: ConfigRegistrar) -> Self {
        let offset = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        self.entries.extend(
            configuration
                .descriptors()
                .into_iter()
                .map(|mut descriptor| {
                    descriptor.registration_order =
                        descriptor.registration_order.saturating_add(offset);
                    descriptor
                }),
        );
        self.configuration = configuration;
        self
    }
    pub fn with_factory_extension(mut self, extension: PluginExtensionContribution) -> Self {
        self.entries.push(ContributionDescriptor {
            registry: RegistryKind::Session,
            kind: ContributionKind::Extension,
            key: ContributionKey::Named {
                owner: self.owner.clone(),
                key: extension.extension_id.clone(),
            },
            registration_order: u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
            priority: None,
            details: ContributionDetails::Named,
        });
        self.factory_extensions.push(extension);
        self
    }
    pub fn with_process_engine(
        mut self,
        kind: impl Into<String>,
        binder: impl Fn(
            &ProcessEngineContributionContext<'_>,
        ) -> Result<crate::ProcessEngineRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let kind = kind.into();
        self.entries.push(ContributionDescriptor {
            registry: RegistryKind::Session,
            kind: ContributionKind::ProcessEngine,
            key: ContributionKey::Named {
                owner: self.owner.clone(),
                key: kind.clone(),
            },
            registration_order: u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
            priority: None,
            details: ContributionDetails::Named,
        });
        self.engines.push(DeclaredProcessEngine {
            kind,
            bind: Arc::new(binder),
        });
        self
    }
    pub fn decorate_process_engines(
        mut self,
        decorate: impl Fn(
            crate::ProcessEngineRegistration,
        ) -> Result<crate::ProcessEngineRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let decorate = Arc::new(decorate);
        for engine in &mut self.engines {
            let bind = Arc::clone(&engine.bind);
            let decorate = Arc::clone(&decorate);
            engine.bind = Arc::new(move |context| decorate(bind(context)?));
        }
        self
    }
    pub(crate) fn bind_process_engines(
        &self,
        context: &ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<crate::ProcessEngineRegistration>, PluginError> {
        self.engines
            .iter()
            .map(|entry| {
                let engine = (entry.bind)(context)?;
                if engine.kind() != entry.kind {
                    return Err(PluginError::Registration(format!(
                        "declared engine `{}` bound `{}`",
                        entry.kind,
                        engine.kind()
                    )));
                }
                Ok(engine)
            })
            .collect()
    }

    pub fn new<T: SessionPlugin + 'static>(
        owner: &str,
        registrations: PluginRegistrar<T>,
        binder: impl Fn(&PluginSessionContext, &PluginStateView) -> Result<Arc<T>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let entries = registrations.descriptors(owner, RegistryKind::Session);
        let requirements = registrations.requirements.clone();
        Self {
            owner: owner.into(),
            configuration: ConfigRegistrar::new(owner),
            factory_extensions: Vec::new(),
            engines: Vec::new(),
            entries,
            requirements,
            bind: Arc::new(move |context, state, reg| {
                let owner = binder(context, state)?;
                registrations.bind(owner.as_ref(), state, reg)?;
                Ok(owner)
            }),
        }
    }
    pub fn empty(
        id: &'static str,
        binder: impl Fn(&PluginSessionContext, &PluginStateView) -> Result<(), PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        struct Instance(&'static str);
        impl SessionPlugin for Instance {
            fn id(&self) -> &'static str {
                self.0
            }
        }
        Self::new(
            id,
            PluginRegistrar::<Instance>::new(id),
            move |context, state| {
                binder(context, state)?;
                Ok(Arc::new(Instance(id)))
            },
        )
    }
    pub(crate) fn from_spec(id: &'static str, registrations: PluginRegistrar<()>) -> Self {
        struct Instance(&'static str);
        impl SessionPlugin for Instance {
            fn id(&self) -> &'static str {
                self.0
            }
        }
        Self {
            owner: id.into(),
            configuration: ConfigRegistrar::new(id),
            factory_extensions: Vec::new(),
            engines: Vec::new(),
            entries: registrations.descriptors(id, RegistryKind::Session),
            requirements: registrations.requirements.clone(),
            bind: Arc::new(move |_, state, reg| {
                registrations.bind(&(), state, reg)?;
                Ok(Arc::new(Instance(id)))
            }),
        }
    }
    pub(crate) fn bind(
        &self,
        context: &PluginSessionContext,
        state: &PluginStateView,
        reg: &mut BoundRegistrar,
    ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        (self.bind)(context, state, reg)
    }
}

type ComponentOwnerBinder = Arc<
    dyn Fn(
            &ProcessComponentContext<'_>,
            &PluginStateView,
            &mut BoundRegistrar,
        ) -> Result<(), PluginError>
        + Send
        + Sync,
>;

/// A component's declarations share the session registration model; process
/// restrictions are checked from descriptors before its owner is bound.
pub struct ProcessRecipe {
    pub(crate) entries: Vec<ContributionDescriptor>,
    pub requirements: Requirements,
    bind: ComponentOwnerBinder,
}
impl ProcessRecipe {
    pub fn new<T: Send + Sync + 'static>(
        owner: &str,
        registrations: PluginRegistrar<T>,
        binder: impl Fn(&ProcessComponentContext<'_>) -> Result<Arc<T>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            entries: registrations.descriptors(owner, RegistryKind::Process),
            requirements: registrations.requirements.clone(),
            bind: Arc::new(move |context, state, reg| {
                let owner = binder(context)?;
                registrations.bind(owner.as_ref(), state, reg)
            }),
        }
    }
    pub(crate) fn from_spec(owner: &str, registrations: PluginRegistrar<()>) -> Self {
        Self::new(owner, registrations, |_| Ok(Arc::new(())))
    }
    pub(crate) fn bind(
        &self,
        context: &ProcessComponentContext<'_>,
        state: &PluginStateView,
        reg: &mut BoundRegistrar,
    ) -> Result<(), PluginError> {
        (self.bind)(context, state, reg)
    }
}

/// A host-created pure recipe with one owner binder; callbacks are declared
/// before the binder sees admitted config and state.
pub struct PluginRecipeFactory {
    declaration: PluginDeclaration,
    recipe: PluginRecipe,
}
impl PluginRecipeFactory {
    pub fn new(declaration: PluginDeclaration, recipe: PluginRecipe) -> Self {
        Self {
            declaration,
            recipe,
        }
    }
}
impl PluginMetadata for PluginRecipeFactory {
    fn plugin_declaration(&self) -> PluginDeclaration {
        self.declaration.clone()
    }
}
impl PluginFactory for PluginRecipeFactory {
    fn id(&self) -> &'static str {
        self.declaration.id.as_str()
    }
    fn recipe(self: Arc<Self>) -> std::result::Result<PluginRecipe, PluginError> {
        Ok(self.recipe.clone())
    }
}
