//! The artifact-cleanup outbox's relay (ADR 0109, ADR 0113 §2.5) and the
//! recovery tick's schedule. Every other kind is a mailbox write and a wake
//! in its producer's transaction (ADR 0132 §12).

mod interval;
pub mod relay;

pub use interval::{RECOVERY_TICK, RecoveryInterval};
