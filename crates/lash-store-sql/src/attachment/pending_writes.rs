//! One independently fenced attempt per write id.
pub const TABLE: &str = "attachment_pending_writes";
crate::statements! {
    pub struct PendingWriteStatements @ "attachment_pending_write" {
        insert = "INSERT INTO attachment_pending_writes (write_id, attachment_id, referrer_kind, referrer_id, begun_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)";
        select_permit = "SELECT 1 FROM attachment_pending_writes WHERE write_id = ?1 AND attachment_id = ?2 AND referrer_kind = ?3 AND referrer_id = ?4";
        delete_permit = "DELETE FROM attachment_pending_writes WHERE write_id = ?1 AND attachment_id = ?2 AND referrer_kind = ?3 AND referrer_id = ?4";
        delete_referrer = "DELETE FROM attachment_pending_writes WHERE referrer_kind = ?1 AND referrer_id = ?2";
        select_referrer_digests = "SELECT DISTINCT attachment_id FROM attachment_pending_writes WHERE referrer_kind = ?1 AND referrer_id = ?2 ORDER BY attachment_id";
    }
}
