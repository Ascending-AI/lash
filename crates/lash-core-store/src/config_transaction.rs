//! Typed config commands as recorded data (FIG-4379).
//!
//! A session's config changes only through a config transaction: an ordered
//! list of typed commands, each naming the owner whose namespace it changes —
//! the reserved core owner or an installed plugin — with its arguments. The
//! transaction rides the session's command lane as
//! [`SessionCommand::ApplyConfigTransaction`](crate::queued_work_vocabulary::SessionCommand), carrying
//! the config revision its submitter wrote it against and the reducer
//! implementation each named owner ran at ingress.
//!
//! At the command drain the transaction is resolved once and the resolution
//! is recorded before anything publishes ([`ConfigResolution`]): either the
//! complete replacements for every touched owner with each command's typed
//! output, a stale base, or a typed refusal. One fenced commit then publishes
//! the replacements, advances `config_revision` exactly once and settles the
//! command with its [`ConfigTransactionOutcome`]. A redrive publishes the
//! recorded resolution and never runs a reducer again.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The owner id of the session's core config: provider, model, prompt,
/// generation, budget and tool access. No plugin may register it.
pub const CORE_CONFIG_OWNER: &str = "core";

/// The core owner's share of a session's recorded config: every config head
/// field a core command changes. It is a view of
/// [`PersistedSessionConfig`](crate::PersistedSessionConfig), which stays the
/// one record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoreConfig {
    pub provider_id: String,
    pub model: crate::ModelSpec,
    pub turn_budget: crate::TurnBudget,
    /// `None` only for a head written before prompt persistence existed; a
    /// command that changes the prompt records `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::PromptLayer>,
    pub generation: crate::GenerationOptions,
    pub tool_access: crate::SessionToolAccess,
}

impl CoreConfig {
    /// The core share of `config`.
    pub fn of(config: &crate::PersistedSessionConfig) -> Self {
        Self {
            provider_id: config.provider_id.clone(),
            model: config.model.clone(),
            turn_budget: config.turn_budget,
            prompt: config.prompt.clone(),
            generation: config.generation.clone(),
            tool_access: config.tool_access.clone(),
        }
    }

    /// Write this core share into `config`, leaving every other field alone.
    pub fn apply_to(&self, config: &mut crate::PersistedSessionConfig) {
        config.provider_id = self.provider_id.clone();
        config.model = self.model.clone();
        config.turn_budget = self.turn_budget;
        config.prompt = self.prompt.clone();
        config.generation = self.generation.clone();
        config.tool_access = self.tool_access.clone();
    }

    /// The prompt layer a prompt command edits: the recorded one, or an
    /// empty layer for a head that recorded none.
    pub fn prompt_layer(&self) -> crate::PromptLayer {
        self.prompt.clone().unwrap_or_default()
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
    /// Each named owner's reducer implementation identity at ingress. A
    /// drain whose installed owners differ resolves nothing and waits for a
    /// build that runs these reducers.
    pub implementations: BTreeMap<String, String>,
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
            "lash-config-transaction/v1",
            canonical.as_bytes(),
        ))
    }
}

/// Why a config transaction was refused: which command, by which owner, and
/// the owner's typed refusal as data. Nothing of a refused transaction is
/// published.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigRefusal {
    /// The refused command's position in the transaction; `None` when the
    /// final candidate as a whole failed an owner's validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The owner's refusal, serialized from its declared refusal type.
    #[schemars(with = "serde_json::Value")]
    pub refusal: serde_json::Value,
    /// The refusal's display text.
    pub message: String,
}

impl std::fmt::Display for ConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.command, self.index) {
            (Some(command), Some(index)) => write!(
                formatter,
                "config command {index} (`{}.{command}`) refused: {}",
                self.owner, self.message
            ),
            _ => write!(
                formatter,
                "config owner `{}` refused the candidate: {}",
                self.owner, self.message
            ),
        }
    }
}

impl std::error::Error for ConfigRefusal {}

/// What a config transaction settled as, carried by the commit that settles
/// its command and read back by any submitter holding its receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigTransactionOutcome {
    /// Every command applied: the config moved from `base_revision` to
    /// `revision` (one step, even for a restatement), and `outputs` holds
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
    pub result: ConfigResolutionResult,
}

