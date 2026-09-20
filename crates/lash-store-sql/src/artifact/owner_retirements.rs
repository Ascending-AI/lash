//! `artifact_owner_retirements`: the permanent publication fence.
//!
//! One row per retired execution owner, and only execution owners ever enter
//! it — `ck_artifact_owner_retirements_owner_kind` refuses anything else. Host
//! and process releases are ordinary exact-edge severance and leave no trace
//! here. The fence is permanent by design (FIG-677, ADR 0093): a retired
//! owner's later publish is refused rather than silently re-staged.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_owner_retirements";

/// Every column, in insert order. The whole row is its primary key.
pub const INSERT_COLUMNS: &str = "owner_kind, owner_id";

crate::statements! {
    /// `artifact_owner_retirements` statements both backends issue verbatim.
    pub struct OwnerRetirementStatements @ "artifact_owner_retirement" {
        /// Whether owner `?1`/`?2` has been permanently retired. Read before
        /// every publish, retain and transfer destination.
        select_is_retired = "SELECT EXISTS (
                 SELECT 1 FROM artifact_owner_retirements
                 WHERE owner_kind = ?1 AND owner_id = ?2
             )";

        /// Fence owner `?1`/`?2` permanently. Retiring twice is the same fact
        /// as retiring once.
        insert_retirement = "INSERT INTO artifact_owner_retirements (owner_kind, owner_id)
             VALUES (?1, ?2) ON CONFLICT DO NOTHING";
    }
}
