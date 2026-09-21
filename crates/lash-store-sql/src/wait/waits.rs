//! `await_event_waits`: one row per durable AwaitEvent promise.

/// The table's unprefixed name.
pub const TABLE: &str = "await_event_waits";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms";

/// The promise as an identity comparison reads it: everything that decides
/// whether a stored row *is* the promise a caller named, plus its terminal.
///
/// The timestamps are deliberately absent. They are records, never inputs: no
/// decision on this table reads `created_at_ms` or `resolved_at_ms`, and a
/// projection that carried them would invite one to.
pub const ROW_COLUMNS: &str = "scope_json, wait_json, session_id, turn_control, terminal_json";

/// What a session's pending promises report to the coordinator rebuilding
/// them. The second projection over this table, and the one the named risk in
/// FIG-3380 anticipates: it keys by `key_id`, which [`ROW_COLUMNS`] does not
/// carry because that read is already keyed by it.
pub const REGISTERED_COLUMNS: &str = "key_id, scope_json, wait_json, turn_control";

/// One `await_event_waits` row, decoded.
///
/// Both backends decoded a byte-identical private copy of this struct and its
/// identity comparison before FIG-3380. The comparison is the whole reason the
/// row is read at all — a stored row under the same key that names a different
/// scope, wait, session or control kind is somebody else's promise — so it
/// belongs to the table, not to a driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitRow {
    /// Canonical JSON of the promise's execution scope.
    pub scope_json: String,
    /// Canonical JSON of the promise's wait identity.
    pub wait_json: String,
    /// Owning session, when the scope has one.
    pub session_id: Option<String>,
    pub turn_control: bool,
    /// The resolved terminal, when the promise has settled.
    pub terminal_json: Option<String>,
}

impl WaitRow {
    /// Whether this stored row is the promise the caller named.
    ///
    /// Compares every identity column and no timestamp: the row under a key
    /// either is that promise or belongs to a revoked predecessor, and only
    /// the identity columns can tell the two apart.
    #[must_use]
    pub fn matches_identity(
        &self,
        scope_json: &str,
        wait_json: &str,
        session_id: Option<&str>,
        turn_control: bool,
    ) -> bool {
        self.scope_json == scope_json
            && self.wait_json == wait_json
            && self.session_id.as_deref() == session_id
            && self.turn_control == turn_control
    }
}

crate::statements! {
    /// `await_event_waits` statements both backends issue verbatim.
    pub struct WaitStatements @ "await_event_wait" {
        /// The row stored under key `?1`.
        select_by_key = "SELECT scope_json, wait_json, session_id, turn_control, terminal_json
             FROM await_event_waits
             WHERE key_id = ?1";

        /// Session `?1`'s unresolved promises, empty once the session's
        /// revocation tombstone exists.
        list_pending_for_session = "SELECT key_id, scope_json, wait_json, turn_control
             FROM await_event_waits
             WHERE session_id = ?1
               AND terminal_json IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM await_event_revoked_sessions
                   WHERE session_id = ?1
               )
             ORDER BY key_id";

        delete_by_session = "DELETE FROM await_event_waits WHERE session_id = ?1";

        /// Delete every promise under the scope whose canonical JSON is `?1`.
        delete_by_scope_json = "DELETE FROM await_event_waits WHERE scope_json = ?1";

        /// Every session-free scope that owns a promise: the wait half of the
        /// deferred-retirement candidate set.
        select_session_free_scope_json = "SELECT DISTINCT scope_json FROM await_event_waits WHERE session_id IS NULL";

        /// How many rows key `?1` has. Read by the cold-process conformance
        /// helpers, which observe the durable row from outside the runtime.
        count_by_key = "SELECT COUNT(*) FROM await_event_waits WHERE key_id = ?1";
    }
}
