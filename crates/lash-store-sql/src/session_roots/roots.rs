//! `session_roots`: one row per `(session, root)` a drive admitted work
//! under, holding the root's terminal evidence once it has one. The row lives
//! until its session is deleted.

/// The table's unprefixed name.
pub const TABLE: &str = "session_roots";

/// The terminal evidence columns, in the order both backends decode them.
pub const TERMINAL_COLUMNS: &str =
    "terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms";

/// The key columns alone: opening a root writes its identity and nothing
/// else, so its terminal columns stay NULL until an end writes them.
pub const KEY_COLUMNS: &str = "session_id, root";

crate::statements! {
    /// `session_roots` statements both backends issue verbatim.
    pub struct SessionRootStatements @ "session_root" {
        /// Open root `?2` of session `?1` if it has no row yet.
        insert_open = "INSERT INTO session_roots (session_id, root) VALUES (?1, ?2)
             ON CONFLICT (session_id, root) DO NOTHING";

        /// The recorded result of root `?2`'s claim (NULL until its claim
        /// transaction commits), while the claim's head input `?3` is still
        /// undelivered. Once the head settles, is cancelled or is pruned,
        /// there is nothing left to replay and no row comes back.
        select_claim_result = "SELECT claim_result_json FROM session_roots
             WHERE session_id = ?1 AND root = ?2
               AND EXISTS (
                   SELECT 1 FROM pending_turn_inputs
                   WHERE session_id = ?1
                     AND input_id = ?3
                     AND {{nonterminal_turn_input_state(state)}}
               )";

        /// Record the claim after opening its root, in the claim transaction.
        write_claim_result = "UPDATE session_roots SET claim_result_json = ?3
             WHERE session_id = ?1 AND root = ?2 AND claim_result_json IS NULL";

        /// The terminal evidence of root `?2` of session `?1`: all four
        /// columns NULL while the root has none.
        select_terminal = "SELECT terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms
             FROM session_roots
             WHERE session_id = ?1 AND root = ?2";

        /// Write root `?2`'s terminal evidence (kind `?3`, cause `?4`, head
        /// revision `?5`, instant `?6`) unless it already has one: the
        /// caller decided the write against the stored evidence in the same
        /// transaction, and a zero row count means another writer won.
        write_terminal = "UPDATE session_roots
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6
             WHERE session_id = ?1 AND root = ?2 AND terminal_kind IS NULL";

        /// The roots of session `?1` without terminal evidence, in root
        /// order: what its close ends.
        select_open_roots = "SELECT root FROM session_roots
             WHERE session_id = ?1 AND terminal_kind IS NULL
             ORDER BY root";

        /// Every root of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_roots WHERE session_id = ?1";
    }
}

/// Terminal evidence plus its key, for the cross-session reconciliation cursor.
/// This is the complete terminal record; no input bindings or intent bodies are read.
pub const TERMINAL_PAGE_COLUMNS: &str =
    "session_id, root, terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms";

/// Rendered statements used by a parked-root control transaction.
pub struct RootVerbStatements {
    pub bound_inputs: crate::Rendered,
    pub rebind: crate::Rendered,
    pub unbind: crate::Rendered,
    pub set_kind: crate::Rendered,
    pub raise_epoch: crate::Rendered,
    pub input: crate::Rendered,
    pub release_inputs: crate::Rendered,
    pub release_batches: crate::Rendered,
    pub delete_batch_items: crate::Rendered,
    pub delete_batch: crate::Rendered,
    pub terminals: crate::Rendered,
    pub sessions: crate::Rendered,
    pub intents: crate::Rendered,
}

impl RootVerbStatements {
    /// Render each statement through its table owner.
    #[must_use]
    pub fn render(dialect: crate::Dialect) -> Self {
        let group0 = crate::session_roots::root_inputs::RootInputVerbStatements::render(dialect);
        let group1 = crate::session_roots::control_intents::ControlVerbStatements::render(dialect);
        let group2 = crate::session::meta::MetaRootVerbStatements::render(dialect);
        let group3 =
            crate::turn_ingress::pending_inputs::PendingRootVerbStatements::render(dialect);
        let group4 = crate::turn_ingress::queued_batches::BatchRootVerbStatements::render(dialect);
        let group5 = crate::turn_ingress::queued_items::ItemRootVerbStatements::render(dialect);
        let group6 = crate::session_roots::roots::TerminalPageStatements::render(dialect);
        Self {
            bound_inputs: group0.bound_inputs,
            rebind: group0.rebind,
            unbind: group0.unbind,
            set_kind: group1.set_kind,
            intents: group1.intents,
            raise_epoch: group2.raise_epoch,
            sessions: group2.sessions,
            input: group3.input,
            release_inputs: group3.release_inputs,
            release_batches: group4.release_batches,
            delete_batch: group4.delete_batch,
            delete_batch_items: group5.delete_batch_items,
            terminals: group6.terminals,
        }
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct TerminalPageStatements @ "session_root" {
        terminals = "SELECT session_id, root, terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms
            FROM session_roots WHERE terminal_kind IS NOT NULL
              AND (session_id > ?1 OR (session_id = ?1 AND root > ?2))
            ORDER BY session_id, root LIMIT ?3";
    }
}
