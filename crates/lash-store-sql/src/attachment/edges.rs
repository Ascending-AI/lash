//! Pure attachment liveness edges.
pub const TABLE: &str = "attachment_referrer_edges";
crate::statements! {
    pub struct AttachmentEdgeStatements @ "attachment_referrer_edge" {
        insert = "INSERT INTO attachment_referrer_edges (attachment_id, referrer_kind, referrer_id)
             VALUES (?1, ?2, ?3) ON CONFLICT (attachment_id, referrer_kind, referrer_id) DO NOTHING";
        select_referrers = "SELECT referrer_kind, referrer_id FROM attachment_referrer_edges
             WHERE attachment_id = ?1 ORDER BY referrer_kind, referrer_id";
        delete_ref = "DELETE FROM attachment_referrer_edges WHERE attachment_id = ?1 AND referrer_kind = ?2 AND referrer_id = ?3";
        delete_referrer = "DELETE FROM attachment_referrer_edges WHERE referrer_kind = ?1 AND referrer_id = ?2";
        delete_unproven_ref = "DELETE FROM attachment_referrer_edges WHERE attachment_id = ?1 AND referrer_kind = ?2 AND referrer_id = ?3
             AND NOT EXISTS (SELECT 1 FROM attachment_uploads WHERE attachment_id = ?1)
             AND NOT EXISTS (SELECT 1 FROM attachment_pending_writes WHERE attachment_id = ?1 AND referrer_kind = ?2 AND referrer_id = ?3)";
        select_live_root = "SELECT 1 WHERE EXISTS (SELECT 1 FROM attachment_referrer_edges WHERE attachment_id = ?1)
             OR EXISTS (SELECT 1 FROM attachment_pending_writes WHERE attachment_id = ?1)";
        select_root_page = "SELECT DISTINCT attachment_id FROM attachment_referrer_edges WHERE referrer_kind = ?1 AND attachment_id > ?2 ORDER BY attachment_id LIMIT ?3";
        select_other_root_page = "SELECT DISTINCT attachment_id FROM attachment_referrer_edges WHERE referrer_kind NOT IN ('execution', 'process_record', 'session', 'upload') AND attachment_id > ?1 ORDER BY attachment_id LIMIT ?2";
    }
}
