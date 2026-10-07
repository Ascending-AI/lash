//! The relays of the obligation kinds a store set still arms (ADR 0109),
//! until L10b (FIG-5191) reduces the outbox to its two kinds (ADR 0132 §12).
//! Session ingress and control intents are not among them: they are session
//! mail, written with a wake in their producer's transaction and drained by
//! the session actor ([`crate::runtime::durable::session_mail`]).

mod interval;
mod lanes;
mod parent_end_relay;
mod reconcile;
pub mod relay;
mod relays;
pub mod scope_close;

pub use interval::{RECOVERY_TICK, RecoveryInterval};
pub use lanes::{LanesTick, RelayLanes};
pub use parent_end_relay::ParentEndRelay;
pub use reconcile::{ReconcileParts, reconcile_once};
pub use relays::{
    ObligationRelayUnavailable, RelayNeed, RelayParts, RelaySupply, obligation_relays,
};
pub use scope_close::{ScopeCloseRelay, deliver_scope_close};
