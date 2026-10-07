//! Admitted model calls: the neutral statements of the `prompts` domain
//! (FIG-5256, FIG-5259, ADR 0133 §5, §6).
//!
//! Owned by P2 (FIG-5256): its statements, and its tables' DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
//!
//! A snapshot row is an audit root; each root names the texts it references
//! through an edge row, and a text is stored once however many roots share
//! it. Nothing but a release removes a root, and a text is reclaimed only
//! when no edge names it.
//!
//! The batch statements over texts and edges fork: each dialect binds a
//! call's texts as one list, `json_each` on SQLite and `unnest` on
//! PostgreSQL, so its own module declares them.

/// The table of admitted-call roots, one row per admitted model call, keyed
/// by session, owner and call.
pub const TABLE: &str = "prompt_snapshots";

/// The table of section texts, one row per content address.
pub const TEXTS_TABLE: &str = "prompt_texts";

/// The table of edges from a root to each text it references.
pub const EDGES_TABLE: &str = "prompt_snapshot_texts";

crate::statements! {
    /// `prompt_snapshots`, `prompt_texts` and `prompt_snapshot_texts`
    /// statements both backends issue verbatim.
    pub struct PromptStatements @ "durable_prompt" {
        /// Record call `?3` of owner `?2` in session `?1` as `?4`, written
        /// at epoch `?5`. No row when the call already has one.
        insert_snapshot = "INSERT INTO prompt_snapshots
                 (session_id, owner, call, snapshot, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (session_id, owner, call) DO NOTHING
             RETURNING call";

        /// Call `?3` of owner `?2` in session `?1`: its record and the epoch
        /// that wrote it.
        read_snapshot = "SELECT snapshot, written_epoch FROM prompt_snapshots
             WHERE session_id = ?1 AND owner = ?2 AND call = ?3";

        /// The texts call `?3` of owner `?2` in session `?1` references.
        read_edges = "SELECT hash FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND owner = ?2 AND call = ?3
             ORDER BY hash";

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
