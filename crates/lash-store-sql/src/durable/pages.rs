//! Persistent-heap pages (FIG-6191, S6): the neutral statements of root
//! leases, the retirement frontier and semantic admissions.
//!
//! A root lease (`root_leases`) and a retirement hold (`retirement_holds`)
//! are rows of the root catalog: while one stands, nothing its root reaches
//! is peeled. A semantic admission (`semantic_admissions`) records which
//! validator certified a checkpoint root; it is no root owner and goes
//! with its root. This file and `pages.rs` in each dialect's durable module
//! issue their SQL.

/// `root_leases`: an activation's lease on the exact committed root it
/// reads on demand.
pub const LEASES_TABLE: &str = "root_leases";

/// `retirement_holds`: the durable retirement frontier.
pub const HOLDS_TABLE: &str = "retirement_holds";

/// `semantic_admissions`: a checkpoint root's validator, by root.
pub const ADMISSIONS_TABLE: &str = "semantic_admissions";

crate::statements! {
    /// Page statements both backends issue verbatim.
    pub struct PageStatements @ "durable_pages" {
        /// Lease root `?2` of the owner encoded `?1` to actor `?3` at epoch
        /// `?4` for `?5` ms from `?6`; answers the lease's id.
        lease_insert = "INSERT INTO root_leases (owner_key, root_ref, actor_key, epoch, ttl_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6 + ?5)
             RETURNING lease_id";

        /// Extend lease `?1` on root `?2` at epoch `?3` by its ttl from
        /// `?4`, unless it expired by `?4`; answers its new expiry.
        lease_renew = "UPDATE root_leases SET expires_at_ms = ?4 + ttl_ms
             WHERE lease_id = ?1 AND root_ref = ?2 AND epoch = ?3 AND expires_at_ms > ?4
             RETURNING expires_at_ms";

        /// Lease `?1` on root `?2`, unless it expired by `?3`.
        lease_live = "SELECT root_ref FROM root_leases
             WHERE lease_id = ?1 AND root_ref = ?2 AND expires_at_ms > ?3";

        /// End lease `?1`; answers the root it held.
        lease_release = "DELETE FROM root_leases WHERE lease_id = ?1 RETURNING root_ref";

        /// End lease `?1` if it expired by `?2`; answers the root it held.
        lease_reap = "DELETE FROM root_leases WHERE lease_id = ?1 AND expires_at_ms <= ?2
             RETURNING root_ref";

        /// Remove the hold on `?1`; answers whether it held a closure.
        hold_take = "DELETE FROM retirement_holds WHERE root_ref = ?1 RETURNING closure";

        /// How many holds stand and when the oldest was queued.
        backlog = "SELECT count(*), min(queued_at_ms) FROM retirement_holds";

        /// Record root `?1`'s semantic admission: kernel `?2`, page format
        /// `?3`, validator `?4`, by commit `?5`. A later publication of the
        /// same root records its own validator.
        admission_put = "INSERT INTO semantic_admissions (root_ref, kernel, page_format, validator, commit_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (root_ref) DO UPDATE SET kernel = excluded.kernel,
                 page_format = excluded.page_format, validator = excluded.validator,
                 commit_id = excluded.commit_id";

        /// Root `?1`'s semantic admission.
        admission_get = "SELECT kernel, page_format, validator, commit_id
             FROM semantic_admissions WHERE root_ref = ?1";
    }
}
