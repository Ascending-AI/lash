//! Typed config commands as recorded data (FIG-4379).
//!
//! A session's config changes only through a config transaction: an ordered
//! list of typed commands, each naming the owner whose namespace it changes —
//! the reserved core owner or an installed plugin — with its arguments. The
//! transaction rides the session's command lane as
//! [`SessionCommand::ApplyConfigTransaction`](crate::queued_work_vocabulary::SessionCommand), carrying
//! the config revision its submitter wrote it against.
//!
//! At the command drain the transaction is resolved once and the resolution
//! is recorded before anything publishes ([`ConfigResolution`]): either the
//! complete replacements for every touched owner with each command's typed
//! output, a stale base, or a typed refusal. One fenced commit then publishes
//! the replacements, advances `config_revision` exactly once and settles the
//! command with its [`ConfigTransactionOutcome`]. A redrive publishes the
//! recorded resolution and never runs a reducer again.

/// version_surface = "coexist"
/// version_guard(items(LASH_CONFIG_TRANSACTION_DOMAIN_VERSION, digest))
const LASH_CONFIG_TRANSACTION_DOMAIN_VERSION: &str = "lash-config-transaction/v1";

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The owner id of the session's core config: model, reasoning, attachment
/// acceptance, generation, the execution controls and tool access.
/// No plugin may register it.
pub const CORE_CONFIG_OWNER: &str = "core";

/// The core owner's share of a session's recorded config: every config head
/// field a core command changes. It is a view of
/// [`PersistedSessionConfig`](crate::PersistedSessionConfig), which stays the
/// one record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoreConfig {
    /// The recorded model and the reasoning it runs with. A model command
    /// records the binding the host's registry minted when it resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<crate::LlmProfileConfig>,
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    pub attachment_acceptance: std::sync::Arc<crate::provider::AttachmentCapabilitySnapshot>,
    pub turn_budget: crate::TurnBudget,
    pub max_tool_calls: crate::MaxToolCalls,
    pub autonomous: bool,
    pub no_progress_budget: crate::NoProgressBudget,
    pub charge_safety: crate::ChargeSafetyPolicy,
    pub generation: crate::GenerationOptions,
    pub tool_access: crate::SessionToolAccess,
}

impl CoreConfig {
    /// The core share of `config`.
    pub fn of(config: &crate::PersistedSessionConfig) -> Self {
        Self {
            model: config.model.clone(),
            attachment_acceptance: std::sync::Arc::clone(&config.attachment_acceptance),
            turn_budget: config.turn_budget,
            max_tool_calls: config.max_tool_calls,
            autonomous: config.autonomous,
            no_progress_budget: config.no_progress_budget,
            charge_safety: config.charge_safety.clone(),
            generation: config.generation.clone(),
            tool_access: config.tool_access.clone(),
        }
    }

    /// Write this core share into `config`, leaving every other field alone.
    pub fn apply_to(&self, config: &mut crate::PersistedSessionConfig) {
        config.model = self.model.clone();
        config.attachment_acceptance = std::sync::Arc::clone(&self.attachment_acceptance);
        config.turn_budget = self.turn_budget;
        config.max_tool_calls = self.max_tool_calls;
        config.autonomous = self.autonomous;
        config.no_progress_budget = self.no_progress_budget;
        config.charge_safety = self.charge_safety.clone();
        config.generation = self.generation.clone();
        config.tool_access = self.tool_access.clone();
    }
}

/// One command of a config transaction: the owner whose namespace it
/// changes, the command's registered name and its arguments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigCommandEntry {
    pub owner: String,
    pub command: String,
    #[schemars(with = "serde_json::Value")]
    pub args: serde_json::Value,
}

/// A config transaction as the command lane carries it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigTransactionRecord {
    /// The submitter's stable id for this transaction; its command's
    /// idempotency key.
    pub id: String,
    /// The `config_revision` the submitter wrote the transaction against.
    pub expected_revision: u64,
    /// The commands, applied in order to one private candidate.
    pub entries: Vec<ConfigCommandEntry>,
}

impl ConfigTransactionRecord {
    /// The digest of what the submitter asked for: the revision it wrote
    /// against and its ordered commands. Two submissions under one id with
    /// different digests are different transactions.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let submitted = serde_json::json!({
            "expected_revision": self.expected_revision,
            "entries": self.entries,
        });
        let canonical = lash_core_ids::stable_hash::stable_json_string(&submitted)?;
        Ok(lash_core_ids::stable_hash::blake3_hex(
            LASH_CONFIG_TRANSACTION_DOMAIN_VERSION,
            canonical.as_bytes(),
        ))
    }
}

