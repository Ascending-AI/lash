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
use super::registry::{WakeDelivery, WakeDeliveryLifecycle, WakeDeliveryState, WakeDiscardReason};

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
    version: u32,
}

/// `fleet_format` is the `F` the bound registry store recorded: the read
/// admits the pair `{fleet's writer version, this build's newest}` — ADR 0106
/// §2's `[N-1, N]` window (FIG-3796). An admitted older payload climbs to the
/// newest through the surface's `RecordUpcaster` hooks; anything else is
/// refused as unsupported.
pub(super) fn decode_process_wake_delivery(
    delivery_json: &str,
    fleet_format: crate::FleetFormat,
) -> Result<ProcessWakeDelivery, PluginError> {
    let probe: ProcessWakeDeliveryFormatVersionProbe =
        serde_json::from_str(delivery_json).map_err(registry_row_decode_error)?;
    let found = probe.version;
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
    /// The status the row carried when it was pruned.
    pub terminal_label: crate::RetiredProcessStatus,
    /// When the prune removed the process row.
    pub pruned_at_ms: u64,
}

impl ProcessTombstoneStamp {
    /// The stamp of `process_id`'s stored tombstone row.
    ///
    /// # Errors
    ///
    /// A stored label that is not a retired status: the row is corrupt, and
    /// is refused rather than reported under a status it never had.
    pub fn from_row(
        process_id: &ProcessId,
        terminal_label: &str,
        pruned_at_ms: u64,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            terminal_label: retired_process_status_from_label(process_id, terminal_label)?,
            pruned_at_ms,
        })
    }
}

/// The single reader of a tombstone's stored `terminal_label`: an
/// unrecognised or live label is a refusal, never a default.
pub fn retired_process_status_from_label(
    process_id: &ProcessId,
    label: &str,
) -> Result<crate::RetiredProcessStatus, PluginError> {
    crate::RetiredProcessStatus::from_label(label).ok_or_else(|| {
        PluginError::Session(format!(
            "process `{process_id}` tombstone has unknown retired status `{label}`"
        ))
    })
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
pub const LIVE_PROCESS_STATUS_LABELS: [&str; 2] = ["running", "waiting"];

/// The `status` column labels retention may reclaim, i.e. the exact complement
/// of [`LIVE_PROCESS_STATUS_LABELS`] that both registries' prune SQL selects
/// with `status NOT IN ('running', 'waiting')`.
///
/// Reclaiming a row is a retention act, never an outcome claim.
pub const RETIRED_PROCESS_STATUS_LABELS: [&str; 4] =
    ["completed", "failed", "cancelled", "abandoned"];

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
        Some("source_unreadable") => Ok(Some(WakeDiscardReason::SourceUnreadable)),
        Some("content_conflict") => Ok(Some(WakeDiscardReason::ContentConflict)),
        Some(reason) => Err(PluginError::Session(format!(
            "wake delivery `{delivery_id}` has unknown discard reason `{reason}`"
        ))),
    }
}

/// Columns of a persisted wake-delivery row, before projection.
#[derive(Clone, Debug)]
pub struct WakeDeliveryRow {
    /// `delivery_id`, which must be the identity the decoded wake computes.
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
        // The row is total (`ck_process_wake_deliveries_lifecycle`): a claim
        // token exactly while enqueuing, a discard reason exactly once
        // discarded. A row outside that is refused, never repaired.
        let disposition = match (state, self.claim_token, discard_reason) {
            (WakeDeliveryState::Pending, None, None) => WakeDeliveryLifecycle::Pending,
            (WakeDeliveryState::Enqueuing, Some(claim_token), None) => {
                WakeDeliveryLifecycle::Enqueuing { claim_token }
            }
            (WakeDeliveryState::Enqueued, None, None) => WakeDeliveryLifecycle::Enqueued,
            (WakeDeliveryState::Discarded, None, Some(reason)) => {
                WakeDeliveryLifecycle::Discarded { reason }
            }
            (WakeDeliveryState::Enqueuing, None, _) => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{}` is enqueuing without a claim token",
                    self.delivery_id
                )));
            }
            (WakeDeliveryState::Discarded, _, None) => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{}` is discarded without a discard reason",
                    self.delivery_id
                )));
            }
            (
                WakeDeliveryState::Pending
                | WakeDeliveryState::Enqueued
                | WakeDeliveryState::Discarded,
                Some(_),
                _,
            ) => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{}` is {} with a claim token",
                    self.delivery_id,
                    state.as_str()
                )));
            }
            (
                WakeDeliveryState::Pending
                | WakeDeliveryState::Enqueuing
                | WakeDeliveryState::Enqueued,
                _,
                Some(_),
            ) => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{}` is {} with a discard reason",
                    self.delivery_id,
                    state.as_str()
                )));
            }
        };
        let wake = decode_process_wake_delivery(&self.delivery_json, fleet_format)?;
        // The row is keyed by the identity its wake computes. A key that
        // names any other wake is refused, never read as that delivery.
        let wake_id = wake.wake_id();
        if wake_id != self.delivery_id {
            return Err(PluginError::WakeDeliveryIdentityMismatch {
                delivery_id: self.delivery_id,
                wake_id: wake_id.into_inner(),
            });
        }
        Ok(WakeDelivery {
            wake,
            disposition,
            attempts: self.attempts as u64,
            first_attempt_ms: self.first_attempt_ms.map(|value| value as u64),
            next_attempt_at_ms: self.next_attempt_at_ms as u64,
            expires_at_ms: self.expires_at_ms as u64,
        })
    }
}
