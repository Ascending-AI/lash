//! The drive an admitted ingress row owes its session (ADR 0109 §3,
//! FIG-3851).

use super::DriveRequestId;

/// The attempt of an ingress row's first ask: the producer's immediate
/// delivery, and a waiter on the row, which attaches to that same drive.
pub const FIRST_INGRESS_ATTEMPT: u32 = 1;

/// The drive the `attempt`th claim of the ingress row with item id `item_id`
/// — a turn input's id or a queued batch's id — asks its session for:
/// `ingress:{item_id}:{attempt}`, the row's obligation id qualified by the
/// attempt (ADR 0109 §3).
///
/// One request per row and attempt: the engine dedupes a redelivery within
/// an attempt, and two rows never share an ask a finishing drive could
/// swallow. The attempt is part of the key because the engine also dedupes
/// against an invocation it lost: a claim that lapsed because nothing
/// admitted the row is asked again under the next attempt, never under a key
/// the engine would answer from the lost invocation.
#[must_use]
pub fn ingress_drive_request(item_id: &str, attempt: u32) -> DriveRequestId {
    DriveRequestId::new(format!("ingress:{item_id}:{attempt}"))
}