/// Where a config refusal was raised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RefusalSite {
    /// One command of a transaction, by its position and registered name.
    Command { index: usize, command: String },
    /// The candidate as a whole: a transaction's final candidate, or the
    /// config a run's overrides derived.
    Candidate,
    /// The creation of the session's namespace.
    Creation,
}

/// Which value of a config judgment could not be read or written as its
/// owner's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigValueRole {
    /// What a creator stated for the namespace.
    CreationInput,
    /// A command's arguments.
    Arguments,
    /// The options a run stated for the namespace.
    RunOptions,
    /// The namespace the owner produced: created, reduced or overridden.
    Candidate,
    /// A command's output.
    Output,
    /// The owner's own refusal, which did not encode.
    Refusal,
}

impl ConfigValueRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreationInput => "creation input",
            Self::Arguments => "arguments",
            Self::RunOptions => "run options",
            Self::Candidate => "candidate config",
            Self::Output => "output",
            Self::Refusal => "refusal",
        }
    }
}

/// Why a config change was refused. Only [`Self::Owner`] carries data in the
/// owner's registered refusal type; every other reason is the framework's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigRefusalReason {
    /// The namespace's stored or requested writer format is unavailable.
    Format {
        refusal: crate::plugin_state::FormatRefusal,
    },
    /// The owner refused: `refusal` is its registered refusal type,
    /// serialized, and `message` that refusal's display text.
    Owner {
        #[schemars(with = "serde_json::Value")]
        refusal: serde_json::Value,
        message: String,
    },
    /// No installed plugin registers the named owner.
    UnknownOwner,
    /// The owner registers no command of the name the site carries.
    UnknownCommand,
    /// The session recorded no namespace for the owner, so there is nothing
    /// for the command to change.
    UnrecordedNamespace,
    /// A value did not read or write as the owner's type.
    Unreadable {
        role: ConfigValueRole,
        message: String,
    },
}

// `serde_json::Value` never holds NaN or an infinite number, so its
// `PartialEq` is reflexive.
impl Eq for ConfigRefusalReason {}

impl std::fmt::Display for ConfigRefusalReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format { refusal } => refusal.fmt(formatter),
            Self::Owner { message, .. } => formatter.write_str(message),
            Self::UnknownOwner => formatter.write_str("no installed plugin registers this owner"),
            Self::UnknownCommand => formatter.write_str("the owner registers no such command"),
            Self::UnrecordedNamespace => {
                formatter.write_str("the session recorded no config for this owner")
            }
            Self::Unreadable { role, message } => {
                write!(formatter, "its {} cannot be read: {message}", role.as_str())
            }
        }
    }
}

/// A refused config change: the owner it names, where it was refused and
/// why. Nothing of a refused change is published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigRefusal {
    pub owner: String,
    pub at: RefusalSite,
    pub reason: ConfigRefusalReason,
}

impl ConfigRefusalReason {
    /// An owner's own typed `refusal`.
    pub fn by_owner<R>(refusal: &R) -> Self
    where
        R: Serialize + std::fmt::Display,
    {
        match serde_json::to_value(refusal) {
            Ok(encoded) => Self::Owner {
                refusal: encoded,
                message: refusal.to_string(),
            },
            Err(error) => Self::Unreadable {
                role: ConfigValueRole::Refusal,
                message: format!("{refusal}: {error}"),
            },
        }
    }
}

impl ConfigRefusal {
    /// `owner`'s own typed `refusal`, raised at `at`.
    pub fn by_owner<R>(owner: impl Into<String>, at: RefusalSite, refusal: &R) -> Self
    where
        R: Serialize + std::fmt::Display,
    {
        Self {
            owner: owner.into(),
            at,
            reason: ConfigRefusalReason::by_owner(refusal),
        }
    }

    /// The owner's refusal as its registered type `R`: `None` for a
    /// framework reason, and for a refusal that is not an `R`.
    pub fn owner_refusal<R: serde::de::DeserializeOwned>(&self) -> Option<R> {
        match &self.reason {
            ConfigRefusalReason::Owner { refusal, .. } => {
                serde_json::from_value(refusal.clone()).ok()
            }
            ConfigRefusalReason::Format { .. }
            | ConfigRefusalReason::UnknownOwner
            | ConfigRefusalReason::UnknownCommand
            | ConfigRefusalReason::UnrecordedNamespace
            | ConfigRefusalReason::Unreadable { .. } => None,
        }
    }
}

impl std::fmt::Display for ConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { owner, at, reason } = self;
        match at {
            RefusalSite::Command { index, command } => write!(
                formatter,
                "config command {index} (`{owner}.{command}`) refused: {reason}"
            ),
            RefusalSite::Candidate => write!(
                formatter,
                "config owner `{owner}` refused the candidate: {reason}"
            ),
            RefusalSite::Creation => write!(
                formatter,
                "config owner `{owner}` refused the session's creation: {reason}"
            ),
        }
    }
}

