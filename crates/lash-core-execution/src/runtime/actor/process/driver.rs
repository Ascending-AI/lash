//! The process driver's state: what lash keeps beside an engine's state so
//! the next event can be derived from rows (ADR 0132 §10). It is stored on
//! the process row and moves in the same `process.advance` commit as the
//! engine state, so the two never disagree.

use std::collections::BTreeMap;

use lash_durable::domain::WaitId;
use serde::{Deserialize, Serialize};

use crate::ToolCallId;
use lash_execution_contracts::process::KeyName;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_vocabulary::identity::ProcessId;

/// The driver's state of one process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Driver {
    /// Current namespace postimage, committed with outcomes before active runs
    /// can be consumed. Recovery never needs completed run records.
    pub(super) namespace_state: Option<crate::plugin::PluginState>,
    /// Evidence cut pinned by the run's first transition. Required on reopen;
    /// a missing recorded value is never inferred from the current host.
    pub(super) effect_occurrence_cap: u64,
    /// The last whole component state/config conversion postimage.
    pub(super) component_formats: Option<super::component_formats::ComponentFormatCut>,
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
    /// How many catalog tool steps each run admitted, while any step of the
    /// run is in flight: what the process holds against its
    /// `max_tool_calls` (FIG-4546).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) held: BTreeMap<u64, usize>,
    /// The host-resolvable keys pinned so far, by name.
    pub(super) keys: BTreeMap<KeyName, Pinned>,
    /// What the process is blocked on, if anything.
    pub(super) blocked: Option<Blocked>,
    /// An event the next transition receives at once.
    pub(super) immediate: Option<Immediate>,
    /// How many effect occurrences each node has had recorded one by one,
    /// across all of its sites: what the per-node cap is held against.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) recorded_effects: BTreeMap<String, u64>,
    /// The settled effect occurrences past the per-node cap, by node and
    /// outcome class: recorded once, with the terminal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) omitted_effects:
        BTreeMap<String, lash_execution_contracts::process::ProcessEffectOmittedCounts>,
    /// A host's successor request (FIG-6121), recorded by the pass that read
    /// its mail and applied, or refused, once the run is at rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) successor: Option<lash_execution_contracts::process::ProcessSuccessorRequest>,
    /// The status the engine projected from the state the last transition
    /// committed, when its registration declares a projection: recorded in
    /// that transition's own commit, read back by [`engine_status`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<Box<lash_execution_contracts::process::EngineStatus>>,
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
    pub(super) target: Option<crate::runtime::process::steps::InvocationTarget>,
    /// The operation name its admission captured, independent of live providers.
    pub(super) operation: Option<lash_kernel_value::names::QualifiedOperation>,
    /// Whether it is the resolution of a granted tool's call (P6): its
    /// committed outcome is the match lash consumes, never the engine's
    /// answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) resolves: bool,
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
        /// The node that waits, when the engine named one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_kernel_value::sites::EffectId>,
    },
    /// Another process's terminal.
    Process {
        /// The awaited process.
        process: ProcessId,
        /// The `process_terminal` wait row.
        wait: StoredWaitId,
        /// The node that waits, when the engine named one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_kernel_value::sites::EffectId>,
    },
    /// A durable instant.
    Sleep {
        /// When, in store milliseconds.
        until: i64,
        /// The node that sleeps, when the engine named one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<lash_kernel_value::sites::EffectId>,
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

/// The status the engine of `process` projected from its last committed
/// transition, or `None` for a process whose engine projects none, one
/// that has not transitioned, or one that is not retained.
///
/// # Errors
///
/// A store fault, or a stored status that does not decode.
pub async fn engine_status(
    reads: &dyn lash_durable::DurableReads,
    process: &ProcessId,
) -> Result<Option<lash_execution_contracts::process::EngineStatus>, lash_durable::DurableError> {
    /// The one field of a driver row this read decodes: a `SessionTurn`
    /// process's driver is another shape, and records no status.
    #[derive(Deserialize)]
    struct Recorded {
        #[serde(default)]
        status: Option<lash_execution_contracts::process::EngineStatus>,
    }
    let Some(row) = reads.process(process).await? else {
        return Ok(None);
    };
    let Some(stored) = row.driver_json else {
        return Ok(None);
    };
    serde_json::from_str::<Recorded>(&stored)
        .map(|recorded| recorded.status)
        .map_err(|error| {
            super::activation::corrupt(
                &format!("process `{process}`'s recorded engine status"),
                error,
            )
        })
}

impl Driver {
    pub(crate) fn idle(&self) -> bool {
        matches!(self.blocked, Some(Blocked::Idle))
    }
    /// The driver state stored on a process row, or the state before the
    /// first transition.
    pub(crate) fn decode(stored: Option<&str>) -> Result<Self, serde_json::Error> {
        stored.map_or_else(|| Ok(Self::default()), serde_json::from_str)
    }

    /// The stored encoding.
    #[expect(
        clippy::expect_used,
        reason = "the driver state is plain data whose encoding cannot fail"
    )]
    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).expect("the driver state encodes")
    }
}
