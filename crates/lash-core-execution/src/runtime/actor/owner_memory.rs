//! The allocation admission seam shared by session, process and prompt owners.
//!
//! The ledger lives below the backend so every derived backend and activation
//! uses the same node budget. Tokens move with immutable allocations and drafts.
pub use lash_durable::owner_memory::{
    MemoryClass, MemoryReservation, MemoryUsage, NodeMemory, OwnerMemory, ReserveError,
};
