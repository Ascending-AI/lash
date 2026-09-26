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

crate::statements! {
    /// Atomic parked-root control writes, shared by both stores.
    pub struct RootVerbStatements @ "root_verb" {
        bound_inputs = "SELECT b.input_id FROM session_root_inputs b JOIN pending_turn_inputs i ON i.session_id = b.session_id AND i.input_id = b.input_id WHERE b.session_id = ?1 AND b.root = ?2 AND {{nonterminal_turn_input_state(i.state)}}";
        rebind = "UPDATE session_root_inputs SET root = ?3 WHERE session_id = ?1 AND input_id = ?2";
        unbind = "DELETE FROM session_root_inputs WHERE session_id = ?1 AND input_id = ?2";
        set_kind = "UPDATE control_intents SET kind_json = ?2 WHERE intent_id = ?1";
        raise_epoch = "UPDATE session_meta SET drive_epoch = drive_epoch + 1, drive_admission_id = ?2, drive_root_start = NULL WHERE session_id = ?1 AND closing_intent IS NULL";
        input = "UPDATE pending_turn_inputs SET state = ?3,
            claim_id = NULL, claim_owner_id = NULL, claim_owner_incarnation_id = NULL,
            claim_token = NULL, claim_session_lease_generation = 0,
            claim_bound_turn_id = NULL, claim_bound_receipt_input_id = NULL
            WHERE session_id = ?1 AND input_id = ?2 AND {{nonterminal_turn_input_state(state)}}";
        release_inputs = "UPDATE pending_turn_inputs SET
            claim_id = NULL, claim_owner_id = NULL, claim_owner_incarnation_id = NULL,
            claim_token = NULL, claim_session_lease_generation = 0,
            claim_bound_turn_id = NULL, claim_bound_receipt_input_id = NULL
            WHERE session_id = ?1 AND claim_token IS NOT NULL";
        release_batches = "UPDATE queued_work_batches SET
            claim_id = NULL,
            claim_token = NULL, claim_session_lease_generation = 0
            WHERE session_id = ?1 AND claim_token IS NOT NULL";
        delete_batch_items = "DELETE FROM queued_work_items WHERE batch_id = ?1";
        delete_batch = "DELETE FROM queued_work_batches WHERE session_id = ?1 AND batch_id = ?2";
        terminals = "SELECT session_id, root, terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms
            FROM session_roots WHERE terminal_kind IS NOT NULL
              AND (session_id > ?1 OR (session_id = ?1 AND root > ?2))
            ORDER BY session_id, root LIMIT ?3";
        sessions = "SELECT session_id FROM session_meta WHERE session_id > ?1 AND closing_intent IS NULL ORDER BY session_id LIMIT ?2";
        intents = "SELECT intent_id, session_id, format, kind_json, state_json, attempts, created_at_ms, engine_ref
            FROM control_intents WHERE intent_id > ?1 ORDER BY intent_id LIMIT ?2";
    }
}