impl std::error::Error for ConfigRefusal {}

/// An owner's recorded namespace that does not read as the owner's recorded
/// type. The session recorded it from that owner's own typed value, so this
/// is corruption of stored data, never a refusal of the change being judged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("config owner `{owner}`'s recorded namespace is unreadable: {message}")]
pub struct RecordedNamespaceCorrupt {
    pub owner: String,
    pub message: String,
}

impl RecordedNamespaceCorrupt {
    /// This corruption as the store's typed error for unreadable stored
    /// data.
    pub fn into_store_error(self) -> crate::StoreError {
        crate::StoreError::StoredDataCorrupt {
            record_kind: "session_config_namespace",
            message: self.to_string(),
        }
    }
}

/// Why a config judgment gave no verdict to publish: the change was
/// refused, or the recorded config it is judged against is corrupt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigFault {
    #[error(transparent)]
    Refused(#[from] ConfigRefusal),
    #[error(transparent)]
    RecordedCorrupt(#[from] RecordedNamespaceCorrupt),
}

/// What a config transaction settled as, carried by the commit that settles
/// its command and read back by any submitter holding its receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigTransactionOutcome {
    /// Every command applied: the config moved from `base_revision` to
    /// `revision` (one step, even when it sets the same values), and `outputs` holds
    /// each command's typed output in order.
    Applied {
        base_revision: u64,
        revision: u64,
        #[schemars(with = "Vec<serde_json::Value>")]
        outputs: Vec<serde_json::Value>,
    },
    /// The transaction was written against `expected`, but the session's
    /// config was at `actual`: no reducer ran and nothing was published.
    Stale { expected: u64, actual: u64 },
    /// An owner refused: nothing was published.
    Refused { refusal: ConfigRefusal },
}

/// A config transaction's resolution, recorded before publication: the
/// base it resolved against and what it resolved to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigResolution {
    pub base_revision: u64,
    pub result: ConfigResolutionDecision,
}

/// The recorded decision of a config transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigResolutionDecision {
    /// The complete replacements for every touched owner, and each
    /// command's output in order.
    Applied {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        core: Option<Box<CoreConfig>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        namespaces: BTreeMap<String, crate::plugin_state::PluginConfigNamespace>,
        outputs: Vec<serde_json::Value>,
    },
    Stale {
        expected: u64,
        actual: u64,
    },
    Refused {
        refusal: ConfigRefusal,
    },
}

impl ConfigResolution {
    /// Publish this resolution onto `config`: the recorded replacements, and
    /// one revision step. A stale or refused resolution publishes nothing.
    /// Returns the outcome the command settles as.
    pub fn publish(&self, config: &mut crate::PersistedSessionConfig) -> ConfigTransactionOutcome {
        match &self.result {
            ConfigResolutionDecision::Applied {
                core,
                namespaces,
                outputs,
            } => {
                if let Some(core) = core {
                    core.apply_to(config);
                }
                config.plugin_config.apply_namespace_updates(namespaces);
                config.config_revision = self.base_revision.saturating_add(1);
                ConfigTransactionOutcome::Applied {
                    base_revision: self.base_revision,
                    revision: config.config_revision,
                    outputs: outputs.clone(),
                }
            }
            ConfigResolutionDecision::Stale { expected, actual } => {
                ConfigTransactionOutcome::Stale {
                    expected: *expected,
                    actual: *actual,
                }
            }
            ConfigResolutionDecision::Refused { refusal } => ConfigTransactionOutcome::Refused {
                refusal: refusal.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest names what the submitter asked for: the revision it wrote
    /// against and its ordered commands.
    #[test]
    fn the_digest_covers_the_revision_and_the_ordered_commands() {
        let entry = ConfigCommandEntry {
            owner: "counter".to_string(),
            command: "increment".to_string(),
            args: serde_json::json!({ "by": 1 }),
        };
        let record = ConfigTransactionRecord {
            id: "tx".to_string(),
            expected_revision: 4,
            entries: vec![entry.clone()],
        };
        let rewritten = ConfigTransactionRecord {
            expected_revision: 5,
            ..record.clone()
        };
        let reordered = ConfigTransactionRecord {
            entries: vec![
                entry.clone(),
                ConfigCommandEntry {
                    args: serde_json::json!({ "by": 2 }),
                    ..entry
                },
            ],
            ..record.clone()
        };

        let digest = record.digest().expect("digest");
        assert_eq!(record.clone().digest().expect("digest"), digest);
        assert_ne!(rewritten.digest().expect("digest"), digest);
        assert_ne!(reordered.digest().expect("digest"), digest);
    }
}
