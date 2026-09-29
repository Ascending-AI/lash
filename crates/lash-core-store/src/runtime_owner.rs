//! Who a runtime runs for: a session, or a process named by its minted id.
//!
//! The type lives beside `SessionId` and `ProcessId` in `lash-sansio`,
//! because tool intents carry it as the authority they were declared under.

pub use lash_sansio::RuntimeOwner;
