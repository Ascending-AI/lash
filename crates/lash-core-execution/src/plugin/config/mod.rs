//! Recorded session config and the typed commands that change it (FIG-4379).
//!
//! A session records every installed owner's config namespace with its
//! config head, the protocol's among them, beside the core owner's share
//! (model, reasoning, attachment acceptance, generation, budget and
//! tool access). Each
//! namespace has one owner:
//!
//! - An owner registers before any session exists
//!   ([`PluginFactory::register_config`](super::PluginFactory::register_config)):
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
//!
//! The [`ConfigRegistry`] holds every registration: the one list the
//! resolver, the ingress check and the [catalog](ConfigRegistry::catalog)
//! are generated from.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use self::core::{CORE_CONFIG_IMPLEMENTATION, CoreConfigOwner, CoreConfigRefusal};
pub use lash_core_store::config_transaction::{
    CORE_CONFIG_OWNER, ConfigCommandEntry, ConfigRefusal, ConfigResolution,
    ConfigResolutionDecision, ConfigTransactionOutcome, ConfigTransactionRecord, CoreConfig,
};
pub use lash_core_store::execution_state::{AdmittedPluginConfig, PluginConfig};

use super::{PluginFactory, PluginOptions};
use crate::SessionConfigRefusal;

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

    /// The identity of this owner's reducers. A config transaction records
    /// the identity each named owner ran at ingress; a drain that runs a
    /// different identity resolves nothing and waits for a build that runs
    /// the recorded one.
    fn implementation(&self) -> &str;

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

    /// Judge the raw options a run states for this owner's namespace, before
    /// they are laid over it. A protocol's run options are their own typed
    /// shape, narrower than its recorded namespace: what only creation or a
    /// config command may set (its prompt config, FIG-4589) is refused here,
    /// whatever value the run states for it. The default admits everything.
    fn validate_run_options(&self, _options: &serde_json::Value) -> Result<(), Self::Refusal> {
        Ok(())
    }
}

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
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
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
    fn models_command<C: ConfigCommand>(
        &mut self,
        reduce: impl Fn(
            &RecordedOf<C>,
            C,
            &dyn crate::RuntimeModels,
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
            Arc::new(ModelsCommand::<C, _> {
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
    fn implementation(&self) -> String;
    fn refusal_schema(&self) -> serde_json::Value;
    fn create(
        &self,
        owner_id: &str,
        input: Option<&serde_json::Value>,
        parent: Option<&serde_json::Value>,
        is_root_session: bool,
    ) -> Result<Option<serde_json::Value>, ConfigRefusal>;
    fn validate(
        &self,
        owner_id: &str,
        value: &serde_json::Value,
        base: Option<&serde_json::Value>,
        facts: &CandidateFacts<'_>,
    ) -> Result<(), ConfigRefusal>;
    fn validate_run_options(
        &self,
        owner_id: &str,
        options: &serde_json::Value,
    ) -> Result<(), ConfigRefusal>;
}

trait ErasedCommand: Send + Sync {
    fn decode(&self, args: &serde_json::Value) -> Result<(), String>;
    fn reduce(
        &self,
        recorded: &serde_json::Value,
        args: &serde_json::Value,
        models: &dyn crate::RuntimeModels,
    ) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure>;
    fn input_schema(&self) -> serde_json::Value;
    fn output_schema(&self) -> serde_json::Value;
}

/// Why a command did not reduce: its owner refused, as data, or its
/// recorded namespace or arguments could not be read.
enum ConfigCommandFailure {
    Refused {
        refusal: serde_json::Value,
        message: String,
    },
    Unreadable(String),
}

struct TypedOwner<O>(O);

fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null)
}

fn owner_refusal<R: Serialize + std::fmt::Display>(owner_id: &str, refusal: &R) -> ConfigRefusal {
    ConfigRefusal {
        index: None,
        owner: owner_id.to_string(),
        command: None,
        refusal: serde_json::to_value(refusal).unwrap_or(serde_json::Value::Null),
        message: refusal.to_string(),
    }
}

fn unreadable(owner_id: &str, message: String) -> ConfigRefusal {
    ConfigRefusal {
        index: None,
        owner: owner_id.to_string(),
        command: None,
        refusal: serde_json::json!({ "unreadable": message }),
        message,
    }
}

impl<O: ConfigOwner> ErasedOwner for TypedOwner<O> {
    fn implementation(&self) -> String {
        self.0.implementation().to_string()
    }

    fn refusal_schema(&self) -> serde_json::Value {
        schema_of::<O::Refusal>()
    }

    fn create(
        &self,
        owner_id: &str,
        input: Option<&serde_json::Value>,
        parent: Option<&serde_json::Value>,
        is_root_session: bool,
    ) -> Result<Option<serde_json::Value>, ConfigRefusal> {
        let input = input
            .map(|input| serde_json::from_value::<O::Create>(input.clone()))
            .transpose()
            .map_err(|error| unreadable(owner_id, format!("invalid creation config: {error}")))?;
        let parent = parent
            .map(|parent| serde_json::from_value::<O::Recorded>(parent.clone()))
            .transpose()
            .map_err(|error| {
                unreadable(
                    owner_id,
                    format!("the parent's recorded config is unreadable: {error}"),
                )
            })?;
        let created = self
            .0
            .create(
                input,
                CreationFacts {
                    parent: parent.as_ref(),
                    is_root_session,
                },
            )
            .map_err(|refusal| owner_refusal(owner_id, &refusal))?;
        created
            .map(|recorded| {
                serde_json::to_value(recorded).map_err(|error| {
                    unreadable(
                        owner_id,
                        format!("the created config does not encode: {error}"),
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
    ) -> Result<(), ConfigRefusal> {
        let value = serde_json::from_value::<O::Recorded>(value.clone()).map_err(|error| {
            unreadable(
                owner_id,
                format!("the candidate config is unreadable: {error}"),
            )
        })?;
        let base = base
            .map(|base| serde_json::from_value::<O::Recorded>(base.clone()))
            .transpose()
            .map_err(|error| {
                unreadable(
                    owner_id,
                    format!("the recorded config is unreadable: {error}"),
                )
            })?;
        self.0
            .validate(&value, base.as_ref(), facts)
            .map_err(|refusal| owner_refusal(owner_id, &refusal))
    }

    fn validate_run_options(
        &self,
        owner_id: &str,
        options: &serde_json::Value,
    ) -> Result<(), ConfigRefusal> {
        self.0
            .validate_run_options(options)
            .map_err(|refusal| owner_refusal(owner_id, &refusal))
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
        args: &serde_json::Value,
        _models: &dyn crate::RuntimeModels,
    ) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure> {
        reduce_typed::<C>(recorded, args, |recorded, command| {
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
struct ModelsCommand<C, F> {
    reduce: F,
    _command: std::marker::PhantomData<fn() -> C>,
}

impl<C, F> ErasedCommand for ModelsCommand<C, F>
where
    C: ConfigCommand,
    F: Fn(
            &RecordedOf<C>,
            C,
            &dyn crate::RuntimeModels,
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
        args: &serde_json::Value,
        models: &dyn crate::RuntimeModels,
    ) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure> {
        reduce_typed::<C>(recorded, args, |recorded, command| {
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
    args: &serde_json::Value,
    reduce: impl FnOnce(
        &RecordedOf<C>,
        C,
    ) -> Result<OwnerChange<RecordedOf<C>, C::Output>, RefusalOf<C>>,
) -> Result<(serde_json::Value, serde_json::Value), ConfigCommandFailure> {
    let recorded = serde_json::from_value::<RecordedOf<C>>(recorded.clone())
        .map_err(|error| ConfigCommandFailure::Unreadable(format!("recorded config: {error}")))?;
    let command = serde_json::from_value::<C>(args.clone())
        .map_err(|error| ConfigCommandFailure::Unreadable(format!("arguments: {error}")))?;
    let change = reduce(&recorded, command).map_err(|refusal| ConfigCommandFailure::Refused {
        refusal: serde_json::to_value(&refusal).unwrap_or(serde_json::Value::Null),
        message: refusal.to_string(),
    })?;
    let next = serde_json::to_value(change.recorded).map_err(|error| {
        ConfigCommandFailure::Unreadable(format!("next config does not encode: {error}"))
    })?;
    let output = serde_json::to_value(change.output).map_err(|error| {
        ConfigCommandFailure::Unreadable(format!("output does not encode: {error}"))
    })?;
    Ok((next, output))
}

/// A creation or a transaction named config owners no installed plugin
/// registers, so recording them would keep facts nobody validates and
/// nobody reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no installed plugin owns session config for {}", plugin_ids.join(", "))]
pub struct UnknownPluginConfigOwner {
    pub plugin_ids: Vec<String>,
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

/// A recorded transaction's owners run reducers other than the ones it was
/// admitted under: resolving it here would decide it with code it was not
/// admitted to run, so it waits for a build that runs the recorded ones.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "config owner `{owner}` runs reducer implementation {current:?}, but the transaction was \
     admitted under {recorded:?}"
)]
pub struct ConfigImplementationMismatch {
    pub owner: String,
    pub recorded: String,
    pub current: Option<String>,
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
        Ok(Self { owners })
    }

    /// The recorded plugin configuration of a session being created: every
    /// registered plugin owner creates its namespace — the stated value, its
    /// defaults otherwise — and `protocol_plugin_id` names the protocol
    /// owner. A stated namespace no owner registers is refused typed.
    pub fn resolve_creation(
        &self,
        protocol_plugin_id: Option<&str>,
        requested: &PluginOptions,
        parent: Option<&PluginConfig>,
        is_root_session: bool,
    ) -> Result<PluginConfig, SessionConfigRefusal> {
        let unknown: Vec<String> = requested
            .plugins
            .keys()
            .filter(|plugin_id| {
                plugin_id.as_str() == CORE_CONFIG_OWNER
                    || !self.owners.contains_key(plugin_id.as_str())
            })
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(SessionConfigRefusal::new(UnknownPluginConfigOwner {
                plugin_ids: unknown,
            }));
        }
        let mut config = PluginConfig::for_protocol(protocol_plugin_id.map(str::to_string));
        for (plugin_id, registered) in &self.owners {
            if plugin_id == CORE_CONFIG_OWNER {
                continue;
            }
            let created = registered
                .owner
                .create(
                    plugin_id,
                    requested.plugins.get(plugin_id),
                    parent.and_then(|parent| parent.get(plugin_id)),
                    is_root_session,
                )
                .map_err(SessionConfigRefusal::new)?;
            if let Some(value) = created {
                config.insert(plugin_id.clone(), value);
            }
        }
        Ok(config)
    }

    /// Admit a transaction at ingress: every command names a registered
    /// owner and command and its arguments decode. The record carries each
    /// named owner's reducer implementation.
    pub fn admit(
        &self,
        id: impl Into<String>,
        expected_revision: u64,
        entries: Vec<ConfigCommandEntry>,
    ) -> Result<ConfigTransactionRecord, ConfigSubmitError> {
        if entries.is_empty() {
            return Err(ConfigSubmitError::Empty);
        }
        let mut implementations = BTreeMap::new();
        for entry in &entries {
            let command = self.command(&entry.owner, &entry.command)?;
            command
                .decode(&entry.args)
                .map_err(|detail| ConfigSubmitError::InvalidArgs {
                    owner: entry.owner.clone(),
                    command: entry.command.clone(),
                    detail,
                })?;
            if let Some(registered) = self.owners.get(&entry.owner) {
                implementations.insert(entry.owner.clone(), registered.owner.implementation());
            }
        }
        Ok(ConfigTransactionRecord {
            id: id.into(),
            expected_revision,
            entries,
            implementations,
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

    /// Whether this registry runs the reducers `transaction` was admitted
    /// under.
    pub fn check_implementations(
        &self,
        transaction: &ConfigTransactionRecord,
    ) -> Result<(), ConfigImplementationMismatch> {
        for (owner, recorded) in &transaction.implementations {
            let current = self
                .owners
                .get(owner)
                .map(|registered| registered.owner.implementation());
            if current.as_deref() != Some(recorded.as_str()) {
                return Err(ConfigImplementationMismatch {
                    owner: owner.clone(),
                    recorded: recorded.clone(),
                    current,
                });
            }
        }
        Ok(())
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
    pub fn resolve(
        &self,
        base: &crate::PersistedSessionConfig,
        transaction: &ConfigTransactionRecord,
        models: &dyn crate::RuntimeModels,
    ) -> ConfigResolution {
        let base_revision = base.config_revision;
        let result = if transaction.expected_revision == base_revision {
            self.reduce(base, transaction, models)
        } else {
            ConfigResolutionDecision::Stale {
                expected: transaction.expected_revision,
                actual: base_revision,
            }
        };
        ConfigResolution {
            base_revision,
            result,
        }
    }

    fn reduce(
        &self,
        base: &crate::PersistedSessionConfig,
        transaction: &ConfigTransactionRecord,
        models: &dyn crate::RuntimeModels,
    ) -> ConfigResolutionDecision {
        let base_core = CoreConfig::of(base);
        let mut candidate: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut outputs = Vec::with_capacity(transaction.entries.len());
        for (index, entry) in transaction.entries.iter().enumerate() {
            let refused =
                |refusal: serde_json::Value, message: String| ConfigResolutionDecision::Refused {
                    refusal: ConfigRefusal {
                        index: Some(index),
                        owner: entry.owner.clone(),
                        command: Some(entry.command.clone()),
                        refusal,
                        message,
                    },
                };
            let command = match self.command(&entry.owner, &entry.command) {
                Ok(command) => command,
                Err(error) => {
                    return refused(
                        serde_json::json!({ "unknown": error.to_string() }),
                        error.to_string(),
                    );
                }
            };
            let recorded = match candidate.get(&entry.owner) {
                Some(value) => value.clone(),
                None if entry.owner == CORE_CONFIG_OWNER => {
                    match serde_json::to_value(&base_core) {
                        Ok(value) => value,
                        Err(error) => {
                            return refused(
                                serde_json::json!({ "unreadable": error.to_string() }),
                                error.to_string(),
                            );
                        }
                    }
                }
                None => match base.plugin_config.get(&entry.owner) {
                    Some(value) => value.clone(),
                    None => {
                        let message = format!(
                            "the session recorded no `{}` config for this command to change",
                            entry.owner
                        );
                        return refused(serde_json::json!({ "unrecorded": entry.owner }), message);
                    }
                },
            };
            match command.reduce(&recorded, &entry.args, models) {
                Ok((next, output)) => {
                    candidate.insert(entry.owner.clone(), next);
                    outputs.push(output);
                }
                Err(ConfigCommandFailure::Refused { refusal, message }) => {
                    return refused(refusal, message);
                }
                Err(ConfigCommandFailure::Unreadable(message)) => {
                    return refused(serde_json::json!({ "unreadable": message }), message);
                }
            }
        }
        let core = match candidate.remove(CORE_CONFIG_OWNER) {
            Some(value) => match serde_json::from_value::<CoreConfig>(value) {
                Ok(core) => Some(core),
                Err(error) => {
                    return ConfigResolutionDecision::Refused {
                        refusal: unreadable(CORE_CONFIG_OWNER, error.to_string()),
                    };
                }
            },
            None => None,
        };
        let namespaces = candidate;
        let final_core = core.clone().unwrap_or_else(|| base_core.clone());
        let mut final_plugins = base.plugin_config.clone();
        final_plugins.apply_namespace_updates(&namespaces);
        let facts = CandidateFacts {
            core: &final_core,
            plugin_config: &final_plugins,
        };
        if core.is_some()
            && let Err(refusal) = core::validate_candidate(&base_core, &final_core)
        {
            return ConfigResolutionDecision::Refused {
                refusal: owner_refusal(CORE_CONFIG_OWNER, &refusal),
            };
        }
        // Every owner judges the final candidate, touched or not: a change
        // to one namespace, the core's included, can break another owner's
        // invariant.
        for (owner, value) in final_plugins.iter() {
            let Some(registered) = self.owners.get(owner.as_str()) else {
                continue;
            };
            if let Err(refusal) =
                registered
                    .owner
                    .validate(owner, value, base.plugin_config.get(owner), &facts)
            {
                return ConfigResolutionDecision::Refused { refusal };
            }
        }
        ConfigResolutionDecision::Applied {
            core: core.map(Box::new),
            namespaces,
            outputs,
        }
    }

    /// Validate a root's config that a run override derived from the
    /// session's `base`: every namespace the override changed is judged by
    /// its owner against its recorded value, so an overlay cannot set what
    /// the owner does not admit. `run_options` is the raw protocol options
    /// the run stated: the protocol's owner judges them first, as stated, so
    /// an option it refuses is refused even when it restates the recorded
    /// value.
    pub fn validate_derived(
        &self,
        base: &crate::PersistedSessionConfig,
        derived: &crate::PersistedSessionConfig,
        run_options: Option<&crate::ProtocolTurnOptions>,
    ) -> Result<(), ConfigRefusal> {
        if let Some(options) = run_options
            && let Some(protocol) = base.plugin_config.protocol_plugin_id()
            && let Some(registered) = self.owners.get(protocol)
        {
            registered
                .owner
                .validate_run_options(protocol, &options.payload)?;
        }
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

#[cfg(test)]
mod tests;
