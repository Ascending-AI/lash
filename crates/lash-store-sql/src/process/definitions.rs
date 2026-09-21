//! `process_definitions`: the named process-definition registry (ADR 0095).
//!
//! Written only by the `RegisterProcessDefinition` intent, under a
//! revision-and-fingerprint compare-and-swap. Both backends issue every
//! statement over this table verbatim, so it has no dialect-only set at all.

/// The table's unprefixed name.
pub const TABLE: &str = "process_definitions";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "definition_id, owner_scope, name, revision, fingerprint,
              lifecycle, deleted_at_ms, change_seq, created_at_ms,
              updated_at_ms, record_json";

/// What the registration compare-and-swap reads.
///
/// Narrow on purpose: the CAS compares the revision it expected and the
/// fingerprint it pinned, and returns the stored definition only so a
/// no-op re-registration can answer from it. Nothing on this path reads the
/// lifecycle columns, which the deletion frontier owns.
pub const CAS_COLUMNS: &str = "revision, fingerprint, record_json";

crate::statements! {
    /// `process_definitions` statements both backends issue verbatim.
    pub struct DefinitionStatements @ "process_definition" {
        /// The definition registered under owner scope `?1` and name `?2`, as
        /// its compare-and-swap reads it.
        select_for_cas = "SELECT revision, fingerprint, record_json FROM process_definitions
             WHERE owner_scope = ?1 AND name = ?2";

        /// Revision and change sequence start at one, and a fresh row is enabled and
        /// untombstoned.
        insert_first_revision = "INSERT INTO process_definitions
             (definition_id, owner_scope, name, revision, fingerprint,
              lifecycle, deleted_at_ms, change_seq, created_at_ms,
              updated_at_ms, record_json)
             VALUES (?1, ?2, ?3, 1, ?4, 'enabled', NULL, 1, ?5, ?5, ?6)";

        /// Advance owner scope `?1` / name `?2` to revision `?3` with
        /// fingerprint `?4` at `?5`, carrying record `?6`. Re-registering a
        /// tombstoned name revives it, which is why the lifecycle columns are
        /// written here rather than left alone.
        update_revision = "UPDATE process_definitions SET revision = ?3, fingerprint = ?4,
                 lifecycle = 'enabled', deleted_at_ms = NULL,
                 change_seq = change_seq + 1, updated_at_ms = ?5, record_json = ?6
             WHERE owner_scope = ?1 AND name = ?2";

        /// Every definition owner scope `?1` has registered.
        list_by_owner_scope = "SELECT record_json FROM process_definitions
             WHERE owner_scope = ?1 ORDER BY name ASC";
    }
}
