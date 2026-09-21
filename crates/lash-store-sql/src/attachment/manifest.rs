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

        /// Whether digest `?1` still has a live root, with `?2` the
        /// intent-grace cutoff.
        ///
        /// The row is live unless it is eligible for exactly the forget
        /// [`ManifestStatements::delete_aged_uncommitted`] performs, which is
        /// why the two carry the same predicate: the delete-time probe and
        /// the reconciliation sweep cannot be allowed to disagree about what
        /// a root is. Age alone only retires an unscoped host put; a scoped
        /// intent needs its owner proven gone.
        ///
        /// This is the shape a deployment with no process registry issues.
        /// The one that can prove a process owner dead is
        /// [`ManifestProcessOwnerStatements::select_live_root_proving_process_death`], a separate
        /// statement rather than an optional predicate: neither
        /// `COALESCE(?N, column)` nor `?N IS NULL OR …` is sargable, and the
        /// two shapes are two production shapes.
        select_live_root = "SELECT 1 FROM attachment_manifest AS manifest
             WHERE manifest.attachment_id = ?1
               AND NOT (
                    manifest.committed_at_ms IS NULL
                    AND manifest.intent_at_ms <= ?2
                    AND (
                        manifest.owner_kind IS NULL
                        OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                                   WHERE deleted.session_id = manifest.session_id)
                        OR (
                            {{turn_attachment_owner(manifest.owner_kind)}}
                            AND EXISTS (
                                SELECT 1 FROM runtime_turn_commits AS turn_commit
                                WHERE turn_commit.session_id = manifest.session_id
                                  AND turn_commit.turn_id <> manifest.owner_id
                                  AND turn_commit.committed_at_ms > manifest.intent_at_ms
                            )
                        )
                    )
               )
             LIMIT 1";

        /// Forget every uncommitted intent older than `?1` whose owner is
        /// provably gone. The negation of
        /// [`ManifestStatements::select_live_root`], over every digest at
        /// once.
        delete_aged_uncommitted = "DELETE FROM attachment_manifest AS manifest
             WHERE manifest.committed_at_ms IS NULL
               AND manifest.intent_at_ms <= ?1
               AND (
                    manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                    OR (
                        {{turn_attachment_owner(manifest.owner_kind)}}
                        AND EXISTS (
                            SELECT 1 FROM runtime_turn_commits AS turn_commit
                            WHERE turn_commit.session_id = manifest.session_id
                              AND turn_commit.turn_id <> manifest.owner_id
                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                        )
                    )
               )";
    }
}

crate::statements! {
    /// The same two operations, for a deployment that can prove a process
    /// owner dead.
    ///
    /// A process-owned intent outlives its session's turn, so age cannot
    /// retire it; what retires it is the absence of the owning incarnation
    /// from the process registry. Reading that in the *same* statement is the
    /// point — a read-process-then-forget pair would race a registration
    /// across the per-session topology — and on SQLite the registry is a
    /// different database, reached through an `ATTACH`ed name. That is the
    /// join a per-statement schema qualifier could not spell and a per-table
    /// layout can (FIG-3406).
    ///
    /// These render only for a layout that places `processes`. A connection
    /// with no registry bound has no such layout, so it cannot issue them at
    /// all and conservatively retains process-owned rows rather than guessing
    /// liveness — the behaviour the `format!`ed pair had, now enforced by the
    /// renderer instead of by an `if`.
    pub struct ManifestProcessOwnerStatements @ "attachment_manifest" {
        /// [`ManifestStatements::select_live_root`] plus the owner-death
        /// proof.
        select_live_root_proving_process_death = "SELECT 1 FROM attachment_manifest AS manifest
             WHERE manifest.attachment_id = ?1
               AND NOT (
                    manifest.committed_at_ms IS NULL
                    AND manifest.intent_at_ms <= ?2
                    AND (
                        manifest.owner_kind IS NULL
                        OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                                   WHERE deleted.session_id = manifest.session_id)
                        OR (
                            {{turn_attachment_owner(manifest.owner_kind)}}
                            AND EXISTS (
                                SELECT 1 FROM runtime_turn_commits AS turn_commit
                                WHERE turn_commit.session_id = manifest.session_id
                                  AND turn_commit.turn_id <> manifest.owner_id
                                  AND turn_commit.committed_at_ms > manifest.intent_at_ms
                            )
                        )
                        OR (
                            {{process_attachment_owner(manifest.owner_kind)}}
                            AND NOT EXISTS (
                                SELECT 1 FROM processes AS process
                                WHERE process.process_id = manifest.owner_id
                                  AND process.incarnation = manifest.owner_incarnation
                            )
                        )
                    )
               )
             LIMIT 1";

        /// [`ManifestStatements::delete_aged_uncommitted`] plus the
        /// owner-death proof.
        delete_aged_uncommitted_proving_process_death = "DELETE FROM attachment_manifest AS manifest
             WHERE manifest.committed_at_ms IS NULL
               AND manifest.intent_at_ms <= ?1
               AND (
                    manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                    OR (
                        {{turn_attachment_owner(manifest.owner_kind)}}
                        AND EXISTS (
                            SELECT 1 FROM runtime_turn_commits AS turn_commit
                            WHERE turn_commit.session_id = manifest.session_id
                              AND turn_commit.turn_id <> manifest.owner_id
                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                        )
                    )
                    OR (
                        {{process_attachment_owner(manifest.owner_kind)}}
                        AND NOT EXISTS (
                            SELECT 1 FROM processes AS process
                            WHERE process.process_id = manifest.owner_id
                              AND process.incarnation = manifest.owner_incarnation
                        )
                    )
               )";
    }
}
