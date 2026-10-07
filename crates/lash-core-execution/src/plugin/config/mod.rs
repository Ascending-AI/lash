//! Recorded session config and the typed commands that change it (FIG-4379).
//!
//! A session records every installed owner's config namespace with its
//! config head, the protocol's among them, beside the core owner's share
//! (model, reasoning, attachment acceptance, generation, budget and
//! tool access). Each
//! namespace has one owner:
//!
//! - The host collects owners when configuration is first used, after format
//!   preflight ([`PluginFactory::register_config`](super::PluginFactory::register_config)):
//!   its [`ConfigOwner`] creates the namespace from the creator's input, its
//!   defaults and its parent's recorded value, and validates every candidate.
//! - The owner's typed [`ConfigCommand`]s are the only changes the namespace
//!   admits. A setting no command changes is immutable by construction.
//!   A reducer sees the recorded namespace and its command, and nothing else:
//!   no graph, process, session-write or I/O services. The core owner's
//!   model command alone also reads the host's models, to mint the binding
//!   its key names; the resolution records that binding, so nothing
//!   re-derives it.
//! - A config transaction orders commands of any owners. It resolves once,
//!   over one private candidate, into a recorded
//!   [`ConfigResolution`], which one fenced commit publishes with one config
//!   revision step ([`ConfigRegistry::resolve`]).
//! - An owner's reducers are its plugin's behaviour (FIG-4791): a change to
//!   them is a bump of the plugin's declared
//!   [`behavior_revision`](super::PluginDeclaration::behavior_revision),
//!   which the build generation hashes. They have no identity of their own
//!   and a transaction records none: the build whose lane admits its command
//!   run resolves it, and a redrive of that run stays on that lane.
//!
//! The [`ConfigRegistry`] holds every registration: the one list the
//! resolver, the ingress check and the [catalog](ConfigRegistry::catalog)
//! are generated from.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use self::core::{CoreConfigOwner, CoreConfigRefusal};
pub use lash_core_store::config_transaction::{
    CORE_CONFIG_OWNER, ConfigCommandEntry, ConfigFault, ConfigRefusal, ConfigRefusalReason,
    ConfigResolution, ConfigResolutionDecision, ConfigTransactionOutcome, ConfigTransactionRecord,
    ConfigValueRole, CoreConfig, RecordedNamespaceCorrupt, RefusalSite,
};
pub use lash_core_store::execution_state::{AdmittedPluginConfig, PluginConfig};

use super::{PluginFactory, PluginOptions};

pub mod core;

/// What a config value, command, output or refusal must be: wire data with a
/// generated schema.
pub trait ConfigWire:
    Serialize + DeserializeOwned + schemars::JsonSchema + Send + Sync + 'static
{
}

impl<T> ConfigWire for T where
    T: Serialize + DeserializeOwned + schemars::JsonSchema + Send + Sync + 'static
{
}

/// What an owner creates its namespace from.
#[derive(Clone, Copy, Debug)]
pub struct CreationFacts<'a, R> {
    /// The creating parent's recorded namespace, for a child session: the
    /// owner decides what a child inherits.
    pub parent: Option<&'a R>,
    /// Whether the session being created is a root session.
    pub is_root_session: bool,
}

/// The candidate config an owner validates its namespace in: the core share
/// and every namespace, as they will be published together.
#[derive(Clone, Copy, Debug)]
pub struct CandidateFacts<'a> {
    pub core: &'a CoreConfig,
    pub plugin_config: &'a PluginConfig,
}

/// The owner of one recorded config namespace.
pub trait ConfigOwner: Send + Sync + 'static {
    /// What a creator states for this namespace.
    type Create: ConfigWire;
    /// The recorded namespace.
    type Recorded: ConfigWire + Clone;
    /// The owner's typed refusal.
    type Refusal: ConfigWire + std::fmt::Display;
    /// What a run states for this namespace: its own typed shape, narrower
    /// than the recorded namespace. What only creation or a config command
    /// may set (a pin, the prompt config, FIG-4589) is no field of it, so a
    /// run that states one does not decode, whatever value it states. An
    /// owner whose namespace no run overrides uses [`NoRunOptions`].
    type RunOptions: ConfigWire;

    /// The namespace a session being created records: the creator's input,
    /// this owner's defaults, and what a child inherits from `facts.parent`.
    /// `None` records nothing. Called at creation only, never on an open.
    fn create(
        &self,
        input: Option<Self::Create>,
        facts: CreationFacts<'_, Self::Recorded>,
    ) -> Result<Option<Self::Recorded>, Self::Refusal>;

    /// Validate `value` as this owner's namespace within the final
    /// candidate. `base` is the namespace the candidate was derived from:
    /// a run override is judged against it.
    fn validate(
        &self,
        value: &Self::Recorded,
        base: Option<&Self::Recorded>,
        facts: &CandidateFacts<'_>,
    ) -> Result<(), Self::Refusal>;

    /// The namespace a run executes under: `recorded` with the run's
    /// `options` laid over it. The owner alone knows how its options meet
    /// its namespace; nothing else merges them. The result is then validated
    /// against `recorded` like any candidate.
    fn apply_run_options(
        &self,
        recorded: &Self::Recorded,
        options: Self::RunOptions,
    ) -> Result<Self::Recorded, Self::Refusal>;
}

