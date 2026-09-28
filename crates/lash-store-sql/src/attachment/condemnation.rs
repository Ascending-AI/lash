//! `attachment_condemnations`: the attachment GC fence, one row per condemned
//! digest.
//!
//! Deliberately timestampless: the protocol is compare-and-swap transitions
//! only (`lash_core::AttachmentCondemnation`), never an expiry. A row in
//! `condemned` with no `write_token` is sweep-owned; a row in `condemned`
//! carrying a token has been claimed by a restoring writer; a row in
//! `deleting` authorizes the physical delete and nothing may revoke it.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_condemnations";

crate::statements! {
    /// `attachment_condemnations` statements both backends issue verbatim.
    pub struct CondemnationStatements @ "attachment_condemnation" {
        /// Every condemnation, as the durable authority reports it.
        select_all = "SELECT attachment_id, phase, write_token, write_session_id
             FROM attachment_condemnations";

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

        /// `Condemned -> Deleting` for `?1`: the compare-and-swap that
        /// authorizes the physical delete. A writer that revoked the
        /// condemnation removed the row, so this matches nothing and the
        /// delete is never issued.
        arm_delete = "UPDATE attachment_condemnations SET phase = 'deleting'
             WHERE attachment_id = ?1 AND phase = 'condemned' AND write_token IS NULL";

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

        /// A stale sweep cannot clear a restoring writer's token, which is why the token
        /// predicate rides on the `condemned` arm only.
        delete_sweep_owned = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1
               AND (phase = 'deleting'
                    OR (phase = 'condemned' AND write_token IS NULL))";

        /// Retire `?1`'s armed condemnation once the physical delete has
        /// succeeded. The digest returns to `Free` holding no upload evidence,
        /// because condemning it already cleared every manifest row.
        delete_armed = "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND phase = 'deleting'";
    }
}
