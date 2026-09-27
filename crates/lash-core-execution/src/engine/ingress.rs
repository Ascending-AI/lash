//! The drive an admitted ingress row owes its session (ADR 0109 §3,
//! FIG-3851).

use super::DriveRequestId;

/// The drive the ingress row with item id `item_id` — a turn input's id or a
/// queued batch's id — asks its session for: `ingress:{item_id}`.
///
/// One request per row: the engine dedupes a redelivery of the same row, and
/// two rows never share an ask a finishing drive could swallow. A waiter on
/// an input attaches to the same request.
#[must_use]
pub fn ingress_drive_request(item_id: &str) -> DriveRequestId {
    DriveRequestId::new(format!("ingress:{item_id}"))
}
