//! Evidence that bytes were successfully written, invalidated only by condemnation.
pub const TABLE: &str = "attachment_uploads";
crate::statements! {
    pub struct UploadStatements @ "attachment_upload" {
        insert = "INSERT INTO attachment_uploads (attachment_id, written_at_ms) VALUES (?1, ?2)
             ON CONFLICT (attachment_id) DO UPDATE SET written_at_ms = CASE WHEN excluded.written_at_ms < attachment_uploads.written_at_ms THEN excluded.written_at_ms ELSE attachment_uploads.written_at_ms END";
        select_evidence = "SELECT 1 FROM attachment_uploads WHERE attachment_id = ?1";
        delete_by_id = "DELETE FROM attachment_uploads WHERE attachment_id = ?1";
    }
}
