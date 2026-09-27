//! `recovery_leader`: one row per engine authority naming the deployment that
//! runs the leader-only recovery duties (ADR 0109 §1.6).
//!
//! Every statement compares against a database instant the caller read in the
//! same transaction (bound as `?`), so hosts with skewed clocks agree on
//! expiry. The acquire is one upsert: it inserts the first holder, and replaces
//! an expired holder or, after its minimum tenure, a lower-ranked one; a
//! holder change bumps the term.

/// The table's unprefixed name.
pub const TABLE: &str = "recovery_leader";

/// The lease row as a reader decodes it, by position.
pub const ROW_COLUMNS: &str = "holder_id, generation_rank, term, elected_at_ms, expires_at_ms";

crate::statements! {
    /// `recovery_leader` statements both backends issue verbatim.
    pub struct RecoveryLeaderStatements @ "recovery_leader" {
        /// Take lease `?1` for holder `?2` at rank `?3` and instant `?4` with
        /// TTL `?5`, if nobody holds it, its holder expired, or its holder
        /// ranks lower and was elected more than `?6` before `?4`. Returns the
        /// row only when the holder changed.
        acquire = "INSERT INTO recovery_leader AS l
                 (name, holder_id, generation_rank, term, elected_at_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, 1, ?4, ?4 + ?5)
             ON CONFLICT (name) DO UPDATE
                 SET holder_id = excluded.holder_id,
                     generation_rank = excluded.generation_rank,
                     term = l.term + 1,
                     elected_at_ms = excluded.elected_at_ms,
                     expires_at_ms = excluded.expires_at_ms
                 WHERE l.expires_at_ms < ?4
                    OR (l.generation_rank < excluded.generation_rank
                        AND l.elected_at_ms + ?6 < ?4)
             RETURNING holder_id, generation_rank, term, elected_at_ms, expires_at_ms";

        /// Extend holder `?2`'s unexpired lease `?1` of term `?3` to `?4 + ?5`.
        renew = "UPDATE recovery_leader SET expires_at_ms = ?4 + ?5
             WHERE name = ?1 AND holder_id = ?2 AND term = ?3 AND expires_at_ms >= ?4
             RETURNING holder_id, generation_rank, term, elected_at_ms, expires_at_ms";

        /// Give up holder `?2`'s unexpired lease `?1` of term `?3` at `?4`:
        /// expire it now, so the next claimant takes it with the term bumped.
        resign = "UPDATE recovery_leader SET expires_at_ms = ?4 - 1
             WHERE name = ?1 AND holder_id = ?2 AND term = ?3 AND expires_at_ms >= ?4";

        /// Lease `?1`'s row.
        select = "SELECT holder_id, generation_rank, term, elected_at_ms, expires_at_ms
             FROM recovery_leader WHERE name = ?1";
    }
}