/// The run options of an owner whose namespace no run overrides: the empty
/// object, and nothing else.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct NoRunOptions {}

/// One allowed change to an owner's namespace.
pub trait ConfigCommand: ConfigWire {
    type Owner: ConfigOwner;
    /// What the command answers when it applies.
    type Output: ConfigWire;
    /// The command's registered name, unique within its owner.
    const NAME: &'static str;
}

/// What a reducer returns: the owner's next namespace and the command's
/// output.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnerChange<R, O> {
    pub recorded: R,
    pub output: O,
}

type RecordedOf<C> = <<C as ConfigCommand>::Owner as ConfigOwner>::Recorded;
type RefusalOf<C> = <<C as ConfigCommand>::Owner as ConfigOwner>::Refusal;

/// A config registration that cannot stand.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigRegistrationError {
    #[error("plugin `{plugin_id}` registered a second config owner")]
    DuplicateOwner { plugin_id: String },
    #[error("plugin `{plugin_id}` registered config command `{command}` twice")]
    DuplicateCommand { plugin_id: String, command: String },
    #[error(
        "plugin `{plugin_id}` registered config command `{command}` for an owner type it did not \
         register"
    )]
    ForeignCommand { plugin_id: String, command: String },
    #[error("no plugin may own the reserved config owner id `{owner}`")]
    ReservedOwner { owner: String },
}

/// Where a factory registers its config owner and commands. The namespace is
/// the registering factory's id.
pub struct ConfigRegistrar {
    owner_id: String,
    owner: Option<RegisteredOwner>,
}

impl ConfigRegistrar {
    fn new(owner_id: impl Into<String>) -> Self {
        Self {
            owner_id: owner_id.into(),
            owner: None,
        }
    }

    /// Register this plugin's config owner.
    pub fn owner<O: ConfigOwner>(&mut self, owner: O) -> Result<(), ConfigRegistrationError> {
        if self.owner.is_some() {
            return Err(ConfigRegistrationError::DuplicateOwner {
                plugin_id: self.owner_id.clone(),
            });
        }
        self.owner = Some(RegisteredOwner {
            type_id: TypeId::of::<O>(),
            owner: Arc::new(TypedOwner(owner)),
            commands: BTreeMap::new(),
            command_types: BTreeMap::new(),
        });
        Ok(())
    }

    /// Register the typed command `C` of this plugin's owner, reduced by
    /// `reduce`.
    pub fn command<C: ConfigCommand>(
        &mut self,
        reduce: impl Fn(
            &RecordedOf<C>,
            C,
        ) -> Result<OwnerChange<RecordedOf<C>, C::Output>, RefusalOf<C>>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), ConfigRegistrationError> {
        let plugin_id = self.owner_id.clone();
        let Some(owner) = self
            .owner
            .as_mut()
            .filter(|owner| owner.type_id == TypeId::of::<C::Owner>())
        else {
            return Err(ConfigRegistrationError::ForeignCommand {
                plugin_id,
                command: C::NAME.to_string(),
            });
        };
        if owner.commands.contains_key(C::NAME) {
            return Err(ConfigRegistrationError::DuplicateCommand {
                plugin_id,
                command: C::NAME.to_string(),
            });
        }
        owner
            .command_types
            .insert(TypeId::of::<C>(), C::NAME.to_string());
        owner.commands.insert(
            C::NAME.to_string(),
            Arc::new(TypedCommand::<C, _> {
                reduce,
                _command: std::marker::PhantomData,
            }),
        );
        Ok(())
    }

    /// Register the core command `C`, whose reducer also reads the host's
    /// models. Only the core owner registers one: a plugin's reducer never
    /// sees the host's models.
    fn llm_profiles_command<C: ConfigCommand>(
        &mut self,
        reduce: impl Fn(
            &RecordedOf<C>,
            C,
            &dyn crate::LlmProfiles,
        ) -> Result<OwnerChange<RecordedOf<C>, C::Output>, RefusalOf<C>>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), ConfigRegistrationError> {
        let plugin_id = self.owner_id.clone();
        let Some(owner) = self
            .owner
            .as_mut()
            .filter(|owner| owner.type_id == TypeId::of::<C::Owner>())
        else {
            return Err(ConfigRegistrationError::ForeignCommand {
                plugin_id,
                command: C::NAME.to_string(),
            });
        };
        if owner.commands.contains_key(C::NAME) {
            return Err(ConfigRegistrationError::DuplicateCommand {
                plugin_id,
                command: C::NAME.to_string(),
            });
        }
        owner
            .command_types
            .insert(TypeId::of::<C>(), C::NAME.to_string());
        owner.commands.insert(
            C::NAME.to_string(),
            Arc::new(LlmProfilesCommand::<C, _> {
                reduce,
                _command: std::marker::PhantomData,
            }),
        );
        Ok(())
    }
}

#[derive(Clone)]
struct RegisteredOwner {
    type_id: TypeId,
    owner: Arc<dyn ErasedOwner>,
    commands: BTreeMap<String, Arc<dyn ErasedCommand>>,
    command_types: BTreeMap<TypeId, String>,
}

