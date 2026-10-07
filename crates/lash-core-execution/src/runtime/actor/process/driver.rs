//! The process driver's state: what lash keeps beside an engine's state so
//! the next event can be derived from rows (ADR 0132 §10). It is stored on
//! the process row and moves in the same `process.advance` commit as the
//! engine state, so the two never disagree.

use std::collections::BTreeMap;

use lash_durable::domain::WaitId;
use serde::{Deserialize, Serialize};

use crate::runtime::process::engine_state::{KeyName, StepName, StepRequest};
use crate::{ProcessId, ToolCallId};

/// The driver's state of one process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Driver {
    /// The engine's cancel grace in milliseconds, recorded with the first
    /// transition and read back from here ever after.
    pub(super) cancel_grace_ms: u64,
    /// When the forced terminal is due: set by the transition that
    /// delivered `Cancelled`, which is delivered once.
    pub(super) grace_until: Option<i64>,
    /// The run the next `Steps` action is admitted as.
    pub(super) next_run: u64,
    /// The steps admitted and not yet handed to `advance`, by name.
    pub(super) steps: BTreeMap<StepName, InFlight>,
    /// The host-resolvable keys pinned so far, by name.
    pub(super) keys: BTreeMap<KeyName, Pinned>,
    /// What the process is blocked on, if anything.
    pub(super) blocked: Option<Blocked>,
    /// An event the next transition receives at once.
    pub(super) immediate: Option<Immediate>,
}

/// One admitted step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InFlight {
    /// The run that admitted it.
    pub(super) run: u64,
    /// Its member index in that run.
    pub(super) member: u64,
    /// Its call.
    pub(super) call: ToolCallId,
    /// The request, so a `Repeatable` body can run again at its ordinal.
    pub(super) request: StepRequest,
    /// The policy its admission pinned.
    pub(super) policy: lash_sansio::ExecutionPolicy,
    /// Its limit's expiry, as admitted: never refreshed.
    pub(super) limit_expires_at: u64,
    /// Its limit's longest slice in milliseconds, as admitted.
    pub(super) limit_max_slice_ms: u64,
}

impl InFlight {
    /// The limit its admission recorded.
    pub(super) fn limit(&self) -> lash_sansio::ExecutionLimit {
        lash_sansio::ExecutionLimit {
            expires_at: self.limit_expires_at,
            max_slice: std::time::Duration::from_millis(self.limit_max_slice_ms),
        }
    }
}

/// One pinned key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pinned {
    /// Its wait row.
    pub(super) wait: StoredWaitId,
    /// The key the engine hands out.
    pub(super) key: String,
}

/// What a process waits for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Blocked {
    /// A pinned key's resolution.
    External {
        /// The key.
        name: KeyName,
        /// Its wait row.
        wait: StoredWaitId,
    },
    /// Another process's terminal.
    Process {
        /// The awaited process.
        process: ProcessId,
        /// The `process_terminal` wait row.
        wait: StoredWaitId,
    },
    /// A durable instant.
    Sleep {
        /// When, in store milliseconds.
        until: i64,
    },
    /// Nothing but its mailbox.
    Idle,
}

/// An event the next transition receives without waiting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Immediate {
    /// A key the last transition pinned.
    KeyPinned {
        /// The key.
        name: KeyName,
    },
    /// The event the last transition emitted is committed.
    Emitted,
}

/// A wait id as the driver stores it: 32 lowercase hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct StoredWaitId(pub(super) WaitId);

impl Serialize for StoredWaitId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(32);
        for byte in self.0.0 {
            text.push_str(&format!("{byte:02x}"));
        }
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for StoredWaitId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let invalid = || serde::de::Error::custom(format!("`{text}` is not a stored wait id"));
        if text.len() != 32 {
            return Err(invalid());
        }
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let pair = text.get(index * 2..index * 2 + 2).ok_or_else(invalid)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| invalid())?;
        }
        Ok(Self(WaitId(bytes)))
    }
}

impl Driver {
    /// The driver state stored on a process row, or the state before the
    /// first transition.
    pub(super) fn decode(stored: Option<&str>) -> Result<Self, serde_json::Error> {
        stored.map_or_else(|| Ok(Self::default()), serde_json::from_str)
    }

    /// The stored encoding.
    #[expect(
        clippy::expect_used,
        reason = "the driver state is plain data whose encoding cannot fail"
    )]
    pub(super) fn encode(&self) -> String {
        serde_json::to_string(self).expect("the driver state encodes")
    }
}