/// The recorded decision of a config transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigResolutionResult {
    /// The complete replacements for every touched owner, and each
    /// command's output in order.
    Applied {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        core: Option<Box<CoreConfig>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        namespaces: BTreeMap<String, serde_json::Value>,
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
            ConfigResolutionResult::Applied {
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
            ConfigResolutionResult::Stale { expected, actual } => ConfigTransactionOutcome::Stale {
                expected: *expected,
                actual: *actual,
            },
            ConfigResolutionResult::Refused { refusal } => ConfigTransactionOutcome::Refused {
                refusal: refusal.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> crate::PersistedSessionConfig {
        let mut config = crate::PersistedSessionConfig::from(&crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        config.config_revision = 4;
        config
    }

    fn applied(core: Option<CoreConfig>) -> ConfigResolution {
        ConfigResolution {
            base_revision: 4,
            result: ConfigResolutionResult::Applied {
                core: core.map(Box::new),
                namespaces: BTreeMap::from([(
                    "counter".to_string(),
                    serde_json::json!({ "count": 2 }),
                )]),
                outputs: vec![serde_json::json!(2)],
            },
        }
    }

    /// ADR 0101 §12: an applied resolution publishes every recorded
    /// replacement and advances the revision by exactly one, however many
    /// commands it resolved and even when it restates the current values.
    #[test]
    fn an_applied_resolution_publishes_its_replacements_with_one_revision_step() {
        let mut config = head();
        let mut core = CoreConfig::of(&config);
        core.turn_budget = crate::TurnBudget::bounded(7);

        let outcome = applied(Some(core)).publish(&mut config);

        assert_eq!(
            outcome,
            ConfigTransactionOutcome::Applied {
                base_revision: 4,
                revision: 5,
                outputs: vec![serde_json::json!(2)],
            }
        );
        assert_eq!(config.turn_budget, crate::TurnBudget::bounded(7));
        assert_eq!(
            config.plugin_config.get("counter"),
            Some(&serde_json::json!({ "count": 2 }))
        );

        let restating = ConfigResolution {
            base_revision: 5,
            result: ConfigResolutionResult::Applied {
                core: Some(Box::new(CoreConfig::of(&config))),
                namespaces: BTreeMap::new(),
                outputs: Vec::new(),
            },
        };
        restating.publish(&mut config);
        assert_eq!(config.config_revision, 6, "a restatement still steps once");
    }

    /// A stale or refused resolution publishes nothing and keeps the
    /// revision.
    #[test]
    fn stale_and_refused_resolutions_publish_nothing() {
        let before = head();
        for result in [
            ConfigResolutionResult::Stale {
                expected: 3,
                actual: 4,
            },
            ConfigResolutionResult::Refused {
                refusal: ConfigRefusal {
                    index: Some(0),
                    owner: "counter".to_string(),
                    command: Some("increment".to_string()),
                    refusal: serde_json::json!({ "kind": "too_large" }),
                    message: "too large".to_string(),
                },
            },
        ] {
            let mut config = before.clone();
            let outcome = ConfigResolution {
                base_revision: 4,
                result,
            }
            .publish(&mut config);
            assert!(!matches!(outcome, ConfigTransactionOutcome::Applied { .. }));
            assert_eq!(config, before);
        }
    }

    /// The digest names what the submitter asked for, never the reducer
    /// identities ingress stamped on it.
    #[test]
    fn the_digest_covers_the_request_and_not_the_admitted_implementations() {
        let entry = ConfigCommandEntry {
            owner: "counter".to_string(),
            command: "increment".to_string(),
            args: serde_json::json!({ "by": 1 }),
        };
        let record = ConfigTransactionRecord {
            id: "tx".to_string(),
            expected_revision: 4,
            entries: vec![entry.clone()],
            implementations: BTreeMap::from([("counter".to_string(), "a".to_string())]),
        };
        let restamped = ConfigTransactionRecord {
            implementations: BTreeMap::from([("counter".to_string(), "b".to_string())]),
            ..record.clone()
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
        assert_eq!(restamped.digest().expect("digest"), digest);
        assert_ne!(rewritten.digest().expect("digest"), digest);
        assert_ne!(reordered.digest().expect("digest"), digest);
    }
}