/// The generated schemas of one registered command.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ConfigCommandDescriptor {
    pub owner: String,
    pub command: String,
    #[schemars(with = "serde_json::Value")]
    pub input_schema: serde_json::Value,
    #[schemars(with = "serde_json::Value")]
    pub output_schema: serde_json::Value,
    #[schemars(with = "serde_json::Value")]
    pub refusal_schema: serde_json::Value,
}

/// Every config command a session admits, generated from the registrations,
/// with the config revision it describes. It is discovery: it promises no
/// supplied value will pass its owner's validation, and it authorizes
/// nothing.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ConfigCommandCatalog {
    pub revision: u64,
    pub commands: Vec<ConfigCommandDescriptor>,
}

trait ErasedOwner: Send + Sync {
    fn refusal_schema(&self) -> serde_json::Value;
    fn create(
        &self,
        owner_id: &str,
        input: Option<&serde_json::Value>,
        parent: Option<&serde_json::Value>,
        is_root_session: bool,
    ) -> Result<Option<serde_json::Value>, ConfigFault>;
    fn validate(
        &self,
        owner_id: &str,
        value: &serde_json::Value,
        base: Option<&serde_json::Value>,
        facts: &CandidateFacts<'_>,
    ) -> Result<(), ConfigFault>;
    fn apply_run_options(
        &self,
        owner_id: &str,
        recorded: &serde_json::Value,
        options: &serde_json::Value,
    ) -> Result<serde_json::Value, ConfigFault>;
}

/// Where the namespace a command reduces came from, which decides what an
/// unreadable one means.
#[derive(Clone, Copy)]
enum ReducedFrom {
    /// The session's recorded namespace: unreadable is corruption.
    Recorded,
    /// An earlier command's output in the same transaction: unreadable is
    /// the owner's own candidate.
    Candidate,
}

trait ErasedCommand: Send + Sync {
    fn decode(&self, args: &serde_json::Value) -> Result<(), String>;
    fn reduce(
        &self,
        recorded: &serde_json::Value,
        from: ReducedFrom,
        args: &serde_json::Value,
        models: &dyn crate::LlmProfiles,
    ) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure>;
    fn input_schema(&self) -> serde_json::Value;
    fn output_schema(&self) -> serde_json::Value;
}

/// Why a command did not reduce: a reason to refuse it, or a recorded
/// namespace that does not read as its owner's type.
enum ConfigCommandFailure {
    Refused(ConfigRefusalReason),
    RecordedCorrupt(String),
}

struct TypedOwner<O>(O);

fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null)
}

fn unreadable(role: ConfigValueRole, error: impl std::fmt::Display) -> ConfigRefusalReason {
    ConfigRefusalReason::Unreadable {
        role,
        message: error.to_string(),
    }
}

fn format_fault(refusal: super::FormatRefusal, at: RefusalSite) -> ConfigFault {
    let owner = refusal.plugin.clone();
    refused(&owner, at, ConfigRefusalReason::Format { refusal })
}

fn refused(owner_id: &str, at: RefusalSite, reason: ConfigRefusalReason) -> ConfigFault {
    ConfigFault::Refused(ConfigRefusal {
        owner: owner_id.to_string(),
        at,
        reason,
    })
}

fn corrupt(owner_id: &str, error: impl std::fmt::Display) -> RecordedNamespaceCorrupt {
    RecordedNamespaceCorrupt {
        owner: owner_id.to_string(),
        message: error.to_string(),
    }
}

impl<O: ConfigOwner> ErasedOwner for TypedOwner<O> {
    fn refusal_schema(&self) -> serde_json::Value {
        schema_of::<O::Refusal>()
    }

    fn create(
        &self,
        owner_id: &str,
        input: Option<&serde_json::Value>,
        parent: Option<&serde_json::Value>,
        is_root_session: bool,
    ) -> Result<Option<serde_json::Value>, ConfigFault> {
        let at = || RefusalSite::Creation;
        let input = input
            .map(|input| serde_json::from_value::<O::Create>(input.clone()))
            .transpose()
            .map_err(|error| {
                refused(
                    owner_id,
                    at(),
                    unreadable(ConfigValueRole::CreationInput, error),
                )
            })?;
        let parent = parent
            .map(|parent| serde_json::from_value::<O::Recorded>(parent.clone()))
            .transpose()
            .map_err(|error| corrupt(owner_id, error))?;
        let created = self
            .0
            .create(
                input,
                CreationFacts {
                    parent: parent.as_ref(),
                    is_root_session,
                },
            )
            .map_err(|refusal| ConfigRefusal::by_owner(owner_id, at(), &refusal))?;
        created
            .map(|recorded| {
                serde_json::to_value(recorded).map_err(|error| {
                    refused(
                        owner_id,
                        at(),
                        unreadable(ConfigValueRole::Candidate, error),
                    )
                })
            })
            .transpose()
    }

