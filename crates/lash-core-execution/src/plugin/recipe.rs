//! Executable declarations frozen before binding an owner.
use super::prompt::{
    PromptSectionFamilySpec, PromptSectionId, PromptSectionSpec, PromptWrapId, PromptWrapSpec,
};
use super::registrar::BoundRegistrar;
use super::registries::{
    Admits, CallbackContribution, Contribution, Process, Registry, Session, contribution,
};
use super::*;
pub use lash_vocabulary::claims::ObligationToken;
pub use lash_vocabulary::claims::RegistryKind;
pub use lash_vocabulary::providers::CallbackSlot;
pub use lash_vocabulary::providers::TraceScopeOf;
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
    TranscriptDecoder,
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
    Callback {
        stream_state_from: Option<HookKey>,
        /// For a tool provider, the execution binding formats it reads,
        /// declared with its key (S8): what its node offers as the codec of
        /// those versions (FIG-6993). Empty for every other callback.
        reads: &'static [lash_vocabulary::providers::FormatVersion],
    },
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
    Owner(lash_vocabulary::identity::RuntimeOwner),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContributionReadiness {
    /// Declarations have not passed deployment preflight and serve no callbacks.
    NotValidated,
    Ready,
    Refused(PreflightIssue),
}
#[derive(Clone, Debug)]
pub struct InstalledInventory {
    pub scope: InventoryScope,
    pub entries: Vec<ContributionDescriptor>,
    pub readiness: Vec<ContributionReadiness>,
    /// The plugins whose recipes declare they keep no state
    /// ([`PluginRecipe::stateless`]).
    pub stateless: std::collections::BTreeSet<String>,
}
impl InstalledInventory {
    /// Whether `plugin` may publish state: every plugin but one whose recipe
    /// declares it stateless.
    pub fn keeps_state(&self, plugin: &str) -> bool {
        !self.stateless.contains(plugin)
    }

    pub fn callbacks(&self) -> impl Iterator<Item = &PluginCallbackIdentity> {
        self.entries.iter().filter_map(|entry| match &entry.key {
            ContributionKey::Callback(key) => Some(key),
            _ => None,
        })
    }

    /// What these registrations offer running code (FIG-6993): each ready
    /// tool provider, by key, and the codec of each binding format it
    /// declares it reads. What a node advertises beside its package code.
    pub fn offers(&self) -> lash_kernel_doc::NodeCapabilities {
        let prefix = format!("{}:", CallbackSlot::ToolProvider.key_prefix());
        let mut offers = lash_kernel_doc::NodeCapabilities::default();
        for (entry, readiness) in self.entries.iter().zip(&self.readiness) {
            let (
                ContributionKey::Callback(identity),
                ContributionReadiness::Ready,
                ContributionDetails::Callback { reads, .. },
            ) = (&entry.key, readiness, &entry.details)
            else {
                continue;
            };
            if entry.kind != ContributionKind::Callback(CallbackSlot::ToolProvider) {
                continue;
            }
            let Some(key) = identity.key.strip_prefix(&prefix) else {
                continue;
            };
            let provider = format!("{}/{key}", identity.owner);
            offers
                .codecs
                .extend(reads.iter().map(|format| (provider.clone(), format.get())));
            offers.providers.insert(provider);
        }
        offers
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
/// A pure list of typed contributions for the registry `R`. Every entry
/// contains its descriptor and the binder that supplies its declared
/// implementation. A method exists only for the registries the contribution
/// table admits its contribution in ([`super::registries`]).
pub struct PluginRegistrar<T = (), R = Session> {
    entries: Vec<Registration<T>>,
    owner: Option<String>,
    registry: std::marker::PhantomData<fn() -> R>,
    pub requirements: Requirements,
}
impl<T, R> Clone for PluginRegistrar<T, R> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            registry: std::marker::PhantomData,
            entries: self.entries.clone(),
            requirements: self.requirements.clone(),
        }
    }
}
impl<R: Registry> Default for PluginRegistrar<(), R> {
    fn default() -> Self {
        Self::owned(None)
    }
}
impl<T: Send + Sync + 'static> PluginRegistrar<T, Session> {
    /// A session plugin's registrar, owned by `owner`.
    pub fn session(owner: &str) -> Self {
        Self::owned(Some(owner.into()))
    }

    /// A process component's registrar, with its registry named by this
    /// constructor rather than a type argument at the call site.
    pub fn process(owner: &str) -> PluginRegistrar<T, Process> {
        PluginRegistrar::owned(Some(owner.into()))
    }

    /// A session plugin's registrar, owned by `owner`.
    pub fn new(owner: &str) -> Self {
        Self::owned(Some(owner.into()))
    }
}
impl<T: Send + Sync + 'static> PluginRegistrar<T, Process> {
    /// A process component's registrar, owned by `owner`.
    pub fn for_process(owner: &str) -> Self {
        Self::owned(Some(owner.into()))
    }
}
impl<T, R: Registry> PluginRegistrar<T, R> {
    fn owned(owner: Option<String>) -> Self {
        Self {
            owner,
            registry: std::marker::PhantomData,
            entries: Vec::new(),
            requirements: Requirements::default(),
        }
    }

