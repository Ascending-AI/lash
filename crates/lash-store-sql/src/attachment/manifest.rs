//! `attachment_manifest`: one row per (session, digest) attachment intent.
//!
//! The row is the write-ahead record of an attachment: who intended it, which
//! write attempt currently owns it, whether the bytes were proven uploaded
//! (`written_at_ms`), and whether a turn or process committed it
//! (`committed_at_ms`). The table is also the attachment GC root set: a digest
//! with no row here is unrooted.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_manifest";

/// Every column, in insert order.
///
/// `write_id` is the identity of the write attempt that owns the row, and the
/// owner triple is the durable edge that keeps the row alive past its
/// session's turn.
pub const INSERT_COLUMNS: &str = "attachment_id, session_id, canonical_uri, intent_at_ms, write_id,
     written_at_ms, committed_at_ms, owner_kind, owner_id, owner_incarnation";

/// What adoption writes: a row that is committed the moment it exists.
///
/// Narrow on purpose, and not for speed. Adoption owns no write attempt, so
/// `write_id` must stay NULL, and it carries no owner edge, so the owner
/// triple must stay NULL — `ck_attachment_manifest_owner_identity` refuses any
/// partially filled owner. Naming those four columns in this insert would
/// invite a later edit to bind one of them.
pub const ADOPTION_COLUMNS: &str =
    "attachment_id, session_id, canonical_uri, intent_at_ms, written_at_ms, committed_at_ms";

/// The manifest row as the retention sweep reports it.
///
/// Every column except `write_id`. The write identity is the fence between one
/// writer and its own superseded attempts; it is meaningless outside the
/// transaction that minted it, and no caller of the sweep has ever read it.
pub const ENTRY_COLUMNS: &str = "attachment_id, session_id, canonical_uri, intent_at_ms,
     committed_at_ms, owner_kind, owner_id, owner_incarnation, written_at_ms";

crate::statements! {
    /// `attachment_manifest` statements both backends issue verbatim.
    pub struct ManifestStatements @ "attachment_manifest" {
        /// Record this attempt's intent for `?1` (digest) in `?2` (session),
        /// `?3` canonical URI, `?4` intent instant, `?5`/`?6`/`?7` the owner
        /// triple and `?8` this attempt's write identity.
        ///
        /// A fresh attempt has proven nothing, so it takes the row with no
        /// upload stamp and no commit stamp; evidence and commitment already
        /// earned by earlier attempts survive the upsert untouched.
        insert_intent = "INSERT INTO attachment_manifest (
                 attachment_id, session_id, canonical_uri, intent_at_ms, write_id,
                 written_at_ms, committed_at_ms, owner_kind, owner_id, owner_incarnation
             )
             VALUES (?1, ?2, ?3, ?4, ?8, NULL, NULL, ?5, ?6, ?7)
             ON CONFLICT (session_id, attachment_id) DO UPDATE SET
                 canonical_uri = excluded.canonical_uri,
                 intent_at_ms = excluded.intent_at_ms,
                 write_id = excluded.write_id,
                 owner_kind = excluded.owner_kind,
                 owner_id = excluded.owner_id,
                 owner_incarnation = excluded.owner_incarnation";

        /// Adopt digest `?2` into session `?3` at `?1` with canonical URI `?4`
        /// and the uploader's proven instant `?5`.
        ///
        /// The `COALESCE` pair is what makes adoption idempotent: an existing
        /// row keeps the first proven upload and the first commitment.
        ///
        /// The stored row is named by its table, not left bare. Inside
        /// `DO UPDATE` both the target row and `excluded` are in scope, and
        /// PostgreSQL refuses the bare column as ambiguous — `column
        /// reference "committed_at_ms" is ambiguous` — where SQLite resolves
        /// it to the target. The renderer supplies each backend's spelling of
        /// the table, so one qualified text is right on both.
        upsert_adopted = "INSERT INTO attachment_manifest
             (attachment_id, session_id, canonical_uri, intent_at_ms, written_at_ms, committed_at_ms)
             VALUES (?2, ?3, ?4, ?1, ?5, ?1)
             ON CONFLICT (session_id, attachment_id) DO UPDATE
             SET committed_at_ms =
                     COALESCE(attachment_manifest.committed_at_ms, excluded.committed_at_ms),
                 written_at_ms =
                     COALESCE(attachment_manifest.written_at_ms, excluded.written_at_ms)";

        /// Stamp `?4` as the upload instant of the row attempt `?3` owns for
        /// `?1` in `?2`. Id-matched, so a superseded permit stamps nothing,
        /// and `COALESCE` keeps the first proven upload.
        stamp_written = "UPDATE attachment_manifest
             SET written_at_ms = COALESCE(written_at_ms, ?4)
             WHERE attachment_id = ?1 AND session_id = ?2 AND write_id = ?3";

        /// Commit every uncommitted row of session `?2` owned by `?4`/`?3` at
        /// `?1`, keeping an earlier commitment.
        commit_owned = "UPDATE attachment_manifest
             SET committed_at_ms = COALESCE(committed_at_ms, ?1)
             WHERE session_id = ?2
               AND owner_kind = ?4
               AND owner_id = ?3
               AND committed_at_ms IS NULL";

        /// Every row for digest `?1`, in every session.
        ///
        /// Issued once a condemnation has proven the digest unrooted: what is
        /// left is stale evidence of an upload whose bytes this sweep is about
        /// to delete, and clearing it is what makes a byte-absence tombstone
        /// unnecessary.
        delete_by_id = "DELETE FROM attachment_manifest WHERE attachment_id = ?1";

        /// The unproven row for `?1` in session `?2`: neither uploaded nor
        /// committed, so nothing is lost by forgetting it.
        delete_unproven_for_session = "DELETE FROM attachment_manifest
             WHERE attachment_id = ?1 AND session_id = ?2
               AND written_at_ms IS NULL AND committed_at_ms IS NULL";

        /// The unproven row attempt `?3` still owns. A superseded permit
        /// matches nothing and deletes nothing.
        delete_unproven_for_write = "DELETE FROM attachment_manifest
             WHERE attachment_id = ?1 AND session_id = ?2 AND write_id = ?3
               AND written_at_ms IS NULL AND committed_at_ms IS NULL";

        /// The earliest proven upload instant for digest `?1`, from any
        /// session: the uploader and the adopter need not be the same, and the
        /// earliest proof is the one that is copied onto the adopter's row.
        select_earliest_written_at = "SELECT MIN(written_at_ms) FROM attachment_manifest
             WHERE attachment_id = ?1 AND written_at_ms IS NOT NULL";

        /// Every digest the manifest still roots.
        select_rooted_ids = "SELECT DISTINCT attachment_id FROM attachment_manifest";
    }
}
