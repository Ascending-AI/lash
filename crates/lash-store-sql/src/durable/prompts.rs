//! Admitted model calls: the neutral statements of the `prompts` domain
//! (FIG-5256, FIG-5259, ADR 0133 §5, §6).
//!
//! Owned by P2 (FIG-5256): its statements, and its tables' DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
//!
//! A call retains independent snapshot and request roots in the shared
//! graph. Settlement releases request roots; audit retention releases
//! snapshot roots. The session separately retains its reusable prefix.
//! Graph installation, immutable child edges and collection belong to the
//! graph domain. Each backend binds root lists using its native mechanism.

/// The table of admitted-call roots, one row per admitted model call, keyed
/// by session, owner and call.
pub const TABLE: &str = "prompt_snapshots";

/// The table of edges from a root's part to each text it references.
pub const EDGES_TABLE: &str = "prompt_snapshot_texts";
/// The one reusable lowered history root owned by each session.
pub const PREFIXES_TABLE: &str = "prompt_prefixes";

/// The edge part of a text a call's request references.
pub const REQUEST_PART: &str = "request";

crate::statements! {
    /// Prompt call identities and graph ownership statements shared by both backends.
    pub struct PromptStatements @ "durable_prompt" {
        /// Record call `?3` of owner `?2` in session `?1` with snapshot `?4`
        /// and request `?5`, written at epoch `?6`. No row when the call
        /// already has one.
        insert_snapshot = "INSERT INTO prompt_snapshots
                 (session_id, owner, call, snapshot, request, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (session_id, owner, call) DO NOTHING
             RETURNING call";

        /// Call `?3` of owner `?2` in session `?1`: its snapshot, its
        /// request and the epoch that wrote it.
        read_snapshot = "SELECT snapshot, request, written_epoch,
                 request IS NULL AND (snapshot IS NULL OR EXISTS (
                     SELECT 1 FROM session_runs AS ended
                     WHERE ended.session_id = audit.session_id
                       AND 'turn:' || ended.run = audit.owner
                       AND ended.terminal_kind IS NOT NULL
                       AND ended.terminal_window_revision IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM session_revisions AS revision
                           WHERE revision.session_id = ended.session_id
                             AND revision.head_revision = ended.terminal_window_revision
                       )
                 )) AS expired
             FROM prompt_snapshots AS audit
             WHERE session_id = ?1 AND owner = ?2 AND call = ?3";

        /// One bounded page of settled audit roots whose revision was released.
        /// Pending requests retain both parts regardless of revision membership.
        expired_calls = "SELECT audit.session_id, audit.owner, audit.call
             FROM prompt_snapshots AS audit
             JOIN session_runs AS ended ON ended.session_id = audit.session_id
               AND audit.owner = 'turn:' || ended.run
             WHERE (?1 IS NULL OR audit.session_id = ?1)
               AND audit.request IS NULL AND audit.snapshot IS NOT NULL
               AND ended.terminal_kind IS NOT NULL
               AND ended.terminal_window_revision IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM session_revisions AS revision
                   WHERE revision.session_id = ended.session_id
                     AND revision.head_revision = ended.terminal_window_revision
               )
             ORDER BY audit.session_id, audit.owner, audit.call LIMIT ?2";

        /// Sever only a settled call's audit edges; the compact identity stays.
        release_audit_edges = "DELETE FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND owner = ?2 AND call = ?3 AND part = 'snapshot'
             RETURNING hash";

        /// Expire an audit without forgetting that the call was already admitted.
        expire_audit = "UPDATE prompt_snapshots SET snapshot = NULL
             WHERE session_id = ?1 AND owner = ?2 AND call = ?3 AND request IS NULL";

        /// Sever the session's reusable prefix, returning its former root.
        release_prefix = "DELETE FROM prompt_prefixes WHERE session_id = ?1 RETURNING hash";

        /// Replace the one reusable prefix of a session.
        set_prefix = "INSERT INTO prompt_prefixes (session_id, hash) VALUES (?1, ?2)
             ON CONFLICT (session_id) DO UPDATE SET hash = excluded.hash";

        /// Release every edge of session `?1`, returning each text it named.
        release_session_edges = "DELETE FROM prompt_snapshot_texts WHERE session_id = ?1
             RETURNING hash";

        /// Release every root of session `?1`.
        release_session_snapshots = "DELETE FROM prompt_snapshots WHERE session_id = ?1";

        /// Release every edge of owner `?2` in session `?1`, returning each
        /// text it named.
        release_run_edges = "DELETE FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND owner = ?2
             RETURNING hash";

        /// Release every root of owner `?2` in session `?1`.
        release_run_snapshots = "DELETE FROM prompt_snapshots
             WHERE session_id = ?1 AND owner = ?2";
    }
}
