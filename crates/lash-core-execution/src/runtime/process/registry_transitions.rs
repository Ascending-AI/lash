//! Pure transition tables shared by every durable process registry.
//!
//! A durable registry backend reads rows, asks this table what the observation
//! means, and applies the write the table prescribes. The backend never decides
//! a process schedulable, never classifies a missing process, and never parses a
//! persisted label: those are the decisions that must be identical on SQLite,
//! on PostgreSQL, and on anything else that implements
//! [`ProcessRegistry`](crate::ProcessRegistry), and duplicating them is how
//! they drift.
//!
//! There is no SQL, no clock, no I/O and nothing `async` here. Time-dependent
//! decisions receive the instant supplied by the backend transaction that owns
//! them.

use crate::ProcessId;
use crate::plugin::PluginError;

use super::events::{PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessWakeDelivery};
use super::registry::{
    WakeDelivery, WakeDeliveryDisposition, WakeDeliveryState, WakeDiscardReason,
};

/// Failure text for a persisted registry payload that will not decode.
///
/// One vocabulary for both backends: every process-registry row body that fails
/// `serde_json` reports this, so a corrupt payload reads the same whichever
/// substrate stored it.
fn registry_row_decode_error(err: serde_json::Error) -> PluginError {
    PluginError::Session(format!("failed to decode process registry row: {err}"))
}

#[derive(serde::Deserialize)]
struct ProcessWakeDeliveryFormatVersionProbe {
    version: Option<u32>,
}

/// `fleet_format` is the `F` the bound registry store recorded: the read
/// admits the pair `{fleet's writer version, this build's newest}` — ADR 0106
/// §2's `[N-1, N]` window (FIG-3796). An admitted older payload climbs to the
/// newest through the surface's `RecordUpcaster` hooks; anything else is
/// refused as unsupported.
fn decode_process_wake_delivery(
    delivery_json: &str,
    fleet_format: crate::FleetFormat,
) -> Result<ProcessWakeDelivery, PluginError> {
    let probe: ProcessWakeDeliveryFormatVersionProbe =
        serde_json::from_str(delivery_json).map_err(registry_row_decode_error)?;
    // A delivery written before the format carried a stamp is format 2.
    let found = probe.version.unwrap_or(2);
    let window = fleet_format.read_window(lash_core_store::surface_format!(
        PROCESS_WAKE_DELIVERY_FORMAT_VERSION
    ));
    if !window.admits(found) {
        return Err(PluginError::ProcessWakeDeliveryFormatVersionMismatch {
            expected: window.newest(),
            found,
        });
    }
    if found == window.newest() {
        return serde_json::from_str(delivery_json).map_err(registry_row_decode_error);
    }
    let mut value: serde_json::Value =
        serde_json::from_str(delivery_json).map_err(registry_row_decode_error)?;
    lash_core_store::store::upcast_json_record(
        "process wake delivery",
        lash_core_store::surface_format!(PROCESS_WAKE_DELIVERY_FORMAT_VERSION),
        found,
        window.newest(),
        &mut value,
    )
    .map_err(PluginError::from)?;
    serde_json::from_value(value).map_err(registry_row_decode_error)
}

// ---------------------------------------------------------------------------
// Terminal / tombstone classification
// ---------------------------------------------------------------------------

/// The stamp a pruned process leaves behind in its tombstone row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessTombstoneStamp {
    /// The terminal `status` label the row carried when it was pruned.
    pub terminal_label: String,
    /// When the prune removed the process row.
    pub pruned_at_ms: u64,
}

/// Refusal for a process whose row was pruned but whose tombstone is retained.
pub fn process_no_longer_retained(stamp: ProcessTombstoneStamp) -> PluginError {
    PluginError::ProcessNoLongerRetained {
        terminal_label: stamp.terminal_label,
        pruned_at_ms: stamp.pruned_at_ms,
    }
}

/// Refusal for a process id no registry ever knew.
pub fn unknown_process(process_id: &ProcessId) -> PluginError {
    PluginError::ProcessUnknown {
        process_id: process_id.clone(),
    }
}

/// Classify a lookup that found no live process row.
///
/// The three-way split — retained row, retained tombstone, never known — is
/// what lets a host tell "this finished and was reaped" from "this id is
/// wrong", so the two absent cases must never collapse into one error.
pub fn absent_process_error(
    process_id: &ProcessId,
    tombstone: Option<ProcessTombstoneStamp>,
) -> PluginError {
    match tombstone {
        Some(stamp) => process_no_longer_retained(stamp),
        None => unknown_process(process_id),
    }
}

/// The `status` column labels a process row carries while it is still live —
/// that is, while lash may still act on it.
///
/// Both registries' retention SQL is written against exactly this set —
/// `status IN ('running', 'waiting')` selects live rows, `status NOT IN (…)`
/// selects retention candidates. The set is a constant rather than a query
/// fragment on purpose: the registries keep their literal SQL, and
/// `process_status_labels_partition_live_from_retired` is what fails if a new
/// variant would silently land on the wrong side.
///
/// Live is **not** the complement of terminal.
/// [`ProcessStatus::CallerDeparted`](crate::ProcessStatus::CallerDeparted) is
/// neither: lash may never act on such a row and may never assert an outcome
/// for it, so it is excluded here (recovery must not pick it up) and included
/// in [`RETIRED_PROCESS_STATUS_LABELS`] (retention may reclaim it).
pub const LIVE_PROCESS_STATUS_LABELS: [&str; 2] = ["running", "waiting"];

