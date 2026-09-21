//! The trigger family: durable subscriptions, the occurrences that fire them,
//! the deliveries a firing reserves, and the receipts that make a mutation
//! replayable.
//!
//! Four tables — [`subscriptions`], [`occurrences`], [`deliveries`] and
//! [`mutation_receipts`] — and no family-wide shared statement. The one read
//! that spans all four (the retention sweep's session-owner enumeration) forks
//! on JSON extraction in both backends, so it is declared twice under the
//! `trigger_retention` prefix and manifested; the column lists it projects
//! still live with their tables.
//!
//! # Where the row types are
//!
//! Every row of this family stores the durable record as `record_json` (a
//! subscription, an occurrence) or `subscription_snapshot_json` (a delivery's
//! frozen copy of one), and every read decodes it into the `lash_core` port
//! type the store's trait already returns — `TriggerSubscriptionRecord`,
//! `TriggerOccurrenceRecord`, `TriggerDeliveryReservation`. Per
//! `docs/store-sql-authoring.md`, a row that is already a port type of the
//! driver that consumes it keeps its definition there and this crate owns only
//! the column order, so these modules declare column lists and statements and
//! no row struct: one owner per fact, not two.

pub mod deliveries;
pub mod mutation_receipts;
pub mod occurrences;
pub mod subscriptions;