    fn validate(
        &self,
        owner_id: &str,
        value: &serde_json::Value,
        base: Option<&serde_json::Value>,
        facts: &CandidateFacts<'_>,
    ) -> Result<(), ConfigFault> {
        // The base first: a namespace the candidate left untouched is the
        // recorded one, and unreadable it is corruption, not a candidate.
        let base = base
            .map(|base| serde_json::from_value::<O::Recorded>(base.clone()))
            .transpose()
            .map_err(|error| corrupt(owner_id, error))?;
        let value = serde_json::from_value::<O::Recorded>(value.clone()).map_err(|error| {
            refused(
                owner_id,
                RefusalSite::Candidate,
                unreadable(ConfigValueRole::Candidate, error),
            )
        })?;
        self.0
            .validate(&value, base.as_ref(), facts)
            .map_err(|refusal| {
                ConfigRefusal::by_owner(owner_id, RefusalSite::Candidate, &refusal).into()
            })
    }

    fn apply_run_options(
        &self,
        owner_id: &str,
        recorded: &serde_json::Value,
        options: &serde_json::Value,
    ) -> Result<serde_json::Value, ConfigFault> {
        let at = || RefusalSite::Candidate;
        let recorded = serde_json::from_value::<O::Recorded>(recorded.clone())
            .map_err(|error| corrupt(owner_id, error))?;
        let options =
            serde_json::from_value::<O::RunOptions>(options.clone()).map_err(|error| {
                refused(
                    owner_id,
                    at(),
                    unreadable(ConfigValueRole::RunOptions, error),
                )
            })?;
        let applied = self
            .0
            .apply_run_options(&recorded, options)
            .map_err(|refusal| ConfigRefusal::by_owner(owner_id, at(), &refusal))?;
        serde_json::to_value(applied).map_err(|error| {
            refused(
                owner_id,
                at(),
                unreadable(ConfigValueRole::Candidate, error),
            )
        })
    }
}

struct TypedCommand<C, F> {
    reduce: F,
    _command: std::marker::PhantomData<fn() -> C>,
}

impl<C, F> ErasedCommand for TypedCommand<C, F>
where
    C: ConfigCommand,
    F: Fn(&RecordedOf<C>, C) -> Result<OwnerChange<RecordedOf<C>, C::Output>, RefusalOf<C>>
        + Send
        + Sync
        + 'static,
{
    fn decode(&self, args: &serde_json::Value) -> Result<(), String> {
        serde_json::from_value::<C>(args.clone())
            .map(drop)
            .map_err(|error| error.to_string())
    }

    fn reduce(
        &self,
        recorded: &serde_json::Value,
        from: ReducedFrom,
        args: &serde_json::Value,
        _llm_profiles: &dyn crate::LlmProfiles,
    ) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure> {
        reduce_typed::<C>(recorded, from, args, |recorded, command| {
            (self.reduce)(recorded, command)
        })
    }

    fn input_schema(&self) -> serde_json::Value {
        schema_of::<C>()
    }

    fn output_schema(&self) -> serde_json::Value {
        schema_of::<C::Output>()
    }
}

/// A core command whose reducer also reads the host's models: the one place
/// a reducer mints a model binding.
struct LlmProfilesCommand<C, F> {
    reduce: F,
    _command: std::marker::PhantomData<fn() -> C>,
}

impl<C, F> ErasedCommand for LlmProfilesCommand<C, F>
where
    C: ConfigCommand,
    F: Fn(
            &RecordedOf<C>,
            C,
            &dyn crate::LlmProfiles,
        ) -> Result<OwnerChange<RecordedOf<C>, C::Output>, RefusalOf<C>>
        + Send
        + Sync
        + 'static,
{
    fn decode(&self, args: &serde_json::Value) -> Result<(), String> {
        serde_json::from_value::<C>(args.clone())
            .map(drop)
            .map_err(|error| error.to_string())
    }

    fn reduce(
        &self,
        recorded: &serde_json::Value,
        from: ReducedFrom,
        args: &serde_json::Value,
        models: &dyn crate::LlmProfiles,
    ) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure> {
        reduce_typed::<C>(recorded, from, args, |recorded, command| {
            (self.reduce)(recorded, command, models)
        })
    }

    fn input_schema(&self) -> serde_json::Value {
        schema_of::<C>()
    }

    fn output_schema(&self) -> serde_json::Value {
        schema_of::<C::Output>()
    }
}

/// Decode `recorded` and `args` as `C`'s namespace and command, reduce them
/// with `reduce`, and encode the change.
fn reduce_typed<C: ConfigCommand>(
    recorded: &serde_json::Value,
    from: ReducedFrom,
    args: &serde_json::Value,
    reduce: impl FnOnce(
        &RecordedOf<C>,
        C,
    ) -> Result<OwnerChange<RecordedOf<C>, C::Output>, RefusalOf<C>>,
) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure> {
    let refuse =
        |role, error: serde_json::Error| ConfigCommandFailure::Refused(unreadable(role, error));
    let recorded =
        serde_json::from_value::<RecordedOf<C>>(recorded.clone()).map_err(|error| match from {
            ReducedFrom::Recorded => ConfigCommandFailure::RecordedCorrupt(error.to_string()),
            ReducedFrom::Candidate => refuse(ConfigValueRole::Candidate, error),
        })?;
    let command = serde_json::from_value::<C>(args.clone())
        .map_err(|error| refuse(ConfigValueRole::Arguments, error))?;
    let change = reduce(&recorded, command).map_err(|refusal| {
        ConfigCommandFailure::Refused(ConfigRefusalReason::by_owner(&refusal))
    })?;
    let next = serde_json::to_value(change.recorded)
        .map_err(|error| refuse(ConfigValueRole::Candidate, error))?;
    let output = serde_json::to_value(change.output)
        .map_err(|error| refuse(ConfigValueRole::Output, error))?;
    Ok((next, output))
}

