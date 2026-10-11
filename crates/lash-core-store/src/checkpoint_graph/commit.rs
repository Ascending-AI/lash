//! Commit identity, budgets and measurements (FIG-5887 §5 and §9).
//!
//! A commit's admission is measured over its whole canonical encoding: every
//! supplied node, root key, expected revision, admission, namespace body,
//! prune key and edge row it writes, not only the new root. The
//! measured total plus the reserved failure capacity must fit the host's
//! explicit budget. There is no implicit default budget.

use std::fmt;

use super::node::NodeId;
use super::refusal::GraphRefusal;

/// The identity of an execution snapshot's publication, retained in its
/// root row with the owner's epoch and revision.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitId(pub [u8; 32]);

impl CommitId {
    /// A fresh random identity.
    #[must_use]
    pub fn mint() -> Self {
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(bytes)
    }
}

impl fmt::Debug for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitId({})", NodeId(self.0).to_hex())
    }
}

/// A limit a host chose. It has no default: every budget names its numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExplicitLimit(u64);

impl ExplicitLimit {
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self(limit)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An encoder's memory lease: every staged byte is charged before it is
/// held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeBudget {
    lease: ExplicitLimit,
    spent: u64,
}

impl EncodeBudget {
    #[must_use]
    pub const fn new(lease: ExplicitLimit) -> Self {
        Self { lease, spent: 0 }
    }

    /// Charges `bytes`, refusing when the lease would be exceeded. A refused
    /// charge spends nothing.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::EncodeMemoryLimit`].
    pub fn charge(&mut self, bytes: u64) -> Result<(), GraphRefusal> {
        let spent = self
            .spent
            .checked_add(bytes)
            .filter(|spent| *spent <= self.lease.get())
            .ok_or(GraphRefusal::EncodeMemoryLimit)?;
        self.spent = spent;
        Ok(())
    }

    #[must_use]
    pub const fn spent(&self) -> u64 {
        self.spent
    }

    #[must_use]
    pub const fn lease(&self) -> ExplicitLimit {
        self.lease
    }
}

/// A commit's explicit budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphCommitBudget {
    /// The full encoded commit's bytes, failure reserve included.
    pub encoded_bytes: ExplicitLimit,
    /// Planned inserted, updated and deleted SQL rows.
    pub mutation_rows: ExplicitLimit,
    /// The encoder's memory lease.
    pub encode_memory: ExplicitLimit,
    /// Capacity reserved for recording a bounded failure (an oversized
    /// result, host summary or namespace) instead of this commit.
    pub failure_reserve_bytes: u64,
}

impl GraphCommitBudget {
    /// Admits a commit: its full encoding plus the failure reserve must fit
    /// `encoded_bytes`, and its rows `mutation_rows`. Equality is admitted.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::CommitTooLarge`] or [`GraphRefusal::TooManyMutations`].
    pub fn admit(&self, measured: &EncodedCommitMeasurement) -> Result<(), GraphRefusal> {
        let bytes = measured
            .full_encoded_commit_bytes
            .saturating_add(self.failure_reserve_bytes);
        self.check(bytes, measured.planned_mutation_rows)
    }

    /// Admits the reserved failure commit, which may spend the reserve but
    /// not exceed the same total.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::CommitTooLarge`] or [`GraphRefusal::TooManyMutations`].
    pub fn admit_reserved_failure(
        &self,
        measured: &EncodedCommitMeasurement,
    ) -> Result<(), GraphRefusal> {
        self.check(
            measured.full_encoded_commit_bytes,
            measured.planned_mutation_rows,
        )
    }

    fn check(&self, bytes: u64, rows: u64) -> Result<(), GraphRefusal> {
        if bytes > self.encoded_bytes.get() {
            return Err(GraphRefusal::CommitTooLarge {
                bytes,
                limit: self.encoded_bytes.get(),
            });
        }
        if rows > self.mutation_rows.get() {
            return Err(GraphRefusal::TooManyMutations {
                rows,
                limit: self.mutation_rows.get(),
            });
        }
        Ok(())
    }
}

/// What a prepared commit measures before it is admitted.
///
/// Supplied duplicate nodes count once; supplied nodes already installed
/// still count (their encoding was spent). A reference to an existing node
/// counts its reference and row metadata, never its body again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodedCommitMeasurement {
    /// Unique supplied node envelopes' bytes.
    pub supplied_node_bytes: u64,
    /// The canonical encoding of every non-node mutation.
    pub mutation_encoding_bytes: u64,
    /// The whole commit's canonical encoding.
    pub full_encoded_commit_bytes: u64,
    /// Every planned inserted, updated and deleted SQL row, blob, edge and
    /// receipt rows included; distinct from `CommitBudget.nodes`.
    pub planned_mutation_rows: u64,
}

/// What a successful commit installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstalledGraphMeasurement {
    /// Envelope bytes of nodes absent before this commit and installed by
    /// it. A rollback installs zero.
    pub newly_installed_canonical_bytes: u64,
    pub new_nodes: u64,
    pub new_edges: u64,
    /// Logical row bytes written: text as UTF-8, blobs raw, each integer 8,
    /// bool 1, NULL 0, each column once per row, delete keys separately; no
    /// protocol, header, index or page bytes. Installed payload is a
    /// component, never added twice.
    pub sql_logical_bytes: u64,
}
