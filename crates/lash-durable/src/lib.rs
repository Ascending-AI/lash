//! The lash-owned durability port (ruling #74).
//!
//! Recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
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

#![deny(clippy::future_not_send)]

mod config;
mod dispatch;
pub mod domain;
mod dues;
mod durable_config;
mod error;
mod formats;
mod ids;
mod labels;
pub use labels::CommitCapacity;
#[cfg(feature = "testing")]
pub mod laws;
pub mod node_wake_payload;
mod node_wakes;
pub mod packages;
mod port;
mod probe;
mod retirement;
pub mod runner;
mod tx;

pub use config::{LeaseConfig, LeaseConfigError, LeaseSettings};
pub use dispatch::ActorDispatch;
pub use domain::{DomainRefusal, DomainWrite, DurableReads, MailAnswer, MailDomainWrite};
pub use dues::{DueSource, Dues};
pub use durable_config::{
    DurableConfig, DurableConfigError, DurableSettings, GroupCommit, Notifier, RetirementSettings,
};
pub use error::{DurableError, Fenced, MailRefusal, StoreFailure, StoreFailureKind};
pub use formats::{FormatSet, FormatSurface, fleet_writable};
pub use ids::{
    ActorKey, ActorKeyError, ActorKind, BootId, CommitLabel, DurableInstant, Epoch, MailKind,
    MailSeq, NodeId, StateRevision,
};
pub use node_wakes::{BootLiveness, NodeWakeEvent, NodeWakeFeed, NodeWakes, WakeBatch};
pub use packages::{
    ArtifactDigest, ExecutionRequirements, NodeCapabilities, NodeCapability, PackageArtifact,
    PackageCatalog, PackageSetDigest, StoredSet,
};
pub use port::{
    ActorCommit, ActorSnapshot, ActorState, ClaimActorFacts, ClaimCause, ClaimFacts,
    ClaimNodeFacts, ClaimPurpose, Claimed, DurableStore, HeartbeatOutcome, MailCommit, NodeLease,
    NodeSpec, Owner, Readiness, Reaped, Woken, encode_requires, parse_requires,
};
pub use probe::{DurableProbe, NoProbe, OwnerCut, OwnerRead};
pub use tx::{ActorTx, Mail, MailTx, MailWrite, OpenedActor, Release};
