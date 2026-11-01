//! Persistent-heap pages (FIG-6191, S6): the neutral statements of root
//! leases and the retirement frontier.
//!
//! A root lease (`root_leases`) and a retirement hold (`retirement_holds`)
//! are rows of the root catalog: while one stands, nothing its root reaches
//! is peeled. A retirement step's unfinished frontier
//! (`retirement_frontier`) is the steps' own: no root of the catalog, and
//! no reference a node must outlive. This file and `pages.rs` in each
//! dialect's durable module issue their SQL.

/// `root_leases`: an activation's lease on the exact committed root it
/// reads on demand.
pub const LEASES_TABLE: &str = "root_leases";

/// `retirement_holds`: what publications and lease releases retired.
pub const HOLDS_TABLE: &str = "retirement_holds";

/// `retirement_frontier`: what retirement steps have yet to peel.
pub const FRONTIER_TABLE: &str = "retirement_frontier";

crate::statements! {
    /// Page statements both backends issue verbatim.
    pub struct PageStatements @ "durable_pages" {
        /// Lease root `?2` of the owner encoded `?1` to actor `?3` at epoch
        /// `?4` (both null for a reader) for `?5` ms from `?6`; answers the
        /// lease's id.
        lease_insert = "INSERT INTO root_leases (owner_key, root_ref, actor_key, epoch, ttl_ms, expires_at_ms)
             VALUES (?1, COALESCE((SELECT id FROM blobs WHERE hash = ?2), CASE WHEN ?2 IS NOT NULL THEN -1 END), ?3, ?4, ?5, ?6 + ?5)
             RETURNING lease_id";

        /// Extend lease `?1` on root `?2` at epoch `?3` (null for a
        /// reader's) by its ttl from `?4`, unless it expired by `?4`;
        /// answers its new expiry.
        lease_renew = "UPDATE root_leases SET expires_at_ms = ?4 + ttl_ms
             WHERE lease_id = ?1 AND root_ref = COALESCE((SELECT id FROM blobs WHERE hash = ?2), CASE WHEN ?2 IS NOT NULL THEN -1 END)
                 AND (epoch = ?3 OR (epoch IS NULL AND ?3 IS NULL)) AND expires_at_ms > ?4
             RETURNING expires_at_ms";

        /// End lease `?1`; answers the digest of the root it held.
        lease_release = "DELETE FROM root_leases WHERE lease_id = ?1
             RETURNING (SELECT hash FROM blobs WHERE id = root_ref) AS root_ref";

        /// Remove the hold on node id `?1`; answers whether it held a closure.
        hold_take = "DELETE FROM retirement_holds WHERE root_ref = ?1 RETURNING closure";

        /// How many holds and frontier nodes wait and when the oldest was
        /// queued.
        backlog = "SELECT count(*), min(queued.queued_at_ms) FROM (
                 SELECT queued_at_ms FROM retirement_holds
                 UNION ALL SELECT queued_at_ms FROM retirement_frontier
             ) AS queued";
    }
}
