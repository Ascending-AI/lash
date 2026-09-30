//! Transport-neutral config transaction envelopes (FIG-4379).
//!
//! A remote caller changes a session's config the way a local host does:
//! an ordered list of `{owner, command, args}` entries written against the
//! config revision it read, under a stable id a resubmission reuses. The
//! envelope carries commands only, never a recorded namespace or a caller-
//! minted replacement: every entry's owner decodes its arguments against the
//! schema its registration generated, and the transaction settles as a
//! typed outcome. The command catalog is generated from the same
//! registrations.

use lash_sansio::SessionId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::registry_errors::{RemoteProtocolError, require_non_empty};

/// One command of a config transaction: the owner whose namespace it
/// changes, the command's registered name and its arguments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfigCommandEntry {
    pub owner: String,
    pub command: String,
    pub args: serde_json::Value,
}

/// A config transaction a remote caller submits to one session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfigTransactionRequest {
    pub session_id: SessionId,
    /// The caller's stable id for the transaction; a resubmission with the
    /// same content reuses it, and one with other content is refused.
    pub id: String,
    /// The session config revision the caller wrote the transaction against.
    pub expected_revision: u64,
    /// The commands, applied in order, all or none.
    pub entries: Vec<RemoteConfigCommandEntry>,
}

impl RemoteConfigTransactionRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteConfigTransactionRequest", "id", &self.id)?;
        if self.entries.is_empty() {
            return Err(RemoteProtocolError::MissingRequiredField {
                type_name: "RemoteConfigTransactionRequest",
                field: "entries",
            });
        }
        for entry in &self.entries {
            require_non_empty("RemoteConfigCommandEntry", "owner", &entry.owner)?;
            require_non_empty("RemoteConfigCommandEntry", "command", &entry.command)?;
        }
        Ok(())
    }
}

/// Why an owner refused a config transaction: which command, by which
/// owner, and the owner's typed refusal as data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfigRefusal {
    /// The refused command's position; absent when the final candidate as a
    /// whole failed an owner's validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The owner's refusal, in the shape of its registered refusal schema.
    pub refusal: serde_json::Value,
    pub message: String,
}

/// What a config transaction settled as.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteConfigTransactionOutcome {
    /// Every command applied, and the config moved one revision step.
    Applied {
        base_revision: u64,
        revision: u64,
        outputs: Vec<serde_json::Value>,
    },
    /// The transaction was written against `expected`, but the session's
    /// config was at `actual`: nothing was published.
    Stale { expected: u64, actual: u64 },
    /// An owner refused: nothing was published.
    Refused { refusal: RemoteConfigRefusal },
}

/// One registered config command and its generated schemas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfigCommandDescriptor {
    pub owner: String,
    pub command: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub refusal_schema: serde_json::Value,
}

/// Every config command a session admits, describing its config at
/// `revision`. Discovery only: an owner can still refuse arguments its
/// schema admits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfigCommandCatalog {
    pub revision: u64,
    pub commands: Vec<RemoteConfigCommandDescriptor>,
}
