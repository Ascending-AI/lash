//! Prompt snapshots: the neutral statements of the `prompts` domain
//! (FIG-5256, ADR 0133 §5).
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

/// The table of snapshot roots, one row per admitted model call.
pub const TABLE: &str = "prompt_snapshots";

/// The table of section texts, one row per content address.
pub const TEXTS_TABLE: &str = "prompt_texts";

/// The table of edges from a root to each text it references.
pub const EDGES_TABLE: &str = "prompt_snapshot_texts";

crate::statements! {
    /// `prompt_snapshots`, `prompt_texts` and `prompt_snapshot_texts`
    /// statements both backends issue verbatim.
    pub struct PromptStatements @ "durable_prompt" {
        /// Record call `?3` of turn `?2` in session `?1` as snapshot `?4`,
        /// written at epoch `?5`. No row when the call already has one.
        insert_snapshot = "INSERT INTO prompt_snapshots
                 (session_id, run, call_ordinal, snapshot, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (session_id, run, call_ordinal) DO NOTHING
             RETURNING call_ordinal";

        /// Store text `?2` at content address `?1`, keeping what is already
        /// there: the address names the bytes, so a conflict is the same text.
        insert_text = "INSERT INTO prompt_texts (hash, text) VALUES (?1, ?2)
             ON CONFLICT (hash) DO NOTHING";

        /// Root text `?4` under call `?3` of turn `?2` in session `?1`.
        insert_edge = "INSERT INTO prompt_snapshot_texts (session_id, run, call_ordinal, hash)
             VALUES (?1, ?2, ?3, ?4)";

        /// Call `?3` of turn `?2` in session `?1`: its snapshot and the epoch
        /// that wrote it.
        read_snapshot = "SELECT snapshot, written_epoch FROM prompt_snapshots
             WHERE session_id = ?1 AND run = ?2 AND call_ordinal = ?3";

        /// The texts call `?3` of turn `?2` in session `?1` references.
        read_edges = "SELECT hash FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND run = ?2 AND call_ordinal = ?3
             ORDER BY hash";

        /// The text at content address `?1`.
        read_text = "SELECT text FROM prompt_texts WHERE hash = ?1";

        /// Release every edge of session `?1`, returning each text it named.
        release_session_edges = "DELETE FROM prompt_snapshot_texts WHERE session_id = ?1
             RETURNING hash";

        /// Release every root of session `?1`.
        release_session_snapshots = "DELETE FROM prompt_snapshots WHERE session_id = ?1";

        /// Release every edge of turn `?2` in session `?1`, returning each
        /// text it named.
        release_run_edges = "DELETE FROM prompt_snapshot_texts
             WHERE session_id = ?1 AND run = ?2
             RETURNING hash";

        /// Release every root of turn `?2` in session `?1`.
        release_run_snapshots = "DELETE FROM prompt_snapshots
             WHERE session_id = ?1 AND run = ?2";

        /// Reclaim the text at `?1` when no root references it any more.
        reclaim_text = "DELETE FROM prompt_texts
             WHERE hash = ?1
               AND NOT EXISTS (SELECT 1 FROM prompt_snapshot_texts AS edge WHERE edge.hash = ?1)";
    }
}
