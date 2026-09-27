//! The recovery leader lease (ADR 0109 §1.6).
//!
//! One row per engine authority in the storage names the deployment that runs
//! the leader-only recovery duties. It is load control, never a fence: every
//! duty stays idempotent under two overlapping leaders (ADR 0080). Every
//! comparison runs on the database clock, read inside the statement's own
//! transaction, so hosts with skewed clocks agree on expiry.

use super::StoreError;

/// The lease a deployment competes for: one per engine authority.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeaseName(String);

impl LeaseName {
    /// The lease named `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One process incarnation competing for a lease: fresh per process, so a
/// restarted host never inherits its predecessor's term.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HolderId(String);

impl HolderId {
    /// The holder stored as `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// A fresh holder for this process incarnation.
    #[must_use]
    pub fn mint() -> Self {
        Self(format!("lash:{}", uuid::Uuid::new_v4().simple()))
    }

    /// The holder as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What one acquire or renew asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseClaim {
    pub name: LeaseName,
    pub holder: HolderId,
    /// Host-supplied build rank: a higher rank preempts a lower one once the
    /// lower holder has led for `min_tenure_ms`.
    pub generation_rank: i64,
    pub ttl_ms: u64,
    pub min_tenure_ms: u64,
}

/// The lease row as the database holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRow {
    pub holder: HolderId,
    pub generation_rank: i64,
    pub term: i64,
    pub elected_at_ms: i64,
    pub expires_at_ms: i64,
}

/// The answer to an acquire or renew.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseAnswer {
    /// Whether the claimant leads now.
    pub leader: bool,
    /// The row after the attempt; `None` when nobody holds the lease.
    pub row: Option<LeaseRow>,
    /// The database clock the attempt compared against.
    pub db_now_ms: i64,
}

/// The storage half of the recovery leader lease.
#[async_trait::async_trait]
pub trait RecoveryLeaderStore: Send + Sync {
    /// Take the lease if nobody holds it, its holder expired, or its holder
    /// has a lower rank and led for at least `min_tenure_ms`. A holder that
    /// changes bumps the term. Leader iff the row's holder is the claimant
    /// after the attempt.
    async fn acquire(&self, claim: &LeaseClaim) -> Result<LeaseAnswer, StoreError>;

    /// Extend the claimant's unexpired lease of `term` by `ttl_ms`. Not
    /// leader when the term was lost, taken over or expired.
    async fn renew(&self, claim: &LeaseClaim, term: i64) -> Result<LeaseAnswer, StoreError>;

    /// Give up the claimant's unexpired lease of `term`, so a follower takes
    /// over at its next attempt rather than after the TTL. `false` when the
    /// claimant no longer held it.
    async fn resign(
        &self,
        name: &LeaseName,
        holder: &HolderId,
        term: i64,
    ) -> Result<bool, StoreError>;

    /// Whether due-obligation claims are leader-only on this storage: true
    /// where the database has one writer (SQLite), false where claims skip
    /// each other's locked rows (PostgreSQL).
    fn due_claims_need_leader(&self) -> bool;
}
