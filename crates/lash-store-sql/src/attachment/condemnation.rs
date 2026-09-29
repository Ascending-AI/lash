//! `attachment_condemnations`: the attachment GC fence, one row per condemned
//! digest.
//!
//! Deliberately timestampless: the protocol is compare-and-swap transitions
//! only (`lash_core::AttachmentCondemnation`), never an expiry. A row in
//! `condemned` with no `write_token` is sweep-owned; a row in `condemned`
//! carrying a token has been claimed by a restoring writer; a row in
//! `deleting` authorizes the physical delete and nothing may revoke it.
//!
//! `sweep_generation` names the sweep pass that owns the row. Every sweep
//! transition is conditional on it, so a pass moves only its own rows, and a
//! later pass adopts a row only by compare-and-swapping the generation it
//! read (ADR 0067 §6). `delete_attempts`, `last_delete_error` and
//! `stall_reason` record failed physical deletes; a stalled row is never
//! adopted again.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_condemnations";

crate::statements! {
    /// `attachment_condemnations` statements both backends issue verbatim.
    pub struct CondemnationStatements @ "attachment_condemnation" {
        /// Every condemnation, as the durable authority reports it.
        select_all = "SELECT attachment_id, phase, write_token, write_session_id,
                 delete_attempts, last_delete_error, stall_reason
             FROM attachment_condemnations";

        /// Every sweep-owned row a generation older than `?1` left, as
        /// adoption reads it before claiming each one.
        select_adoptable = "SELECT attachment_id, sweep_generation, phase, delete_attempts,
                 stall_reason
             FROM attachment_condemnations
             WHERE write_token IS NULL AND sweep_generation < ?1
             ORDER BY attachment_id";

        /// Adopt `?1` for generation `?2`, if it still carries the older
        /// generation `?3` adoption read, no writer holds it, and its delete
        /// is not stalled. Zero rows means a peer adopted it first.
        adopt = "UPDATE attachment_condemnations SET sweep_generation = ?2
             WHERE attachment_id = ?1 AND sweep_generation = ?3
               AND write_token IS NULL AND stall_reason IS NULL";

        /// The phase of `?1` and whether a writer already holds it.
        select_phase_and_claim = "SELECT phase, write_token FROM attachment_condemnations
             WHERE attachment_id = ?1";

        /// The restoring writer's claim on `?1`, if one exists.
        select_claim = "SELECT write_token, write_session_id
             FROM attachment_condemnations
             WHERE attachment_id = ?1
               AND phase = 'condemned'
               AND write_token IS NOT NULL";

        /// Own the condemnation of `?1` with attempt `?2` from session `?3`,
        /// if it is still unclaimed. Zero rows means a peer won the claim.
        claim_write = "UPDATE attachment_condemnations
             SET write_token = ?2, write_session_id = ?3
             WHERE attachment_id = ?1
               AND phase = 'condemned'
               AND write_token IS NULL";

        clear_write_claim = "UPDATE attachment_condemnations
             SET write_token = NULL, write_session_id = NULL
             WHERE attachment_id = ?1 AND write_token = ?2";

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
             WHERE attachment_id = ?1 AND write_token = ?2
               AND phase = 'condemned'
               AND EXISTS (
                   SELECT 1 FROM attachment_manifest
                    WHERE attachment_id = ?1 AND session_id = ?3
                      AND committed_at_ms IS NOT NULL
               )";

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
        /// attempt, stalled with reason `?4` when that is not null.
        record_failed_delete = "UPDATE attachment_condemnations
             SET phase = 'condemned', delete_attempts = delete_attempts + 1,
                 last_delete_error = ?3, stall_reason = ?4
             WHERE attachment_id = ?1 AND sweep_generation = ?2 AND phase = 'deleting'";
    }
}