/// The `status` column labels retention may reclaim, i.e. the exact complement
/// of [`LIVE_PROCESS_STATUS_LABELS`] that both registries' prune SQL selects
/// with `status NOT IN ('running', 'waiting')`.
///
/// Reclaiming a row is a retention act, never an outcome claim, which is why
/// the non-terminal `caller_departed` label belongs here: nothing may ever
/// honestly terminalize such a row, so excluding it would let a host
/// accumulate unresolvable rows without bound.
pub const RETIRED_PROCESS_STATUS_LABELS: [&str; 5] = [
    "completed",
    "failed",
    "cancelled",
    "abandoned",
    "caller_departed",
];

// ---------------------------------------------------------------------------
// Wake reconciliation vocabulary
// ---------------------------------------------------------------------------

/// Refusal for a wake-delivery id with no row.
pub fn unknown_wake_delivery(delivery_id: &str) -> PluginError {
    PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
}

/// The labels are durable values, so this is the only reader: an unrecognised
/// one is a refusal, never a default.
pub fn wake_delivery_state_from_label(
    delivery_id: &str,
    label: &str,
) -> Result<WakeDeliveryState, PluginError> {
    match label {
        "pending" => Ok(WakeDeliveryState::Pending),
        "enqueuing" => Ok(WakeDeliveryState::Enqueuing),
        "enqueued" => Ok(WakeDeliveryState::Enqueued),
        "discarded" => Ok(WakeDeliveryState::Discarded),
        state => Err(PluginError::Session(format!(
            "wake delivery `{delivery_id}` has unknown state `{state}`"
        ))),
    }
}

/// `None` stays `None`: a delivery that was never discarded carries no reason.
/// [`WakeDiscardReason`] is `#[non_exhaustive]`, so this single reader is also
/// the single place a new reason has to be taught.
pub fn wake_discard_reason_from_label(
    delivery_id: &str,
    label: Option<&str>,
) -> Result<Option<WakeDiscardReason>, PluginError> {
    match label {
        None => Ok(None),
        Some("expired") => Ok(Some(WakeDiscardReason::Expired)),
        Some("target_gone") => Ok(Some(WakeDiscardReason::TargetGone)),
        Some("retargeted") => Ok(Some(WakeDiscardReason::Retargeted)),
        Some("sequence_rewound") => Ok(Some(WakeDiscardReason::SequenceRewound)),
        Some(reason) => Err(PluginError::Session(format!(
            "wake delivery `{delivery_id}` has unknown discard reason `{reason}`"
        ))),
    }
}

/// Columns of a persisted wake-delivery row, before projection.
#[derive(Clone, Debug)]
pub struct WakeDeliveryRow {
    /// `delivery_id`, the structural wake identity.
    pub delivery_id: String,
    pub state_label: String,
    /// `claim_token`, the ownership fence of the current `enqueuing` claim.
    pub claim_token: Option<String>,
    pub attempts: i64,
    /// `first_attempt_ms`.
    pub first_attempt_ms: Option<i64>,
    /// `next_attempt_at_ms`.
    pub next_attempt_at_ms: i64,
    pub expires_at_ms: i64,
    /// `discard_reason`.
    pub discard_reason_label: Option<String>,
    /// `delivery_json`, the encoded [`ProcessWakeDelivery`](crate::ProcessWakeDelivery).
    pub delivery_json: String,
}

impl WakeDeliveryRow {
    /// Project the row into a [`WakeDelivery`].
    ///
    /// The two label columns are parsed before the payload is decoded, so a row
    /// with an unrecognised state reports the state refusal rather than a decode
    /// failure. `fleet_format` is the `F` the bound store recorded: the
    /// delivery payload's read window comes from it (FIG-3796).
    pub fn project(self, fleet_format: crate::FleetFormat) -> Result<WakeDelivery, PluginError> {
        let state = wake_delivery_state_from_label(&self.delivery_id, &self.state_label)?;
        let discard_reason = wake_discard_reason_from_label(
            &self.delivery_id,
            self.discard_reason_label.as_deref(),
        )?;
        let disposition = match (state, self.claim_token) {
            (WakeDeliveryState::Pending, _) => WakeDeliveryDisposition::Pending,
            (WakeDeliveryState::Enqueuing, Some(claim_token)) => {
                WakeDeliveryDisposition::Enqueuing { claim_token }
            }
            (WakeDeliveryState::Enqueuing, None) => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{}` is enqueuing without a claim token",
                    self.delivery_id
                )));
            }
            (WakeDeliveryState::Enqueued, _) => WakeDeliveryDisposition::Enqueued,
            (WakeDeliveryState::Discarded, _) => match discard_reason {
                Some(reason) => WakeDeliveryDisposition::Discarded { reason },
                None => WakeDeliveryDisposition::DiscardedUnattributed,
            },
        };
        let wake = decode_process_wake_delivery(&self.delivery_json, fleet_format)?;
        Ok(WakeDelivery {
            delivery_id: self.delivery_id,
            wake,
            disposition,
            attempts: self.attempts as u64,
            first_attempt_ms: self.first_attempt_ms.map(|value| value as u64),
            next_attempt_at_ms: self.next_attempt_at_ms as u64,
            expires_at_ms: self.expires_at_ms as u64,
        })
    }
}
