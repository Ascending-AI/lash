//! Admitted model calls: the neutral statements of the `prompts` domain
//! (FIG-5256, FIG-5259, ADR 0133 §5, §6).
//!
//! Owned by P2 (FIG-5256): its statements, and its tables' DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
//!
//! A root row holds a call's snapshot (its audit part) and its request (the
//! part a send reads); each names the texts it references through an edge
//! row tagged with that part, and a text is stored once however many edges
//! share it. Settling a call releases its request part (FIG-5946); only a
//! release removes a root, and a text is reclaimed only when no edge names
//! it.
//!
//! The batch statements over texts and edges fork: each dialect binds a
//! call's texts as one list, `json_each` on SQLite and `unnest` on
//! PostgreSQL, so its own module declares them. So does a settlement:
//! PostgreSQL releases a request in one statement, SQLite in three.

/// The table of admitted-call roots, one row per admitted model call, keyed
/// by session, owner and call.
pub const TABLE: &str = "prompt_snapshots";

/// The table of section texts and body chunks, one row per content address.
pub const TEXTS_TABLE: &str = "prompt_texts";

/// The table of edges from a root's part to each text it references.
pub const EDGES_TABLE: &str = "prompt_snapshot_texts";
/// Immutable fragment-to-fragment edges.
pub const INDEX_EDGES_TABLE: &str = "prompt_text_edges";
/// The one reusable lowered history root owned by each session.
pub const RETIREMENT_TABLE: &str = "prompt_retirement_frontier";
pub const PREFIXES_TABLE: &str = "prompt_prefixes";

/// The edge part of a text a call's snapshot references.
pub const SNAPSHOT_PART: &str = "snapshot";

/// The edge part of a text a call's request references.
pub const REQUEST_PART: &str = "request";

crate::statements! {
    /// `prompt_snapshots`, `prompt_texts` and `prompt_snapshot_texts`
    /// statements both backends issue verbatim.
    pub struct PromptStatements @ "durable_prompt" {
        /// Durable prompt cleanup still queued.
        retirement_count = "SELECT count(*) FROM prompt_retirement_frontier";
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

        /// The texts call `?3` of owner `?2` in session `?1` references,
        /// each with the part that references it.
        read_edges = "WITH RECURSIVE closure AS (
                 SELECT part, hash FROM prompt_snapshot_texts
                  WHERE session_id = ?1 AND owner = ?2 AND call = ?3
                 UNION
                 SELECT closure.part, edge.child FROM closure
                   JOIN prompt_text_edges AS edge ON edge.parent = closure.hash
             ) SELECT part, hash FROM closure ORDER BY part, hash";

        /// Outgoing immutable index edges.
        read_children = "SELECT child FROM prompt_text_edges WHERE parent = ?1 ORDER BY child";

        /// Sever the session's reusable prefix, returning its former root.
        release_prefix = "DELETE FROM prompt_prefixes WHERE session_id = ?1 RETURNING hash";

        /// Replace the one reusable prefix of a session.
        set_prefix = "INSERT INTO prompt_prefixes (session_id, hash) VALUES (?1, ?2)
             ON CONFLICT (session_id) DO UPDATE SET hash = excluded.hash";

        /// The text at content address `?1`.
        read_text = "SELECT text FROM prompt_texts WHERE hash = ?1";

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