/// Why a session's creation recorded no plugin config: an owner refused
/// what the creator stated, the parent's recorded config is corrupt, or
/// this deployment's config registrations cannot stand.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CreationConfigError {
    #[error(transparent)]
    Format(#[from] super::FormatRefusal),
    #[error(transparent)]
    Refused(#[from] ConfigRefusal),
    #[error(transparent)]
    RecordedCorrupt(#[from] RecordedNamespaceCorrupt),
    #[error("config registration is invalid: {0}")]
    Registration(#[from] ConfigRegistrationError),
}

impl From<ConfigFault> for CreationConfigError {
    fn from(fault: ConfigFault) -> Self {
        match fault {
            ConfigFault::Refused(refusal) => Self::Refused(refusal),
            ConfigFault::RecordedCorrupt(corrupt) => Self::RecordedCorrupt(corrupt),
        }
    }
}

/// Why a config transaction was not admitted at ingress: nothing was
/// enqueued.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigSubmitError {
    #[error("a config transaction names at least one command")]
    Empty,
    #[error("no config owner `{owner}` is registered")]
    UnknownOwner { owner: String },
    #[error("config owner `{owner}` registers no command `{command}`")]
    UnknownCommand { owner: String, command: String },
    #[error("config command `{owner}.{command}` does not accept its arguments: {detail}")]
    InvalidArgs {
        owner: String,
        command: String,
        detail: String,
    },
    #[error(
        "config transaction `{id}` was already submitted with different content; a changed \
         transaction takes a new id"
    )]
    ChangedContent { id: String },
    #[error("config registration is invalid: {0}")]
    Registration(ConfigRegistrationError),
}

/// An ordered config transaction a host builds: typed commands, each carried
/// to the owner that registered its type, and named entries a remote caller
/// supplies with their owner.
#[derive(Clone, Debug, Default)]
pub struct ConfigTransaction {
    steps: Vec<ConfigTransactionStep>,
}

#[derive(Clone, Debug)]
enum ConfigTransactionStep {
    Typed {
        command_type: TypeId,
        owner_type: &'static str,
        command: &'static str,
        args: Result<serde_json::Value, String>,
    },
    Named(ConfigCommandEntry),
}

impl ConfigTransaction {
    pub fn new() -> Self {
        Self::default()
    }

    /// A transaction of the one command `command`.
    pub fn of<C: ConfigCommand>(command: C) -> Self {
        Self::new().then(command)
    }

    /// Append the typed command `command`.
    #[must_use]
    pub fn then<C: ConfigCommand>(mut self, command: C) -> Self {
        self.steps.push(ConfigTransactionStep::Typed {
            command_type: TypeId::of::<C>(),
            owner_type: std::any::type_name::<C::Owner>(),
            command: C::NAME,
            args: serde_json::to_value(&command).map_err(|error| error.to_string()),
        });
        self
    }

