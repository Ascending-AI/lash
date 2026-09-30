//! `attachment_condemnations`: the attachment GC fence, one row per condemned
//! digest.
//!
//! Ownership uses compare-and-swap transitions
//! (`lash_core::AttachmentCondemnation`), never an expiry. A row in
//! `condemned` with no `write_token` is sweep-owned; a row in `condemned`
//! carrying a token has been claimed by a restoring writer; a row in
//! `deleting` authorizes the physical delete and nothing may revoke it.
//!
//! `sweep_generation` names the sweep pass that owns the row. Every sweep
//! transition is conditional on it, so a pass moves only its own rows, and a
//! later pass adopts a row only by compare-and-swapping the generation it
//! read (ADR 0067 §6). `delete_attempts`, `last_delete_error` and
//! `stall_reason` record failed physical deletes. `next_delete_at_ms` paces
//! retries on the store clock, including stalled rows; it never proves an
//! owner dead or retires a row.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_condemnations";

crate::statements! {
    /// `attachment_condemnations` statements both backends issue verbatim.
    pub struct CondemnationStatements @ "attachment_condemnation" {
        /// Every condemnation, as the durable authority reports it.
        select_all = "SELECT condemnation.attachment_id, condemnation.phase, condemnation.write_token, pending.referrer_kind, pending.referrer_id,
                 condemnation.delete_attempts, condemnation.last_delete_error, condemnation.stall_reason
             FROM attachment_condemnations AS condemnation LEFT JOIN attachment_pending_writes AS pending ON pending.write_id = condemnation.write_token";

        /// Every sweep-owned row a generation older than `?1` left, as
        /// adoption reads it before claiming each one.
        select_adoptable = "SELECT attachment_id, sweep_generation, phase, delete_attempts,
                 stall_reason, next_delete_at_ms
             FROM attachment_condemnations
             WHERE write_token IS NULL AND sweep_generation < ?1
             ORDER BY attachment_id";

        /// Adopt `?1` for generation `?2`, if it still carries the older
        /// generation `?3` adoption read, no writer holds it, and its retry
        /// is due at store time `?4`. An interrupted armed delete is always due.
        /// Zero rows means a peer adopted it first.
        adopt = "UPDATE attachment_condemnations SET sweep_generation = ?2
             WHERE attachment_id = ?1 AND sweep_generation = ?3
               AND write_token IS NULL
               AND (phase = 'deleting' OR next_delete_at_ms <= ?4)";

        /// The phase of `?1` and whether a writer already holds it.
        select_phase_and_claim = "SELECT phase, write_token FROM attachment_condemnations
             WHERE attachment_id = ?1";

        /// The restoring writer's claim on `?1`, if one exists.
        select_claim = "SELECT pending.write_id, pending.referrer_kind, pending.referrer_id
             FROM attachment_condemnations AS condemnation JOIN attachment_pending_writes AS pending ON pending.write_id = condemnation.write_token
             WHERE condemnation.attachment_id = ?1 AND condemnation.phase = 'condemned'";
        claim_write = "UPDATE attachment_condemnations SET write_token = ?2
             WHERE attachment_id = ?1 AND phase = 'condemned' AND write_token IS NULL";

        /// `Condemned -> Deleting` for `?1` under generation `?2`: the
        /// compare-and-swap that authorizes the physical delete. A writer
        /// that revoked or claimed the condemnation, or a pass that adopted
        /// it, leaves nothing for this to match, and the delete is never
        /// issued.
        arm_delete = "UPDATE attachment_condemnations SET phase = 'deleting'
             WHERE attachment_id = ?1 AND sweep_generation = ?2
               AND phase = 'condemned' AND write_token IS NULL";

        /// Retire the condemnation of `?1` with attempt `?2`'s claim: the
        /// bytes exist now, so the claim is released together with the
        /// condemnation it was holding open.
        delete_by_write_token = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND write_token = ?2";

        /// Retire attempt `?2`'s claimed condemnation of `?1` when session
        /// `?3` committed the digest anyway: the commitment supersedes the
        /// condemnation, so the row goes rather than reverting to sweep-owned.
        delete_superseded_claim = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND write_token = ?2 AND phase = 'condemned'
               AND (EXISTS (SELECT 1 FROM attachment_referrer_edges WHERE attachment_id = ?1 AND (referrer_kind <> ?3 OR referrer_id <> ?4))
                 OR EXISTS (SELECT 1 FROM attachment_pending_writes WHERE attachment_id = ?1 AND write_id <> ?2))";

        /// A fresh committed root supersedes an unarmed, unclaimed
        /// condemnation of `?1`. A restoring writer's claim is left for that
        /// writer to settle.
        delete_unclaimed_condemned = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND phase = 'condemned' AND write_token IS NULL";

        /// Generation `?2` gives `?1` back without deleting it. A restoring
        /// writer's token is never cleared here, which is why the token
        /// predicate rides on the `condemned` arm only.
        delete_spared = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND sweep_generation = ?2
               AND (phase = 'deleting'
                    OR (phase = 'condemned' AND write_token IS NULL))";

        /// Retire generation `?2`'s armed condemnation of `?1` once the bytes
        /// are gone. The digest returns to `Free` holding no upload evidence,
        /// because condemning it already cleared every manifest row.
        delete_armed = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND sweep_generation = ?2 AND phase = 'deleting'";

        /// Generation `?2`'s delete of `?1` failed with `?3`: back to
        /// `condemned`, where a writer may reclaim it, with one more failed
        /// attempt, preserving an existing stall or recording reason `?4`.
        /// Store time `?5` starts the delay: 1 second doubled per failure,
        /// capped at 15 minutes, matching the obligation relay defaults.
        /// The shift uses the old attempt count, before this failure. Counts
        /// saturate at the shared PostgreSQL INTEGER bound.
        record_failed_delete = "UPDATE attachment_condemnations
             SET phase = 'condemned',
                 delete_attempts = CASE WHEN delete_attempts < 2147483647
                     THEN delete_attempts + 1 ELSE delete_attempts END,
                 last_delete_error = ?3, stall_reason = COALESCE(stall_reason, ?4),
                 next_delete_at_ms = ?5 + CASE WHEN delete_attempts >= 10
                     THEN 900000 ELSE (1000 << delete_attempts) END
             WHERE attachment_id = ?1 AND sweep_generation = ?2 AND phase = 'deleting'";
    }
}