    /// These declarations as `Into`'s, each judged by the contribution
    /// table: what a spec declared for one registry and offered to another
    /// (a session plugin's spec as a process component) arrives as data.
    pub(crate) fn admitted_in<Into: Registry>(
        self,
        owner: &str,
    ) -> Result<PluginRegistrar<T, Into>, PluginError> {
        let mut entries = self.entries;
        for entry in &mut entries {
            let descriptor = &mut entry.descriptor;
            if !super::registries::admits(&descriptor.kind, &descriptor.details, Into::KIND) {
                return Err(refused(Into::KIND, Some(owner), &descriptor.kind));
            }
            descriptor.registry = Into::KIND;
        }
        Ok(PluginRegistrar {
            entries,
            owner: self.owner,
            registry: std::marker::PhantomData,
            requirements: self.requirements,
        })
    }
}
fn refused(registry: RegistryKind, owner: Option<&str>, kind: &ContributionKind) -> PluginError {
    let installer = match registry {
        RegistryKind::Session => "session plugin",
        RegistryKind::Process => "process component",
    };
    PluginError::Registration(match owner {
        Some(owner) => format!("{installer} `{owner}` cannot supply {kind:?}"),
        None => format!("a {installer} cannot supply {kind:?}"),
    })
}
impl<T: Send + Sync + 'static, R: Registry> PluginRegistrar<T, R> {
    pub fn require(&mut self, service: ServiceKind) {
        self.requirements.services.insert(service);
    }
    fn add<C: Contribution>(
        &mut self,
        mut key: ContributionKey,
        priority: Option<i32>,
        details: ContributionDetails,
        bind: EntryBinder<T>,
    ) -> Result<(), PluginError>
    where
        R: Admits<C>,
    {
        let kind = C::KIND;
        if !super::registries::admits(&kind, &details, R::KIND) {
            return Err(refused(R::KIND, self.owner.as_deref(), &kind));
        }
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
            return Err(PluginError::Registration(match (&details, &key) {
                (ContributionDetails::Operation(def), _) => {
                    format!("duplicate plugin operation name `{}`", def.name)
                }
                (ContributionDetails::Callback { .. }, ContributionKey::Callback(identity)) => {
                    match &self.owner {
                        Some(owner) => {
                            format!("duplicate hook key `{}` for plugin `{owner}`", identity.key)
                        }
                        None => format!("duplicate hook key `{}`", identity.key),
                    }
                }
                _ => format!("duplicate registration {key:?}"),
            }));
        }
        let registration_order = u32::try_from(self.entries.len())
            .map_err(|_| PluginError::Registration("too many registrations".into()))?;
        self.entries.push(Registration {
            descriptor: ContributionDescriptor {
                registry: R::KIND,
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
    fn callback<C: CallbackContribution>(
        &mut self,
        key: Option<HookKey>,
        priority: Option<i32>,
        details: ContributionDetails,
        bind: EntryBinder<T>,
    ) -> Result<(), PluginError>
    where
        R: Admits<C>,
    {
        let slot = C::SLOT;
        let key = key.map_or_else(
            || slot.key_prefix().to_owned(),
            |key| format!("{}:{key}", slot.key_prefix()),
        );
        self.add::<C>(
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::StateReducer>,
    {
        let name = name.into();
        self.add::<contribution::StateReducer>(
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::Extension>,
    {
        let id = id.into();
        self.add::<contribution::Extension>(
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
    pub fn prompt(&mut self) -> PromptRecipeRegistrations<'_, T, R> {
        PromptRecipeRegistrations { reg: self }
    }
    pub fn operations(&mut self) -> OperationRecipeRegistrations<'_, T, R> {
        OperationRecipeRegistrations { reg: self }
    }
    pub(crate) fn descriptors(&self, owner: &str) -> Vec<ContributionDescriptor> {
        self.entries
            .iter()
            .map(|entry| {
                let mut descriptor = entry.descriptor.clone();
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
        key: ToolProviderKey,
    ) -> Result<ToolProviderHandle, PluginError> {
        let handle = ToolProviderHandle::new(R::KIND, owner, key);
        let identity = handle.callback_identity();
        if self.descriptors(owner).iter().any(|entry| {
            entry.kind == ContributionKind::Callback(CallbackSlot::ToolProvider)
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
        keyed_facet!($facet, $method => $method, $( $name : $slot => $ty ),+);
    };
    ($facet:ident, $method:ident => $bound_method:ident, $( $name:ident : $slot:ident => $ty:ty ),+ $(,)?) => {
        pub struct $facet<'a, T = (), R = Session> { reg: &'a mut PluginRegistrar<T, R> }
        impl<T: Send + Sync + 'static, R: Registry> PluginRegistrar<T, R> {
            pub fn $method(&mut self) -> $facet<'_, T, R> { $facet { reg: self } }
        }
        impl<T: Send + Sync + 'static, R: Registry> $facet<'_, T, R> {
            $(pub fn $name(self, key: HookKey, binder: impl Fn(&T, &PluginStateView) -> Result<$ty, PluginError> + Send + Sync + 'static) -> Result<(), PluginError>
            where R: Admits<contribution::$slot> {
                self.reg.callback::<contribution::$slot>(Some(key), None, ContributionDetails::Callback { stream_state_from: None, reads: &[] },
                    Arc::new(move |owner, state, reg| reg.$bound_method().$name(key, binder(owner, state)?)))
            })+
        }
    }
}
keyed_facet!(TurnRegistrations, turn, before: BeforeTurn => BeforeTurnHook, after: AfterTurn => AfterTurnHook, checkpoint: Checkpoint => CheckpointHook);
keyed_facet!(ToolCallRegistrations, tool_calls, transform_args: ToolArgsTransform => ToolArgsTransformHook, check_args: ToolArgsCheck => ToolArgsCheckHook, transform_result: ToolResultTransform => ToolResultTransformHook, check_result: ToolResultCheck => ToolResultCheckHook);
keyed_facet!(ToolCatalogRegistrations, tool_catalog, contribute: ToolCatalog => ToolCatalogContributor);
keyed_facet!(SessionRegistrations, lifecycle => session, on_event: RuntimeEvent => PluginLifecycleEventHook);
keyed_facet!(OutputRegistrations, output, stream: AssistantStream => AssistantStreamHook, stream_finished: AssistantStreamFinished => AssistantStreamFinishedHook, cell_failure_feedback: CellFailureFeedback => CellFailureFeedbackHook);
keyed_facet!(ToolResultRegistrations, tool_results, presentation_step: PresentationStep => ToolPresentationStep);
macro_rules! singleton {
    ($facet:ident, $method:ident, $( $name:ident : $slot:ident => $ty:ty ),+ $(,)?) => {
        pub struct $facet<'a, T = (), R = Session> { reg: &'a mut PluginRegistrar<T, R> }
        impl<T: Send + Sync + 'static, R: Registry> PluginRegistrar<T, R> { pub fn $method(&mut self) -> $facet<'_, T, R> { $facet { reg: self } } }
        impl<T: Send + Sync + 'static, R: Registry> $facet<'_, T, R> {
            $(pub fn $name(self, binder: impl Fn(&T, &PluginStateView) -> Result<$ty, PluginError> + Send + Sync + 'static) -> Result<(), PluginError>
            where R: Admits<contribution::$slot> {
                self.reg.callback::<contribution::$slot>(None, None, ContributionDetails::Callback { stream_state_from: None, reads: &[] },
                    Arc::new(move |owner, state, reg| reg.$method().$name(binder(owner, state)?)))
            })+
        }
    }
}
singleton!(ProtocolRegistrations, protocol, session: ProtocolSession => Arc<dyn ProtocolSessionPlugin>, protocol_driver: ProtocolDriver => Arc<dyn ProtocolDriverPlugin>);
singleton!(ExecutionRegistrations, execution, code_executor: CodeExecutor => Arc<dyn CodeExecutorPlugin>);
impl<T: Send + Sync + 'static, R: Registry> OutputRegistrations<'_, T, R> {
    pub fn response(
        self,
        key: HookKey,
        stream_state_from: Option<HookKey>,
        binder: impl Fn(&T, &PluginStateView) -> Result<AssistantResponseHook, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::AssistantResponse>,
    {
        self.reg.callback::<contribution::AssistantResponse>(
            Some(key),
            None,
            ContributionDetails::Callback {
                stream_state_from,
                reads: &[],
            },
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::AssistantProseProjector>,
    {
        self.reg.callback::<contribution::AssistantProseProjector>(
            None,
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
                reads: &[],
            },
            Arc::new(move |owner, state, reg| {
                reg.output()
                    .assistant_prose_projector(binder(owner, state)?)
            }),
        )
    }
}
impl<T: Send + Sync + 'static, R: Registry> ToolResultRegistrations<'_, T, R> {
    pub fn presenter(
        self,
        binder: impl Fn(&T, &PluginStateView) -> Result<ToolPresentationPresenter, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::PresentationPresenter>,
    {
        self.reg.callback::<contribution::PresentationPresenter>(
            None,
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
                reads: &[],
            },
            Arc::new(move |owner, state, reg| reg.tool_results().presenter(binder(owner, state)?)),
        )
    }
}
pub struct ContextRegistrations<'a, T = (), R = Session> {
    reg: &'a mut PluginRegistrar<T, R>,
}
impl<T: Send + Sync + 'static, R: Registry> PluginRegistrar<T, R> {
    pub fn context(&mut self) -> ContextRegistrations<'_, T, R> {
        ContextRegistrations { reg: self }
    }
}
impl<T: Send + Sync + 'static, R: Registry> ContextRegistrations<'_, T, R> {
    pub fn pressure(
        self,
        key: HookKey,
        priority: i32,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ContextPressureHook>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::ContextPressure>,
    {
        self.reg.callback::<contribution::ContextPressure>(
            Some(key),
            Some(priority),
            ContributionDetails::Callback {
                stream_state_from: None,
                reads: &[],
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::ContextCompactor>,
    {
        self.reg.callback::<contribution::ContextCompactor>(
            Some(key),
            Some(priority),
            ContributionDetails::Callback {
                stream_state_from: None,
                reads: &[],
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::AttachmentOmission>,
    {
        self.reg.callback::<contribution::AttachmentOmission>(
            Some(key),
            Some(priority),
            ContributionDetails::Callback {
                stream_state_from: None,
                reads: &[],
            },
            Arc::new(move |owner, state, reg| {
                reg.context()
                    .attachment_omissions(key, priority, binder(owner, state)?)
            }),
        )
    }
}
pub struct ToolRegistrations<'a, T = (), R = Session> {
    reg: &'a mut PluginRegistrar<T, R>,
}
impl<T: Send + Sync + 'static, R: Registry> PluginRegistrar<T, R> {
    pub fn tools(&mut self) -> ToolRegistrations<'_, T, R> {
        ToolRegistrations { reg: self }
    }
}
impl<T: Send + Sync + 'static, R: Registry> ToolRegistrations<'_, T, R> {
    pub fn provider(
        self,
        key: ToolProviderKey,
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(R::KIND, owner, key);
        self.record_provider(key, &[], binder)?;
        Ok(handle)
    }
    /// Declare the provider `key` reading execution bindings in `reads`,
    /// the formats its instances answer from
    /// [`ToolProvider::readable_binding_formats`]: a node that installs it
    /// offers the codec of each (FIG-6993). Binding refuses an instance
    /// that reads other formats.
    pub fn provider_reading(
        self,
        key: ToolProviderKey,
        reads: &'static [lash_vocabulary::providers::FormatVersion],
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(R::KIND, owner, key);
        self.record_provider(key, reads, binder)?;
        Ok(handle)
    }
    /// Declare the provider `key` with the reader its `Read` tools need:
    /// the binder's registration must supply both
    /// ([`crate::ToolProviderRegistration::with_reads`]). The reader is its
    /// own callback, which a node serves only when it registers it, and
    /// which a cell's read plan owes.
    pub fn provider_with_reads(
        self,
        key: ToolProviderKey,
        binder: impl Fn(&T, &PluginStateView) -> Result<crate::ToolProviderRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(R::KIND, owner, key);
        self.record_provider_with_reads(key, &[], binder)?;
        Ok(handle)
    }
    pub(crate) fn record_provider_with_reads(
        self,
        key: ToolProviderKey,
        reads: &'static [lash_vocabulary::providers::FormatVersion],
        binder: impl Fn(&T, &PluginStateView) -> Result<crate::ToolProviderRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        self.reg.callback::<contribution::ToolProvider>(
            Some(HookKey::__literal(key.as_str())),
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
                reads,
            },
            Arc::new(move |owner, state, reg| {
                let registration = binder(owner, state)?;
                declared_reads(key, reads, registration.provider.as_ref())?;
                reg.tools().provider_with_reads(key, registration)
            }),
        )?;
        // The reader binds with its provider, above: this entry declares it.
        self.reg.callback::<contribution::ToolReads>(
            Some(HookKey::__literal(key.as_str())),
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
                reads: &[],
            },
            Arc::new(|_, _, _| Ok(())),
        )
    }
    pub(crate) fn record_provider(
        self,
        key: ToolProviderKey,
        reads: &'static [lash_vocabulary::providers::FormatVersion],
        binder: impl Fn(&T, &PluginStateView) -> Result<Arc<dyn ToolProvider>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        self.reg.callback::<contribution::ToolProvider>(
            Some(HookKey::__literal(key.as_str())),
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
                reads,
            },
            Arc::new(move |owner, state, reg| {
                let provider = binder(owner, state)?;
                declared_reads(key, reads, provider.as_ref())?;
                reg.tools().provider(key, provider)
            }),
        )
    }
    pub fn deferred(
        self,
        key: ToolProviderKey,
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<crate::DeferredToolProviderRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(R::KIND, owner, key);
        self.record_deferred(key, &[], binder)?;
        Ok(handle)
    }
    /// [`Self::deferred`], the provider reading execution bindings in
    /// `reads`, as [`Self::provider_reading`] declares them (FIG-6993).
    pub fn deferred_reading(
        self,
        key: ToolProviderKey,
        reads: &'static [lash_vocabulary::providers::FormatVersion],
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<crate::DeferredToolProviderRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<ToolProviderHandle, PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        let owner = self.reg.owner.clone().ok_or_else(|| {
            PluginError::Registration("a provider handle requires its declared owner".into())
        })?;
        let handle = ToolProviderHandle::new(R::KIND, owner, key);
        self.record_deferred(key, reads, binder)?;
        Ok(handle)
    }
    pub(crate) fn record_deferred(
        self,
        key: ToolProviderKey,
        reads: &'static [lash_vocabulary::providers::FormatVersion],
        binder: impl Fn(
            &T,
            &PluginStateView,
        ) -> Result<crate::DeferredToolProviderRegistration, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::ToolProvider>,
    {
        self.reg.callback::<contribution::ToolProvider>(
            Some(HookKey::__literal(key.as_str())),
            None,
            ContributionDetails::Callback {
                stream_state_from: None,
                reads,
            },
            Arc::new(move |owner, state, reg| {
                let registration = binder(owner, state)?;
                declared_reads(key, reads, registration.provider.as_ref())?;
                reg.tools().deferred(key, registration)
            }),
        )
    }
}
/// Refuses a bound provider whose instance reads binding formats other
/// than `declared`, what its registration offers (S8, FIG-6993).
fn declared_reads(
    key: ToolProviderKey,
    declared: &'static [lash_vocabulary::providers::FormatVersion],
    provider: &(impl ToolProvider + ?Sized),
) -> Result<(), PluginError> {
    let reads = provider.readable_binding_formats();
    if reads == declared {
        return Ok(());
    }
    Err(PluginError::Registration(format!(
        "provider `{key}` reads binding formats {reads:?}, and its registration declares {declared:?}"
    )))
}
pub struct PromptRecipeRegistrations<'a, T = (), R = Session> {
    reg: &'a mut PluginRegistrar<T, R>,
}
impl<T: Send + Sync + 'static, R: Registry> PromptRecipeRegistrations<'_, T, R> {
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::PromptSection>,
    {
        self.reg.add::<contribution::PromptSection>(
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::PromptFamily>,
    {
        self.reg.add::<contribution::PromptFamily>(
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
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::PromptWrapper>,
    {
        self.reg.add::<contribution::PromptWrapper>(
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
pub struct OperationRecipeRegistrations<'a, T = (), R = Session> {
    reg: &'a mut PluginRegistrar<T, R>,
}
impl<T: Send + Sync + 'static, R: Registry> OperationRecipeRegistrations<'_, T, R> {
    #[cfg(test)]
    pub(crate) fn query(
        self,
        spec: PluginOperationSpec,
        binder: impl Fn(&T, &PluginStateView) -> Result<PluginQueryHandler, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), PluginError>
    where
        R: Admits<contribution::Operation>,
    {
        let def = PluginOperationDef::from_spec(spec.clone(), PluginOperationKind::Query);
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add::<contribution::Operation>(
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
        R: Admits<contribution::Operation>,
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
        self.reg.add::<contribution::Operation>(
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
        R: Admits<contribution::Operation>,
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
        self.reg.add::<contribution::Operation>(
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
        R: Admits<contribution::Operation>,
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
        self.reg.add::<contribution::Operation>(
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
        R: Admits<contribution::Operation>,
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
        self.reg.add::<contribution::Operation>(
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
        R: Admits<contribution::Operation>,
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
        self.reg.add::<contribution::Operation>(
            key,
            None,
            ContributionDetails::Operation(def),
            Arc::new(move |owner, state, reg| {
                reg.operations()
                    .typed_task_value::<Op, _, _>(binder(owner, state)?)
            }),
        )
    }
    pub(crate) fn register(self, operation: PluginOperationRegistration) -> Result<(), PluginError>
    where
        R: Admits<contribution::Operation>,
    {
        let def = operation.def().clone();
        let key = ContributionKey::Callback(PluginCallbackIdentity {
            owner: String::new(),
            key: format!("{}:{}", CallbackSlot::Operation.key_prefix(), def.name),
        });
        self.reg.add::<contribution::Operation>(
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
    pub(crate) transcript_decoder: Option<Arc<dyn TranscriptDecoderPlugin>>,
    pub(crate) entries: Vec<ContributionDescriptor>,
    pub requirements: Requirements,
    pub(crate) stateless: bool,
    bind: SessionOwnerBinder,
}
impl PluginRecipe {
    /// Count executable callbacks supplied by this recipe's binders.
    /// Metadata and the admission/execution path stay the recipe's own.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn counted(mut self, counts: crate::testing::CallbackCounts) -> Self {
        let bind = self.bind;
        self.bind = Arc::new(move |context, state, reg| {
            let owner = bind(context, state, reg)?;
            counts.wrap(&mut reg.contributions);
            Ok(owner)
        });
        self
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }
    /// Declare that the plugin keeps no state. Its namespace stays empty:
    /// every state command its callbacks or tool bodies return is refused as
    /// [`DecisionOnly`](crate::plugin::StateCommandRefusal::DecisionOnly), and
    /// installing it with a state reducer or initial values is refused.
    /// Certification then reports its state law not applicable.
    #[must_use]
    pub fn stateless(mut self) -> Self {
        self.stateless = true;
        self
    }
    /// Freeze the codecs registered through the configuration facet.
    pub fn with_configuration(mut self, configuration: ConfigRegistrar) -> Self {
        self.entries.retain(|entry| {
            !matches!(
                entry.kind,
                ContributionKind::ConfigNamespace | ContributionKind::ConfigCommand
            )
        });
        for (index, entry) in self.entries.iter_mut().enumerate() {
            entry.registration_order = u32::try_from(index).unwrap_or(u32::MAX);
        }
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
    /// Install the pure history decoder and its owner-attributed descriptor together.
    pub fn with_transcript_decoder(mut self, decoder: Arc<dyn TranscriptDecoderPlugin>) -> Self {
        if self.transcript_decoder.is_none() {
            self.entries.push(ContributionDescriptor {
                registry: RegistryKind::Session,
                kind: ContributionKind::TranscriptDecoder,
                key: ContributionKey::Named {
                    owner: self.owner.clone(),
                    key: "transcript".into(),
                },
                registration_order: u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
                priority: None,
                details: ContributionDetails::Named,
            });
        }
        self.transcript_decoder = Some(decoder);
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
        let entries = registrations.descriptors(owner);
        let requirements = registrations.requirements.clone();
        Self {
            owner: owner.into(),
            configuration: ConfigRegistrar::new(owner),
            factory_extensions: Vec::new(),
            engines: Vec::new(),
            transcript_decoder: None,
            entries,
            requirements,
            stateless: false,
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
    /// Bind plain session data to prompt, state, tool and config entries
    /// without a marker `SessionPlugin` implementation. Use `new` when the
    /// owner supplies its own readiness callback.
    pub fn bound<T: Send + Sync + 'static>(
        id: &'static str,
        registrations: PluginRegistrar<T>,
        binder: impl Fn(&PluginSessionContext, &PluginStateView) -> Result<Arc<T>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        struct Instance<T> {
            id: &'static str,
            _owner: Arc<T>,
        }
        impl<T: Send + Sync> SessionPlugin for Instance<T> {
            fn id(&self) -> &'static str {
                self.id
            }
        }
        Self {
            owner: id.into(),
            configuration: ConfigRegistrar::new(id),
            factory_extensions: Vec::new(),
            engines: Vec::new(),
            transcript_decoder: None,
            entries: registrations.descriptors(id),
            requirements: registrations.requirements.clone(),
            stateless: false,
            bind: Arc::new(move |context, state, reg| {
                let owner = binder(context, state)?;
                registrations.bind(owner.as_ref(), state, reg)?;
                Ok(Arc::new(Instance { id, _owner: owner }))
            }),
        }
    }
    /// A joined registrar whose callbacks need no session-local owner data.
    pub fn from_registrar(id: &'static str, registrations: PluginRegistrar<()>) -> Self {
        Self::bound(id, registrations, |_, _| Ok(Arc::new(())))
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

/// A component's declarations share the session registration model through
/// a process registrar, which offers only what the contribution table
/// admits in a process.
pub struct ProcessRecipe {
    pub(crate) entries: Vec<ContributionDescriptor>,
    pub requirements: Requirements,
    bind: ComponentOwnerBinder,
}
impl ProcessRecipe {
    pub fn new<T: Send + Sync + 'static>(
        owner: &str,
        registrations: PluginRegistrar<T, Process>,
        binder: impl Fn(&ProcessComponentContext<'_>) -> Result<Arc<T>, PluginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            entries: registrations.descriptors(owner),
            requirements: registrations.requirements.clone(),
            bind: Arc::new(move |context, state, reg| {
                let owner = binder(context)?;
                registrations.bind(owner.as_ref(), state, reg)
            }),
        }
    }
    pub(crate) fn from_spec(owner: &str, registrations: PluginRegistrar<(), Process>) -> Self {
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
