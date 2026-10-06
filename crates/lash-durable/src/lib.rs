//! The lash-owned durability port (ruling #74).
//!
//! Every session and every process is an **actor** with one scheduling row.
//! A serving lash process is a **node** with one heartbeat row. A node owns
//! the actors it claimed, and its ownership of each is an **epoch**: claim,
//! reap and release all bump it, and every owner write transaction checks it
//! first, inside the transaction. A stale or reaped owner therefore can never
//! commit, whatever its own clock or lease believes.
//!
//! The port has two write authorities, and they are distinct types:
//!
//! - an [`ActorTx`] is the owner's: it writes the actor's own state (mail
//!   acknowledgement, release with a due time), fenced on its epoch;
//! - a [`MailTx`] is everyone else's: it creates actors, appends mail and
//!   wakes. It has no owner-state writer, so a non-owner cannot reach one.
//!
//! Both are buffered write sets. [`DurableStore::commit`] and
//! [`DurableStore::commit_mail`] apply one atomically, each under a
//! [`CommitLabel`]: the seam a fault-injecting harness wraps.
//!
//! Durable instants come from the database side: the database clock on
//! PostgreSQL, the store's injected clock on SQLite. No instant a caller
//! computes is ever stored.
//!
//! Runtime lanes add their own rows to these commits through [`domain`]:
//! [`DomainWrite`]s on an owner commit, [`MailDomainWrite`]s on a mailbox
//! commit, and [`DurableReads`] to read them back.
//!
//! The SQL lives in one module per dialect, in the store crates that
//! implement [`DurableStore`]; this crate holds none.

mod config;
mod dispatch;
pub mod domain;
mod dues;
mod durable_config;
mod error;
mod ids;
mod labels;
#[cfg(feature = "testing")]
pub mod laws;
mod port;
mod probe;
pub mod runner;
mod tx;

pub use config::{LeaseConfig, LeaseConfigError, LeaseSettings};
pub use dispatch::ActorDispatch;
pub use domain::{DomainRefusal, DomainWrite, DurableReads, MailAnswer, MailDomainWrite};
pub use dues::{DueSource, Dues};
pub use durable_config::{
    DurableConfig, DurableConfigError, DurableSettings, GroupCommit, Notifier,
};
pub use error::{DurableError, Fenced, MailRefusal, StoreFailure, StoreFailureKind};
pub use ids::{
    ActorKey, ActorKeyError, ActorKind, BootId, CommitLabel, DurableInstant, Epoch, FormatSet,
    MailKind, MailSeq, NodeId, StateRevision,
};
pub use port::{
    ActorCommit, ActorSnapshot, ActorState, ClaimCause, Claimed, DurableStore, HeartbeatOutcome,
    MailCommit, NodeLease, NodeSpec, Owner, Reaped, Woken,
};
pub use probe::{DurableProbe, NoProbe};
pub use tx::{ActorTx, Mail, MailTx, MailWrite, OpenedActor, Release};