    /// Append a command by its owner, name and arguments, as a remote caller
    /// names it.
    #[must_use]
    pub fn then_entry(mut self, entry: ConfigCommandEntry) -> Self {
        self.steps.push(ConfigTransactionStep::Named(entry));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Every config registration of an installed plugin set, and the core
/// owner's.
#[derive(Clone)]
pub struct ConfigRegistry {
    owners: BTreeMap<String, RegisteredOwner>,
    factories: Vec<Arc<dyn PluginFactory>>,
}

impl std::fmt::Debug for ConfigRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigRegistry")
            .field("owners", &self.owners.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ConfigRegistry {
    /// Collect every factory's config registration, and the core owner's.
    pub fn build(factories: &[Arc<dyn PluginFactory>]) -> Result<Self, ConfigRegistrationError> {
        let mut owners = BTreeMap::new();
        owners.insert(CORE_CONFIG_OWNER.to_string(), core::registration()?);
        for factory in factories {
            let plugin_id = factory.id();
            if plugin_id == CORE_CONFIG_OWNER {
                return Err(ConfigRegistrationError::ReservedOwner {
                    owner: plugin_id.to_string(),
                });
            }
            let mut registrar = ConfigRegistrar::new(plugin_id);
            factory.register_config(&mut registrar)?;
            if let Some(owner) = registrar.owner {
                owners.insert(plugin_id.to_string(), owner);
            }
        }
        Ok(Self {
            owners,
            factories: factories.to_vec(),
        })
    }

    /// The recorded plugin configuration of a session being created: every
    /// registered plugin owner creates its namespace — the stated value, its
    /// defaults otherwise — and `protocol_plugin_id` names the protocol
    /// owner. A stated namespace no owner registers is refused typed, the
    /// first in key order. A parent's recorded namespace its owner cannot
    /// read is corruption.
    ///
    /// Each namespace is written in the format `writers` records for its
    /// plugin (FIG-4747), and in the plugin's native format when the
    /// admission does not name it.
    pub fn resolve_creation(
        &self,
        protocol_plugin_id: Option<&str>,
        requested: &PluginOptions,
        parent: Option<&PluginConfig>,
        is_root_session: bool,
        writers: &crate::store::plugin_writers::PluginAdmission,
    ) -> Result<PluginConfig, ConfigFault> {
        if let Some(unknown) = requested.plugins.keys().find(|plugin_id| {
            plugin_id.as_str() == CORE_CONFIG_OWNER || !self.owners.contains_key(plugin_id.as_str())
        }) {
            return Err(refused(
                unknown,
                RefusalSite::Creation,
                ConfigRefusalReason::UnknownOwner,
            ));
        }
        let requested_config = PluginConfig::from_recorded_parts(None, requested.plugins.clone());
        let requested = super::formats::decode_config_for(&self.factories, &requested_config)
            .map_err(|refusal| format_fault(refusal, RefusalSite::Creation))?;
        let parent = parent
            .map(|parent| super::formats::decode_config_for(&self.factories, parent))
            .transpose()
            .map_err(|refusal| format_fault(refusal, RefusalSite::Creation))?;
        let mut config = PluginConfig::for_protocol(protocol_plugin_id.map(str::to_string));
        for (plugin_id, registered) in &self.owners {
            if plugin_id == CORE_CONFIG_OWNER {
                continue;
            }
            let created = registered.owner.create(
                plugin_id,
                requested.get(plugin_id),
                parent.as_ref().and_then(|parent| parent.get(plugin_id)),
                is_root_session,
            )?;
            if let Some(value) = created {
                let Some(factory) = self
                    .factories
                    .iter()
                    .find(|factory| factory.id() == plugin_id)
                else {
                    return Err(refused(
                        plugin_id,
                        RefusalSite::Creation,
                        ConfigRefusalReason::UnknownOwner,
                    ));
                };
                let writer = writers
                    .writer(plugin_id)
                    .unwrap_or_else(|| factory.plugin_declaration().format_version);
                let encoded = factory
                    .encode_format(writer, super::FormatNamespace::Config, &value)
                    .map_err(|refusal| format_fault(refusal, RefusalSite::Creation))?;
                config.insert_versioned(plugin_id.clone(), writer, encoded);
            }
        }
        Ok(config)
    }

    /// Admit a transaction at ingress: every command names a registered
    /// owner and command and its arguments decode. The record carries the
    /// submitter's request and nothing of this build.
    pub fn admit(
        &self,
        id: impl Into<String>,
        expected_revision: u64,
        entries: Vec<ConfigCommandEntry>,
    ) -> Result<ConfigTransactionRecord, ConfigSubmitError> {
        if entries.is_empty() {
            return Err(ConfigSubmitError::Empty);
        }
        for entry in &entries {
            let command = self.command(&entry.owner, &entry.command)?;
            command
                .decode(&entry.args)
                .map_err(|detail| ConfigSubmitError::InvalidArgs {
                    owner: entry.owner.clone(),
                    command: entry.command.clone(),
                    detail,
                })?;
        }
        Ok(ConfigTransactionRecord {
            id: id.into(),
            expected_revision,
            entries,
        })
    }

    fn command(
        &self,
        owner: &str,
        command: &str,
    ) -> Result<&Arc<dyn ErasedCommand>, ConfigSubmitError> {
        let registered = self
            .owners
            .get(owner)
            .ok_or_else(|| ConfigSubmitError::UnknownOwner {
                owner: owner.to_string(),
            })?;
        registered
            .commands
            .get(command)
            .ok_or_else(|| ConfigSubmitError::UnknownCommand {
                owner: owner.to_string(),
                command: command.to_string(),
            })
    }

    /// The entries `transaction` carries: each typed command addressed to
    /// the owner that registered its type, never to a name the caller
    /// supplies.
    pub fn entries(
        &self,
        transaction: &ConfigTransaction,
    ) -> Result<Vec<ConfigCommandEntry>, ConfigSubmitError> {
        transaction
            .steps
            .iter()
            .map(|step| match step {
                ConfigTransactionStep::Named(entry) => Ok(entry.clone()),
                ConfigTransactionStep::Typed {
                    command_type,
                    owner_type,
                    command,
                    args,
                } => {
                    let owner = self
                        .owners
                        .iter()
                        .find(|(_, registered)| registered.command_types.contains_key(command_type))
                        .map(|(owner, _)| owner.clone())
                        .ok_or_else(|| ConfigSubmitError::UnknownCommand {
                            owner: (*owner_type).to_string(),
                            command: (*command).to_string(),
                        })?;
                    let args = args
                        .clone()
                        .map_err(|detail| ConfigSubmitError::InvalidArgs {
                            owner: owner.clone(),
                            command: (*command).to_string(),
                            detail,
                        })?;
                    Ok(ConfigCommandEntry {
                        owner,
                        command: (*command).to_string(),
                        args,
                    })
                }
            })
            .collect()
    }

    /// Resolve `transaction` over `base`, the session's config at the
    /// boundary that applies it.
    ///
    /// A base at another revision than the one the transaction was written
    /// against resolves `Stale` without running a reducer. Otherwise the
    /// commands reduce in order over one private candidate, and every owner
    /// with a recorded namespace validates the final candidate, touched or
    /// not, and the core's model and reasoning when the core changed; the
    /// first refusal refuses the whole transaction. A model command mints
    /// its key's binding through `models` here, once.
    /// Nothing here publishes: the caller records the resolution, then
    /// publishes it.
    ///
    /// A recorded namespace its owner cannot read resolves nothing: it is
    /// corruption of the session's config, never the transaction's refusal.
    ///
    /// A namespace the transaction changes is written in the format
    /// `writers` records for its plugin (FIG-4747): the resolution is the
    /// recorded step, so a replay publishes the same formats.
    pub fn resolve(
        &self,
        base: &crate::PersistedSessionConfig,
        transaction: &ConfigTransactionRecord,
        models: &dyn crate::LlmProfiles,
        writers: &crate::store::plugin_writers::PluginAdmission,
        prompts: &super::prompt::PromptCatalog,
    ) -> Result<ConfigResolution, RecordedNamespaceCorrupt> {
        let base_revision = base.config_revision;
        let result = if transaction.expected_revision == base_revision {
            match self.reduce(base, transaction, models, writers, prompts) {
                Ok(applied) => applied,
                Err(ConfigFault::Refused(refusal)) => ConfigResolutionDecision::Refused { refusal },
                Err(ConfigFault::RecordedCorrupt(corrupt)) => return Err(corrupt),
            }
        } else {
            ConfigResolutionDecision::Stale {
                expected: transaction.expected_revision,
                actual: base_revision,
            }
        };
        Ok(ConfigResolution {
            base_revision,
            result,
        })
    }

    /// The applied decision of `transaction` over `base`, or why it has
    /// none.
    fn reduce(
        &self,
        base: &crate::PersistedSessionConfig,
        transaction: &ConfigTransactionRecord,
        models: &dyn crate::LlmProfiles,
        writers: &crate::store::plugin_writers::PluginAdmission,
        prompts: &super::prompt::PromptCatalog,
    ) -> Result<ConfigResolutionDecision, ConfigFault> {
        let mut decoded_base = base.clone();
        decoded_base.plugin_config =
            super::formats::decode_config_for(&self.factories, &base.plugin_config)
                .map_err(|refusal| format_fault(refusal, RefusalSite::Candidate))?;
        let original_base = base;
        let base = &decoded_base;
        let base_core = CoreConfig::of(base);
        let mut candidate: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut outputs = Vec::with_capacity(transaction.entries.len());
        for (index, entry) in transaction.entries.iter().enumerate() {
            let refuse = |reason: ConfigRefusalReason| {
                refused(
                    &entry.owner,
                    RefusalSite::Command {
                        index,
                        command: entry.command.clone(),
                    },
                    reason,
                )
            };
            let registered = self
                .owners
                .get(&entry.owner)
                .ok_or_else(|| refuse(ConfigRefusalReason::UnknownOwner))?;
            let command = registered
                .commands
                .get(&entry.command)
                .ok_or_else(|| refuse(ConfigRefusalReason::UnknownCommand))?;
            let (recorded, from) = match candidate.get(&entry.owner) {
                Some(value) => (value.clone(), ReducedFrom::Candidate),
                None if entry.owner == CORE_CONFIG_OWNER => (
                    // The core share is a view built here, not a stored
                    // namespace: one that does not encode is this
                    // candidate's fault.
                    serde_json::to_value(&base_core)
                        .map_err(|error| refuse(unreadable(ConfigValueRole::Candidate, error)))?,
                    ReducedFrom::Candidate,
                ),
                None => (
                    base.plugin_config
                        .get(&entry.owner)
                        .cloned()
                        .ok_or_else(|| refuse(ConfigRefusalReason::UnrecordedNamespace))?,
                    ReducedFrom::Recorded,
                ),
            };
            match command.reduce(&recorded, from, &entry.args, models) {
                Ok((next, output)) => {
                    if entry.owner == CORE_CONFIG_OWNER
                        && entry.command == core::SetPromptPlan::NAME
                    {
                        let core = serde_json::from_value::<CoreConfig>(next.clone()).map_err(
                            |error| refuse(unreadable(ConfigValueRole::Candidate, error)),
                        )?;
                        prompts.validate_plan(&core.prompt_plan).map_err(|error| {
                            refuse(ConfigRefusalReason::by_owner(
                                &CoreConfigRefusal::PromptPlanRefused { error },
                            ))
                        })?;
                    }
                    candidate.insert(entry.owner.clone(), next);
                    outputs.push(output);
                }
                Err(ConfigCommandFailure::Refused(reason)) => return Err(refuse(reason)),
                Err(ConfigCommandFailure::RecordedCorrupt(message)) => {
                    return Err(corrupt(&entry.owner, message).into());
                }
            }
        }
        let core = candidate
            .remove(CORE_CONFIG_OWNER)
            .map(serde_json::from_value::<CoreConfig>)
            .transpose()
            .map_err(|error| {
                refused(
                    CORE_CONFIG_OWNER,
                    RefusalSite::Candidate,
                    unreadable(ConfigValueRole::Candidate, error),
                )
            })?;
        let namespaces = candidate;
        let final_core = core.clone().unwrap_or_else(|| base_core.clone());
        let mut final_plugins = base.plugin_config.clone();
        for (id, value) in &namespaces {
            final_plugins.insert(id.clone(), value.clone());
        }
        let facts = CandidateFacts {
            core: &final_core,
            plugin_config: &final_plugins,
        };
        if core.is_some() {
            core::validate_candidate(&base_core, &final_core).map_err(|refusal| {
                ConfigRefusal::by_owner(CORE_CONFIG_OWNER, RefusalSite::Candidate, &refusal)
            })?;
        }
        // Every owner judges the final candidate, touched or not: a change
        // to one namespace, the core's included, can break another owner's
        // invariant.
        for (owner, value) in final_plugins.iter() {
            let Some(registered) = self.owners.get(owner.as_str()) else {
                continue;
            };
            registered
                .owner
                .validate(owner, value, base.plugin_config.get(owner), &facts)?;
        }
        let mut encoded_namespaces = BTreeMap::new();
        for factory in &self.factories {
            let id = factory.id();
            let Some(value) = final_plugins.get(id) else {
                continue;
            };
            if !namespaces.contains_key(id)
                && original_base.plugin_config.namespace(id) == final_plugins.namespace(id)
            {
                continue;
            }
            let writer = writers
                .writer(id)
                .unwrap_or_else(|| factory.plugin_declaration().format_version);
            let value = factory
                .encode_format(writer, super::FormatNamespace::Config, value)
                .map_err(|refusal| format_fault(refusal, RefusalSite::Candidate))?;
            encoded_namespaces.insert(
                id.into(),
                super::PluginConfigNamespace {
                    format_version: writer,
                    value,
                },
            );
        }
        Ok(ConfigResolutionDecision::Applied {
            core: core.map(Box::new),
            namespaces: encoded_namespaces,
            outputs,
        })
    }

    /// The protocol namespace a run executes under: the one `config`
    /// recorded with the run's stated `options` applied by the protocol's
    /// owner, as its typed run options. Nothing else lays a run's options
    /// over a namespace.
    pub fn apply_run_options(
        &self,
        config: &PluginConfig,
        protocol: &str,
        options: &crate::ProtocolTurnOptions,
    ) -> Result<serde_json::Value, ConfigFault> {
        let config = super::formats::decode_config_for(&self.factories, config)
            .map_err(|refusal| format_fault(refusal, RefusalSite::Candidate))?;
        let refuse = |reason| refused(protocol, RefusalSite::Candidate, reason);
        let registered = self
            .owners
            .get(protocol)
            .ok_or_else(|| refuse(ConfigRefusalReason::UnknownOwner))?;
        let recorded = config
            .get(protocol)
            .ok_or_else(|| refuse(ConfigRefusalReason::UnrecordedNamespace))?;
        registered
            .owner
            .apply_run_options(protocol, recorded, &options.payload)
    }

    /// Validate a run's config that a run override derived from the
    /// session's `base`: every namespace the override changed is judged by
    /// its owner against its recorded value, so an overlay cannot set what
    /// the owner does not admit.
    pub fn validate_derived(
        &self,
        base: &crate::PersistedSessionConfig,
        derived: &crate::PersistedSessionConfig,
    ) -> Result<(), ConfigFault> {
        let mut native_base = base.clone();
        native_base.plugin_config =
            super::formats::decode_config_for(&self.factories, &base.plugin_config)
                .map_err(|refusal| format_fault(refusal, RefusalSite::Candidate))?;
        let mut native_derived = derived.clone();
        native_derived.plugin_config =
            super::formats::decode_config_for(&self.factories, &derived.plugin_config)
                .map_err(|refusal| format_fault(refusal, RefusalSite::Candidate))?;
        let base = &native_base;
        let derived = &native_derived;
        let core = CoreConfig::of(derived);
        let facts = CandidateFacts {
            core: &core,
            plugin_config: &derived.plugin_config,
        };
        for (owner, value) in derived.plugin_config.iter() {
            let recorded = base.plugin_config.get(owner);
            if recorded == Some(value) {
                continue;
            }
            let Some(registered) = self.owners.get(owner.as_str()) else {
                continue;
            };
            registered.owner.validate(owner, value, recorded, &facts)?;
        }
        Ok(())
    }

    /// The catalog of every registered config command, describing the
    /// config at `revision`.
    pub fn catalog(&self, revision: u64) -> ConfigCommandCatalog {
        let commands = self
            .owners
            .iter()
            .flat_map(|(owner, registered)| {
                registered
                    .commands
                    .iter()
                    .map(move |(name, command)| ConfigCommandDescriptor {
                        owner: owner.clone(),
                        command: name.clone(),
                        input_schema: command.input_schema(),
                        output_schema: command.output_schema(),
                        refusal_schema: registered.owner.refusal_schema(),
                    })
            })
            .collect();
        ConfigCommandCatalog { revision, commands }
    }
}

impl crate::RunOptionsOwner for ConfigRegistry {
    fn apply_run_options(
        &self,
        config: &PluginConfig,
        protocol: &str,
        options: &crate::ProtocolTurnOptions,
    ) -> Result<serde_json::Value, ConfigFault> {
        Self::apply_run_options(self, config, protocol, options)
    }
}

#[cfg(test)]
mod tests;
