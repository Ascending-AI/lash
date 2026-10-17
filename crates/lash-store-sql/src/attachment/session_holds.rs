//! Why a session holds an attachment (FIG-6611).
//!
//! A session's edge in `attachment_referrer_edges` is one GC root per
//! attachment; a hold row names each reason the session keeps it: its
//! committed history (`history`, holder id empty), a pending input
//! (`input`, its input id) or a queued batch (`batch`, its batch id). A live
//! session's head only extends, so history holds last as long as the
//! session's graph. Ending an ingress hold queues its attachment in
//! `attachment_session_releases`, and the revision-release boundary drops
//! the session edge of each queued attachment no hold names any more, a
//! bounded page at a time.

/// The hold relation's unprefixed name.
pub const TABLE: &str = "attachment_session_holds";
/// The release queue's unprefixed name.
pub const RELEASES_TABLE: &str = "attachment_session_releases";

/// The holder kind of a session's committed history.
pub const HISTORY: &str = "history";
/// The holder kind of a pending input, named by its input id.
pub const INPUT: &str = "input";
/// The holder kind of a queued batch, named by its batch id.
pub const BATCH: &str = "batch";

crate::statements! {
    /// Hold rows and the release queue, as both backends issue them.
    pub struct SessionHoldStatements @ "attachment_session_hold" {
        /// Session `?1` holds attachment `?2` for holder `?3` `?4`.
        insert = "INSERT INTO attachment_session_holds (session_id, attachment_id, holder_kind, holder_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id, attachment_id, holder_kind, holder_id) DO NOTHING";

        /// Queue every attachment holder `?2` `?3` of session `?1` holds for
        /// the next release, before [`Self::delete_holder`] ends the holds.
        queue_holder = "INSERT INTO attachment_session_releases (session_id, attachment_id)
             SELECT session_id, attachment_id FROM attachment_session_holds
             WHERE session_id = ?1 AND holder_kind = ?2 AND holder_id = ?3
             ON CONFLICT (session_id, attachment_id) DO NOTHING";

        /// End holder `?2` `?3`'s holds in session `?1`.
        delete_holder = "DELETE FROM attachment_session_holds
             WHERE session_id = ?1 AND holder_kind = ?2 AND holder_id = ?3";

        /// Queue every attachment an input or batch of session `?1` (of every
        /// session when `?1` is NULL) holds whose body its revision release
        /// cleared, before [`Self::end_released_ingress`] ends those holds:
        /// the payload release's form of [`Self::queue_holder`] (FIG-6607).
        queue_released_ingress = "INSERT INTO attachment_session_releases (session_id, attachment_id)
             SELECT hold.session_id, hold.attachment_id FROM attachment_session_holds AS hold
             WHERE (hold.session_id = ?1 OR ?1 IS NULL)
               AND ((hold.holder_kind = 'input' AND EXISTS (
                       SELECT 1 FROM pending_turn_inputs AS released
                       WHERE released.session_id = hold.session_id
                         AND released.input_id = hold.holder_id
                         AND released.input_json IS NULL
                   ))
                 OR (hold.holder_kind = 'batch' AND EXISTS (
                       SELECT 1 FROM queued_work_batches AS released
                       WHERE released.session_id = hold.session_id
                         AND released.batch_id = hold.holder_id
                         AND released.payload_json IS NULL
                   )))
             ON CONFLICT (session_id, attachment_id) DO NOTHING";

        /// End the holds [`Self::queue_released_ingress`] queued.
        end_released_ingress = "DELETE FROM attachment_session_holds AS hold
             WHERE (hold.session_id = ?1 OR ?1 IS NULL)
               AND ((hold.holder_kind = 'input' AND EXISTS (
                       SELECT 1 FROM pending_turn_inputs AS released
                       WHERE released.session_id = hold.session_id
                         AND released.input_id = hold.holder_id
                         AND released.input_json IS NULL
                   ))
                 OR (hold.holder_kind = 'batch' AND EXISTS (
                       SELECT 1 FROM queued_work_batches AS released
                       WHERE released.session_id = hold.session_id
                         AND released.batch_id = hold.holder_id
                         AND released.payload_json IS NULL
                   )))";

        /// Whether an ingress holder of session `?1` holds attachment `?2`,
        /// and whether its history does.
        select_holders = "SELECT
                 EXISTS (SELECT 1 FROM attachment_session_holds
                         WHERE session_id = ?1 AND attachment_id = ?2 AND holder_kind <> 'history'),
                 EXISTS (SELECT 1 FROM attachment_session_holds
                         WHERE session_id = ?1 AND attachment_id = ?2 AND holder_kind = 'history')";

        /// Drop session `?1`'s edge to each of the first `?2` queued
        /// attachments that no hold names any more.
        release_edges = "DELETE FROM attachment_referrer_edges AS edge
             WHERE edge.referrer_kind = 'session' AND edge.referrer_id = ?1
               AND edge.attachment_id IN (
                   SELECT queued.attachment_id FROM attachment_session_releases AS queued
                   WHERE queued.session_id = ?1
                   ORDER BY queued.attachment_id LIMIT ?2
               )
               AND NOT EXISTS (
                   SELECT 1 FROM attachment_session_holds AS hold
                   WHERE hold.session_id = ?1 AND hold.attachment_id = edge.attachment_id
               )";

        /// Dequeue the page [`Self::release_edges`] decided.
        dequeue = "DELETE FROM attachment_session_releases
             WHERE session_id = ?1 AND attachment_id IN (
                 SELECT queued.attachment_id FROM attachment_session_releases AS queued
                 WHERE queued.session_id = ?1
                 ORDER BY queued.attachment_id LIMIT ?2
             )";

        /// The first `?1` sessions with queued releases.
        select_queued_sessions = "SELECT DISTINCT session_id FROM attachment_session_releases
             ORDER BY session_id LIMIT ?1";

        /// Session `?1`'s holds, when its referrer ends.
        delete_by_session = "DELETE FROM attachment_session_holds WHERE session_id = ?1";

        /// Session `?1`'s queued releases, when its referrer ends.
        delete_releases_by_session = "DELETE FROM attachment_session_releases WHERE session_id = ?1";
    }
}
