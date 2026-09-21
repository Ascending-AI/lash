//! `turn_cancel_closure_authorizations`: the pinned closure obligation of one
//! cancelled turn.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancel_closure_authorizations";

/// Every column an insert writes.
pub const INSERT_COLUMNS: &str = "session_id, turn_id, authorization_json";

/// The authorization keyed by its session, for the catalog-wide sweep that
/// asks which sessions still pin a scope.
///
/// `turn_id` is absent because the sweep decides per session and orders by the
/// turn without reading it.
pub const SESSION_KEYED_COLUMNS: &str = "session_id, authorization_json";

crate::statements! {
    /// `turn_cancel_closure_authorizations` statements both backends issue
    /// verbatim.
    pub struct ClosureAuthorizationStatements @ "turn_cancel_closure_authorization" {
        /// Pin turn `?2` of session `?1` to the closure authorization `?3`.
        insert_new = "INSERT INTO turn_cancel_closure_authorizations (
                 session_id, turn_id, authorization_json
             )
             VALUES (?1, ?2, ?3)";

        /// Session `?1`'s pinned closures, in turn order.
        list_by_session = "SELECT authorization_json
             FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1
             ORDER BY turn_id";

        /// Every pinned closure in the catalog, grouped by session.
        ///
        /// Retiring a cancellation scope has to prove no session anywhere still
        /// pins it, and the pin is inside the stored authorization rather than
        /// in a column, so the sweep reads them all in one ordered pass.
        list_all = "SELECT session_id, authorization_json
             FROM turn_cancel_closure_authorizations
             ORDER BY session_id, turn_id";

        /// How many closures session `?1` still pins: the session-deletion
        /// gate's count.
        count_by_session = "SELECT COUNT(*) FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1";

        /// Release turn `?2` of session `?1`'s pin.
        delete_by_turn = "DELETE FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1 AND turn_id = ?2";

        /// Release turn `?2` of session `?1`'s pin only if it is still the
        /// exact authorization `?3` the caller settled.
        ///
        /// The whole document is the predicate because it is the identity: a
        /// pin replaced by a different authorization between the settle and the
        /// release is somebody else's obligation.
        delete_settled = "DELETE FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1 AND turn_id = ?2 AND authorization_json = ?3";

        delete_by_session = "DELETE FROM turn_cancel_closure_authorizations
             WHERE session_id = ?1";
    }
}
